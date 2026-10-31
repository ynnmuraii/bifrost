//! No core, audio endpoint or native input. Optional PNGs use BIFROST_SETTINGS_SHOTS.
mod support;

use bifrost_gui::audio::AudioAction;
use bifrost_gui::egui::{self, Shape};
use bifrost_gui::gui_settings::Theme;
use bifrost_gui::ui::{self, audio_settings, Palette};
use bifrost_gui::view_model::{Action, Modal, ViewModel};
use protocol::ipc;
use ui_text::{text, Key, Lang};

fn fixture(lang: Lang, theme: Theme, scene: &str) -> ViewModel {
    let mut vm = support::view();
    vm.look.language = lang;
    vm.look.theme = theme;
    vm.status = Some(support::status_with("LOCAL"));
    vm.status.as_mut().unwrap().members[1].display_name = "LAPTOP".into();
    vm.connection = bifrost_gui::view_model::Connection::Connected;
    if scene == "unavailable" {
        return vm;
    }
    vm.audio.available = true;
    vm.audio.draft.group_id = vec![0x11; 16];
    vm.audio.draft.listener_node_id = [0xbb, 0x22].repeat(16);
    vm.audio.draft.peers = vec![ipc::AudioPeerConfig {
        node_id: vm.audio.draft.listener_node_id.clone(),
        allow_system_send: true,
        allow_system_receive: false,
        volume_percent: 100,
        ..Default::default()
    }];
    if scene == "listener" {
        vm.audio.draft.role = ipc::AudioRole::Listener as i32;
        vm.audio.draft.listener_node_id.clear();
        for peer in &mut vm.audio.draft.peers {
            peer.allow_system_send = false;
            peer.allow_system_receive = true;
        }
    }
    vm.audio.status = Some(ipc::AudioStatus {
        revision: 8,
        enabled: false,
        role: vm.audio.draft.role,
        mode: ipc::AudioMode::MixAll as i32,
        config: Some(vm.audio.draft.clone()),
        microphone: Some(ipc::AudioMicrophoneState {
            muted: true,
            ..Default::default()
        }),
        ..Default::default()
    });
    vm.audio.devices = vec![ipc::AudioDevices {
        kind: ipc::AudioDeviceKind::Output as i32,
        revision: 8,
        devices: vec![ipc::AudioDevice {
            id: "fixture-output".into(),
            name: "Studio output — a deliberately long device name".into(),
            is_default: false,
            available: false,
        }],
        complete: false,
    }];
    if scene == "pending" {
        vm.audio.pending = true;
    }
    if scene == "error" {
        vm.audio.error = Some(text(lang, Key::AudioDeviceUnavailable).into());
    }
    if matches!(scene, "active" | "source" | "source-no-mic") {
        let status = vm.audio.status.as_mut().unwrap();
        status.enabled = true;
        let config = status.config.as_mut().unwrap();
        config.enabled = true;
        config.listener_node_id = [0xbb, 0x22].repeat(16);
        config.peers = vec![ipc::AudioPeerConfig {
            node_id: config.listener_node_id.clone(),
            allow_system_send: true,
            volume_percent: 100,
            ..Default::default()
        }];
        let mic = config.microphone.as_mut().unwrap();
        mic.control_enabled = true;
        mic.ptt_enabled = true;
        mic.ptt_hotkey = "Ctrl+Shift+F10".into();
        vm.audio.draft = config.clone();
        status.sessions.push(ipc::AudioSessionStatus {
            capture_active: Some(true),
            sending: Some(true),
            session_id: vec![0x33; 16],
            peer: [0xbb, 0x22].repeat(16),
            source_name: "LAPTOP".into(),
            local_role: ipc::AudioRole::Source as i32,
            state: ipc::AudioSessionState::Running as i32,
            profile: ipc::AudioProfile::Ethernet as i32,
            health_state: ipc::AudioStreamHealth::Ready as i32,
            kind: ipc::AudioKind::System as i32,
            gain_percent: 100,
            latency_estimate_ms: Some(42),
            health: Some(ipc::AudioHealth {
                received_packets: 123,
                ..Default::default()
            }),
            consent: Some(ipc::AudioSessionConsent {
                local: Some(ipc::AudioConsentFlags {
                    allow_system_send: true,
                    ..Default::default()
                }),
                remote: Some(ipc::AudioConsentFlags {
                    allow_system_receive: true,
                    ..Default::default()
                }),
            }),
            ..Default::default()
        });
        status.microphone = Some(ipc::AudioMicrophoneState {
            muted: false,
            control_active: true,
            ptt_pressed: true,
            capture_active: false,
            target_node_id: [0xbb, 0x22].repeat(16),
            native_ptt_arming: None,
            native_ptt_unavailable_reason: None,
        });
    }
    if scene == "source-no-mic" {
        let status = vm.audio.status.as_mut().unwrap();
        status
            .config
            .as_mut()
            .unwrap()
            .microphone
            .as_mut()
            .unwrap()
            .control_enabled = false;
        status.microphone.as_mut().unwrap().ptt_pressed = false;
        vm.audio.draft = status.config.clone().unwrap();
    }
    vm
}

fn frame(
    ctx: &egui::Context,
    vm: &ViewModel,
    size: egui::Vec2,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, Vec<Action>) {
    let mut actions = Vec::new();
    ui::apply_theme(ctx, &Palette::of(vm.look.theme));
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(ctx.input(|input| input.time) + 0.25),
            events,
            ..Default::default()
        },
        |ui| {
            egui::Frame::NONE
                .fill(Palette::of(vm.look.theme).bg)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            audio_settings::body(ui, &Palette::of(vm.look.theme), vm, &mut actions);
                        });
                });
        },
    );
    output.textures_delta.clear();
    (output, actions)
}

fn labels(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    fn visit(shape: &Shape, values: &mut Vec<(String, egui::Rect)>) {
        match shape {
            Shape::Text(s) => values.push((
                s.galley.text().into(),
                egui::Rect::from_min_size(s.pos, s.galley.size()),
            )),
            Shape::Vec(children) => {
                for child in children {
                    visit(child, values);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    for shape in &output.shapes {
        visit(&shape.shape, &mut values);
    }
    values
}

fn click(at: egui::Pos2) -> Vec<egui::Event> {
    vec![
        egui::Event::PointerMoved(at),
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: true,
            modifiers: Default::default(),
        },
        egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed: false,
            modifiers: Default::default(),
        },
    ]
}

fn open_advanced(ctx: &egui::Context, vm: &ViewModel, size: egui::Vec2) {
    let (output, _) = frame(ctx, vm, size, Vec::new());
    let at = labels(&output)
        .into_iter()
        .find(|(s, _)| s == text(vm.look.language, Key::AudioAdvanced))
        .unwrap()
        .1
        .center();
    frame(ctx, vm, size, click(at));
    for _ in 0..3 {
        frame(ctx, vm, size, Vec::new());
    }
}

#[test]
fn settings_render_localized_without_mutating_drafts_in_both_themes() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for width in [320.0, 760.0] {
                for scene in [
                    "unavailable",
                    "ready",
                    "pending",
                    "active",
                    "listener",
                    "error",
                ] {
                    let vm = fixture(lang, theme, scene);
                    let before = vm.audio.draft.clone();
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.enable_accesskit();
                    let size = egui::vec2(width, 6000.0);
                    frame(&ctx, &vm, size, Vec::new());
                    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
                    assert!(actions.is_empty(), "{scene}");
                    assert_eq!(vm.audio.draft, before);
                    let labels = labels(&output);
                    assert!(labels
                        .iter()
                        .any(|(s, _)| s == text(lang, Key::AudioDiagnostics)));
                    assert_eq!(
                        text(lang, Key::AudioDiagnostics),
                        if lang == Lang::En {
                            "Audio"
                        } else {
                            "Звук"
                        }
                    );
                    if vm.audio.status.is_some() {
                        assert!(labels
                            .iter()
                            .any(|(s, _)| s == text(lang, Key::AudioDetails)));
                    }
                    assert!(!labels.iter().any(|(s, _)| {
                        s.contains(support::PEER_NODE)
                            || s.contains(&"11".repeat(16))
                            || s.contains(text(lang, Key::AudioHealthCounters))
                    }));
                    if scene == "unavailable" {
                        assert!(labels
                            .iter()
                            .any(|(s, _)| s == text(lang, Key::AudioUnavailable)));
                    }
                    if scene == "pending" {
                        assert!(labels
                            .iter()
                            .any(|(s, _)| s == text(lang, Key::AudioPending)));
                    }
                    if scene == "active" {
                        assert!(labels
                            .iter()
                            .any(|(s, _)| s.contains(text(lang, Key::AudioSharing))
                                && s.contains("LAPTOP")));
                        assert!(labels
                            .iter()
                            .any(|(s, _)| s.contains(text(lang, Key::AudioMicControlActive))));
                        assert!(!labels
                            .iter()
                            .any(|(s, _)| s == text(lang, Key::AudioMicCaptureActive)));
                    }
                    assert!(output.platform_output.accesskit_update.is_some());
                }
            }
        }
    }
}

#[test]
fn receive_switch_is_one_explicit_intent_without_group_or_apply_ritual() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            let mut vm = fixture(lang, theme, "ready");
            vm.audio.draft.group_id.clear();
            let before = vm.audio.draft.clone();
            let ctx = egui::Context::default();
            ui::install_fonts(&ctx);
            let size = egui::vec2(400.0, 1200.0);
            frame(&ctx, &vm, size, Vec::new());
            let (output, _) = frame(&ctx, &vm, size, Vec::new());
            let values = labels(&output);
            let at = values
                .iter()
                .find(|(s, _)| s == text(lang, Key::AudioReceiveSystem))
                .unwrap()
                .1
                .center();
            let (_, actions) = frame(&ctx, &vm, size, click(at));
            assert_eq!(actions, [Action::Audio(AudioAction::Receive(true))]);
            assert_eq!(vm.audio.draft, before);
            for key in [
                Key::AudioUseThisListener,
                Key::AudioEnabled,
                Key::AudioStart,
                Key::AudioApply,
                Key::AudioRole,
            ] {
                assert!(!values
                    .iter()
                    .any(|(s, _)| s.eq_ignore_ascii_case(text(lang, key))));
            }
        }
    }
}

#[test]
fn microphone_summary_uses_committed_control_config_and_preserves_release() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for enabled in [true, false] {
                let mut vm = fixture(lang, theme, "active");
                vm.audio
                    .status
                    .as_mut()
                    .unwrap()
                    .config
                    .as_mut()
                    .unwrap()
                    .microphone
                    .as_mut()
                    .unwrap()
                    .control_enabled = enabled;
                vm.audio.draft.microphone.as_mut().unwrap().control_enabled = !enabled;
                vm.audio.gui_ptt_held = !enabled;
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                let size = egui::vec2(760.0, 6000.0);
                frame(&ctx, &vm, size, Vec::new());
                let (output, actions) = frame(&ctx, &vm, size, Vec::new());
                let values = labels(&output);
                assert_eq!(
                    values
                        .iter()
                        .any(|(s, _)| s.contains(text(lang, Key::AudioMicNotMuted))),
                    enabled,
                );
                for key in [Key::AudioMicMute, Key::AudioPttPress] {
                    assert_eq!(values.iter().any(|(s, _)| s == text(lang, key)), enabled);
                }
                // A held snapshot retains release even after microphone control is disabled.
                let release = values
                    .iter()
                    .find(|(s, _)| s == text(lang, Key::AudioPttRelease))
                    .expect("urgent release stays reachable")
                    .1
                    .center();
                let (_, released) = frame(&ctx, &vm, size, click(release));
                assert!(released
                    .iter()
                    .any(|action| matches!(action, Action::Audio(AudioAction::SetPtt(false)))));
                if !enabled {
                    assert!(!actions
                        .iter()
                        .any(|action| matches!(action, Action::Audio(AudioAction::SetPtt(false)))));
                }
            }
        }
    }
}

#[test]
fn share_switch_sends_one_intent_with_empty_auto_group_and_no_optimistic_activity() {
    for available in [true, false] {
        let mut vm = fixture(
            Lang::En,
            Theme::Dark,
            if available { "ready" } else { "unavailable" },
        );
        vm.audio.draft.group_id.clear();
        let before = vm.audio.draft.clone();
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(400.0, 1200.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        let at = labels(&output)
            .iter()
            .find(|(s, _)| s == text(Lang::En, Key::AudioShareSystem))
            .unwrap()
            .1
            .center();
        let (_, actions) = frame(&ctx, &vm, size, click(at));
        if available {
            assert_eq!(actions, [Action::Audio(AudioAction::Share(true))]);
        } else {
            assert!(actions.is_empty());
        }
        assert_eq!(vm.audio.draft, before);
        assert!(!vm.audio_player.open);
    }
}

#[test]
fn privacy_stop_and_release_remain_available_while_pending_or_status_is_stale() {
    for available in [true, false] {
        let mut vm = fixture(Lang::En, Theme::Dark, "active");
        vm.audio.pending = true;
        vm.audio.available = available;
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(760.0, 800.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        for key in [
            Key::AudioStopSystem,
            Key::AudioPttRelease,
            Key::AudioMicMute,
        ] {
            let at = labels(&output)
                .iter()
                .find(|(s, _)| s == text(Lang::En, key))
                .unwrap()
                .1
                .center();
            let (_, actions) = frame(&ctx, &vm, size, click(at));
            assert_eq!(actions.len(), 1);
            match key {
                Key::AudioStopSystem => {
                    assert!(matches!(
                        actions[0],
                        Action::Audio(AudioAction::StopSystemAudio)
                    ))
                }
                Key::AudioPttRelease => assert!(matches!(
                    actions[0],
                    Action::Audio(AudioAction::SetPtt(false))
                )),
                Key::AudioMicMute => assert!(matches!(
                    actions[0],
                    Action::Audio(AudioAction::SetMicrophoneMute(true))
                )),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn optional_offscreen_settings_pngs() {
    let Some(dir) = std::env::var_os("BIFROST_SETTINGS_SHOTS") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (lang, language) in [(Lang::En, "en"), (Lang::Ru, "ru")] {
        for (theme, theme_name) in [(Theme::Light, "light"), (Theme::Dark, "dark")] {
            for (width, height, layout) in [
                (320.0, 900.0, "narrow"),
                (760.0, 900.0, "wide"),
                (320.0, 6000.0, "narrow-full"),
                (760.0, 6000.0, "wide-full"),
            ] {
                for scene in [
                    "unavailable",
                    "ready",
                    "pending",
                    "source",
                    "source-no-mic",
                    "listener",
                    "error",
                ] {
                    let vm = fixture(lang, theme, scene);
                    let mut harness = egui_kittest::Harness::builder()
                        .with_size(egui::vec2(width, height))
                        .wgpu()
                        .build_ui(move |ui| {
                            if !ui
                                .data_mut(|data| {
                                    data.get_temp::<bool>(egui::Id::new("settings-fonts"))
                                })
                                .unwrap_or(false)
                            {
                                ui::install_fonts(ui.ctx());
                                ui.data_mut(|data| {
                                    data.insert_temp(egui::Id::new("settings-fonts"), true)
                                });
                            }
                            ui::apply_theme(ui.ctx(), &Palette::of(theme));
                            ui.set_style(ui.ctx().global_style());
                            egui::Frame::NONE
                                .fill(Palette::of(theme).bg)
                                .show(ui, |ui| {
                                    egui::ScrollArea::vertical()
                                        .auto_shrink([false, false])
                                        .show(ui, |ui| {
                                            audio_settings::body(
                                                ui,
                                                &Palette::of(theme),
                                                &vm,
                                                &mut Vec::new(),
                                            );
                                        });
                                });
                        });
                    harness.run_steps(3);
                    harness
                        .render()
                        .unwrap()
                        .save(dir.join(format!(
                            "settings-{scene}-{language}-{theme_name}-{layout}.png"
                        )))
                        .unwrap();
                }
            }
            for (width, height, layout) in [(320.0, 700.0, "narrow"), (1000.0, 900.0, "wide")] {
                for scene in ["source", "listener"] {
                    let mut vm = fixture(lang, theme, scene);
                    vm.modal = Some(Modal::Audio);
                    let now = std::time::Instant::now();
                    let mut state = ui::UiState::new(now);
                    let mut elapsed = 0.0;
                    let mut harness = egui_kittest::Harness::builder()
                        .with_size(egui::vec2(width, height))
                        .wgpu()
                        .build_ui(move |ui| {
                            elapsed += 0.25;
                            ui::draw(
                                ui,
                                &vm,
                                &mut state,
                                ui::Env {
                                    now: now + std::time::Duration::from_secs_f64(elapsed),
                                    animate: false,
                                },
                            );
                        });
                    harness.run_steps(5);
                    harness
                        .render()
                        .unwrap()
                        .save(dir.join(format!(
                            "settings-modal-{scene}-{language}-{theme_name}-{layout}.png"
                        )))
                        .unwrap();
                }
            }
        }
    }
}

#[test]
fn ptt_is_held_and_releases_outside_the_button_without_optimistic_state() {
    let mut vm = fixture(Lang::En, Theme::Dark, "active");
    vm.audio
        .status
        .as_mut()
        .unwrap()
        .microphone
        .as_mut()
        .unwrap()
        .ptt_pressed = false;
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(760.0, 900.0);
    frame(&ctx, &vm, size, Vec::new());
    let (output, _) = frame(&ctx, &vm, size, Vec::new());
    let at = labels(&output)
        .iter()
        .find(|(s, _)| s == text(Lang::En, Key::AudioPttPress))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(
        &ctx,
        &vm,
        size,
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ],
    );
    assert!(matches!(
        actions.as_slice(),
        [Action::Audio(AudioAction::SetPtt(true))]
    ));
    assert!(
        !vm.audio
            .status
            .as_ref()
            .unwrap()
            .microphone
            .as_ref()
            .unwrap()
            .ptt_pressed
    );
    let outside = egui::pos2(750.0, 890.0);
    let (_, actions) = frame(
        &ctx,
        &vm,
        size,
        vec![
            egui::Event::PointerMoved(outside),
            egui::Event::PointerButton {
                pos: outside,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            },
        ],
    );
    assert!(matches!(
        actions.as_slice(),
        [Action::Audio(AudioAction::SetPtt(false))]
    ));
}

#[test]
fn narrow_settings_scroll_to_microphone_and_apply_without_horizontal_scrolling() {
    let vm = fixture(Lang::Ru, Theme::Dark, "ready");
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(320.0, 700.0);
    frame(&ctx, &vm, size, Vec::new());
    frame(&ctx, &vm, size, Vec::new());
    open_advanced(&ctx, &vm, size);
    frame(
        &ctx,
        &vm,
        size,
        vec![
            egui::Event::PointerMoved(egui::pos2(150.0, 350.0)),
            egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -10000.0),
                phase: egui::TouchPhase::Move,
                modifiers: Default::default(),
            },
        ],
    );
    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
    assert!(actions.is_empty());
    let values = labels(&output);
    assert!(values
        .iter()
        .any(|(s, _)| s == text(Lang::Ru, Key::AudioApply)));
    for (_, rect) in values
        .iter()
        .filter(|(s, _)| s == text(Lang::Ru, Key::AudioApply))
    {
        assert!(rect.min.x >= 0.0 && rect.max.x <= size.x);
    }
}

#[test]
fn listener_group_setup_is_primary_and_advanced_fields_are_collapsed() {
    let mut vm = fixture(Lang::En, Theme::Light, "ready");
    vm.audio.draft.role = ipc::AudioRole::Listener as i32;
    vm.audio.status.as_mut().unwrap().role = ipc::AudioRole::Listener as i32;
    vm.audio.status.as_mut().unwrap().config = Some(vm.audio.draft.clone());
    let before = vm.audio.draft.clone();
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(760.0, 6000.0);
    frame(&ctx, &vm, size, Vec::new());
    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
    assert!(actions.is_empty());
    let values = labels(&output);
    for key in [
        Key::AudioGroup,
        Key::AudioMicStreaming,
        Key::AudioExperimentalAcknowledged,
        Key::AudioVirtualDevice,
    ] {
        assert!(!values
            .iter()
            .any(|(s, _)| s.eq_ignore_ascii_case(text(Lang::En, key))));
    }
    assert!(values
        .iter()
        .any(|(s, _)| s == text(Lang::En, Key::AudioReceiveSystem)));
    assert!(!values
        .iter()
        .any(|(s, _)| s == text(Lang::En, Key::AudioApply)));
    assert_eq!(vm.audio.draft, before);
    let at = values
        .iter()
        .find(|(s, _)| s == text(Lang::En, Key::AudioAdvanced))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(&ctx, &vm, size, click(at));
    assert!(actions.is_empty());
    let (output, _) = frame(&ctx, &vm, size, Vec::new());
    let values = labels(&output);
    for key in [
        Key::AudioGroup,
        Key::AudioMicStreaming,
        Key::AudioExperimentalAcknowledged,
        Key::AudioVirtualDevice,
    ] {
        assert!(values
            .iter()
            .any(|(s, _)| s.eq_ignore_ascii_case(text(Lang::En, key))));
    }
}

#[test]
fn details_reveal_raw_counters_without_exposing_ids_in_normal_flow() {
    for lang in [Lang::En, Lang::Ru] {
        let vm = fixture(lang, Theme::Dark, "source");
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(760.0, 6000.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        let values = labels(&output);
        assert!(values.iter().any(|(s, _)| s.contains("LAPTOP")));
        assert!(!values.iter().any(|(s, _)| {
            s.contains(support::PEER_NODE)
                || s.contains(&"11".repeat(16))
                || s.contains(text(lang, Key::AudioHealthCounters))
        }));
        let at = values
            .iter()
            .find(|(s, _)| s == text(lang, Key::AudioDetails))
            .unwrap()
            .1
            .center();
        let (_, actions) = frame(&ctx, &vm, size, click(at));
        assert!(actions.is_empty());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        assert!(labels(&output).iter().any(|(s, _)| {
            s.contains(text(lang, Key::AudioHealthCounters)) && s.contains("123")
        }));
    }
}

#[test]
fn disabled_ptt_hides_hold_and_hotkey_but_keeps_emergency_release() {
    for scene in ["ready", "active"] {
        let mut vm = fixture(Lang::En, Theme::Dark, scene);
        vm.audio.draft.microphone.as_mut().unwrap().ptt_enabled = false;
        vm.audio.status.as_mut().unwrap().config = Some(vm.audio.draft.clone());
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(760.0, 6000.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        let values = labels(&output);
        for key in [Key::AudioPttPress, Key::AudioPttHotkey] {
            assert!(!values.iter().any(|(s, _)| s == text(Lang::En, key)));
        }
        if scene == "active" {
            let at = values
                .iter()
                .find(|(s, _)| s == text(Lang::En, Key::AudioPttRelease))
                .unwrap()
                .1
                .center();
            let (_, actions) = frame(&ctx, &vm, size, click(at));
            assert!(matches!(
                actions.as_slice(),
                [Action::Audio(AudioAction::SetPtt(false))]
            ));
        }
    }
}

#[test]
fn actual_audio_modal_scrolls_to_apply_without_horizontal_clipping() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for width in [320.0, 1000.0] {
                for scene in ["source", "listener"] {
                    let mut vm = fixture(lang, theme, scene);
                    vm.modal = Some(Modal::Audio);
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    let now = std::time::Instant::now();
                    let mut state = ui::UiState::new(now);
                    let size = egui::vec2(width, 700.0);
                    let mut elapsed = 0.0;
                    let mut draw = |events| {
                        elapsed += 0.25;
                        let mut produced = ui::Output::default();
                        let mut output = ctx.run_ui(
                            egui::RawInput {
                                screen_rect: Some(egui::Rect::from_min_size(
                                    egui::Pos2::ZERO,
                                    size,
                                )),
                                time: Some(elapsed),
                                events,
                                ..Default::default()
                            },
                            |ui| {
                                produced = ui::draw(
                                    ui,
                                    &vm,
                                    &mut state,
                                    ui::Env {
                                        now: now + std::time::Duration::from_secs_f64(elapsed),
                                        animate: false,
                                    },
                                );
                            },
                        );
                        output.textures_delta.clear();
                        assert!(produced.actions.is_empty());
                        output
                    };
                    for _ in 0..5 {
                        draw(Vec::new());
                    }
                    draw(vec![
                        egui::Event::PointerMoved(egui::pos2(width * 0.5, 350.0)),
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, -10000.0),
                            phase: egui::TouchPhase::Move,
                            modifiers: Default::default(),
                        },
                    ]);
                    let output = draw(Vec::new());
                    let at = labels(&output)
                        .into_iter()
                        .find(|(s, _)| s == text(lang, Key::AudioAdvanced))
                        .unwrap()
                        .1
                        .center();
                    draw(click(at));
                    for _ in 0..3 {
                        draw(Vec::new());
                    }
                    draw(vec![
                        egui::Event::PointerMoved(egui::pos2(width * 0.5, 350.0)),
                        egui::Event::MouseWheel {
                            unit: egui::MouseWheelUnit::Point,
                            delta: egui::vec2(0.0, -10000.0),
                            phase: egui::TouchPhase::Move,
                            modifiers: Default::default(),
                        },
                    ]);
                    let output = draw(Vec::new());
                    let values = labels(&output);
                    let title = values
                        .iter()
                        .find(|(s, _)| s == Modal::Audio.title(lang))
                        .unwrap();
                    let modal_right = width * 0.5 + ((width - 48.0).min(680.0)) * 0.5;
                    assert!(title.1.max.x <= modal_right - 60.0, "title overlaps close");
                    let apply = values
                        .iter()
                        .find(|(s, _)| s == text(lang, Key::AudioApply))
                        .unwrap_or_else(|| panic!("Apply unreachable: {scene}, {width}, {lang:?}"));
                    assert!(apply.1.min.x >= 0.0 && apply.1.max.x <= size.x);
                    assert!(apply.1.min.y >= 0.0 && apply.1.max.y <= size.y);
                }
            }
        }
    }
}

#[test]
fn unnamed_peer_uses_short_lowercase_identity_not_session_supplied_name() {
    let mut vm = fixture(Lang::En, Theme::Light, "source");
    vm.status.as_mut().unwrap().members[1].display_name.clear();
    vm.audio.status.as_mut().unwrap().sessions[0].source_name = "UNTRUSTED SESSION NAME".into();
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(760.0, 6000.0);
    frame(&ctx, &vm, size, Vec::new());
    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
    assert!(actions.is_empty());
    let values = labels(&output);
    assert!(values.iter().any(|(s, _)| s.contains("bb22bb22")));
    assert!(!values.iter().any(|(s, _)| {
        s.contains(support::PEER_NODE)
            || s.contains("UNTRUSTED SESSION NAME")
            || s.contains(&"11".repeat(16))
    }));
}

#[test]
fn apply_emits_only_the_typed_intent_without_optimistic_activation() {
    let mut vm = fixture(Lang::En, Theme::Light, "ready");
    vm.audio.draft.mode = ipc::AudioMode::FollowFocus as i32;
    vm.audio.dirty = true;
    let before = vm.audio.draft.clone();
    let ctx = egui::Context::default();
    assert_eq!(before.group_id_override, Some(false));
    ui::install_fonts(&ctx);
    let size = egui::vec2(760.0, 6000.0);
    frame(&ctx, &vm, size, Vec::new());
    open_advanced(&ctx, &vm, size);
    let (output, _) = frame(&ctx, &vm, size, Vec::new());
    let at = labels(&output)
        .iter()
        .find(|(s, _)| s == text(Lang::En, Key::AudioApply))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(&ctx, &vm, size, click(at));
    assert!(matches!(
        actions.as_slice(),
        [Action::Audio(AudioAction::ApplyConfig)]
    ));
    assert_eq!(vm.audio.draft, before);
    assert!(!vm.audio.status.as_ref().unwrap().enabled);
}

#[test]
fn passive_status_loss_and_reconnect_do_not_emit_global_release_or_a_fresh_press() {
    let mut vm = fixture(Lang::En, Theme::Dark, "active");
    vm.audio
        .status
        .as_mut()
        .unwrap()
        .microphone
        .as_mut()
        .unwrap()
        .ptt_pressed = false;
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(760.0, 900.0);
    frame(&ctx, &vm, size, Vec::new());
    let (output, _) = frame(&ctx, &vm, size, Vec::new());
    let at = labels(&output)
        .iter()
        .find(|(label, _)| label == text(Lang::En, Key::AudioPttPress))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(
        &ctx,
        &vm,
        size,
        vec![
            egui::Event::PointerMoved(at),
            egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            },
        ],
    );
    assert!(matches!(
        actions.as_slice(),
        [Action::Audio(AudioAction::SetPtt(true))]
    ));
    // A reconnect clears desired GUI ownership, not the physical mouse-down state.
    let status = vm.audio.status.take().unwrap();
    vm.audio.gui_ptt_held = false;
    vm.audio.available = false;
    let (_, actions) = frame(&ctx, &vm, size, Vec::new());
    assert!(!actions
        .iter()
        .any(|action| matches!(action, Action::Audio(AudioAction::SetPtt(_)))));
    vm.audio.status = Some(status);
    vm.audio.available = true;
    let (_, actions) = frame(&ctx, &vm, size, Vec::new());
    assert!(!actions
        .iter()
        .any(|action| matches!(action, Action::Audio(AudioAction::SetPtt(_)))));
}

#[test]
fn native_ptt_arming_is_localized_and_never_disables_gui_hold() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for (state, reason, key) in [
                (None, None, Key::AudioNativePttUnknown),
                (
                    Some(ipc::AudioPttArmingState::Armed),
                    None,
                    Key::AudioNativePttArmed,
                ),
                (
                    Some(ipc::AudioPttArmingState::NeedsPhysicalRelease),
                    None,
                    Key::AudioNativePttNeedsRelease,
                ),
                (
                    Some(ipc::AudioPttArmingState::Unavailable),
                    None,
                    Key::AudioNativePttUnavailable,
                ),
                (
                    Some(ipc::AudioPttArmingState::Unavailable),
                    Some(ipc::AudioPttUnavailableReason::HostClockUnavailable),
                    Key::AudioNativePttHostClockUnavailable,
                ),
                (
                    Some(ipc::AudioPttArmingState::Unavailable),
                    Some(ipc::AudioPttUnavailableReason::PortalFreshnessUnverifiable),
                    Key::AudioNativePttPortalFreshnessUnverifiable,
                ),
                (
                    Some(ipc::AudioPttArmingState::Unavailable),
                    Some(ipc::AudioPttUnavailableReason::RegistrationUnavailable),
                    Key::AudioNativePttRegistrationUnavailable,
                ),
            ] {
                let mut vm = fixture(lang, theme, "active");
                let mic = vm
                    .audio
                    .status
                    .as_mut()
                    .unwrap()
                    .microphone
                    .as_mut()
                    .unwrap();
                mic.ptt_pressed = false;
                mic.native_ptt_arming = state.map(|state| state as i32);
                mic.native_ptt_unavailable_reason = reason.map(|reason| reason as i32);
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                let size = egui::vec2(760.0, 1200.0);
                frame(&ctx, &vm, size, Vec::new());
                let (output, actions) = frame(&ctx, &vm, size, Vec::new());
                assert!(actions.is_empty());
                let labels = labels(&output);
                assert!(labels.iter().any(|(label, _)| label == text(lang, key)));
                assert_eq!(
                    labels
                        .iter()
                        .any(|(label, _)| label == text(lang, Key::AudioNativePttArmHint)),
                    state == Some(ipc::AudioPttArmingState::NeedsPhysicalRelease)
                );
                let at = labels
                    .iter()
                    .find(|(label, _)| label == text(lang, Key::AudioPttPress))
                    .unwrap()
                    .1
                    .center();
                let (_, actions) = frame(
                    &ctx,
                    &vm,
                    size,
                    vec![
                        egui::Event::PointerMoved(at),
                        egui::Event::PointerButton {
                            pos: at,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: Default::default(),
                        },
                    ],
                );
                assert!(matches!(
                    actions.as_slice(),
                    [Action::Audio(AudioAction::SetPtt(true))]
                ));
            }
        }
    }
}

#[test]
fn source_and_receive_off_remain_actionable_during_pending_start_and_status_loss() {
    for (role, key) in [
        (ipc::AudioRole::Source, Key::AudioShareSystem),
        (ipc::AudioRole::Listener, Key::AudioReceiveSystem),
    ] {
        for available in [true, false] {
            let mut vm = fixture(Lang::Ru, Theme::Dark, "pending");
            vm.audio.available = available;
            vm.audio.system_intent = Some((role, true));
            let ctx = egui::Context::default();
            ui::install_fonts(&ctx);
            let size = egui::vec2(320.0, 1600.0);
            frame(&ctx, &vm, size, Vec::new());
            let (output, _) = frame(&ctx, &vm, size, Vec::new());
            let at = labels(&output)
                .into_iter()
                .find(|(s, _)| s == text(Lang::Ru, key))
                .unwrap()
                .1
                .center();
            let (_, actions) = frame(&ctx, &vm, size, click(at));
            let expected = if role == ipc::AudioRole::Source {
                AudioAction::Share(false)
            } else {
                AudioAction::Receive(false)
            };
            assert_eq!(actions, [Action::Audio(expected)]);
        }
    }
}

#[test]
fn task_first_status_and_switch_bounds_wrap_in_both_locales_at_effective_narrow_widths() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for width in [280.0, 320.0, 400.0, 760.0] {
                for scale in [1.0, 1.5, 2.0] {
                    let mut vm = fixture(lang, theme, "error");
                    vm.audio.error = Some(text(lang, Key::AudioSystemConsentRequired).into());
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.set_pixels_per_point(scale);
                    ctx.enable_accesskit();
                    let size = egui::vec2(width, 1600.0);
                    frame(&ctx, &vm, size, Vec::new());
                    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
                    assert!(actions.is_empty());
                    let values = labels(&output);
                    for (value, rect) in &values {
                        assert!(
                            rect.min.x >= 12.0 && rect.max.x <= width - 12.0,
                            "{lang:?} {width} {scale}: {value}: {rect:?}"
                        );
                    }
                    for key in [
                        Key::AudioShareSystem,
                        Key::AudioReceiveSystem,
                        Key::AudioAdvanced,
                    ] {
                        assert!(values.iter().any(|(s, _)| s == text(lang, key)));
                    }
                    for key in [
                        Key::AudioRefresh,
                        Key::AudioApply,
                        Key::AudioStart,
                        Key::AudioEnabled,
                    ] {
                        assert!(!values.iter().any(|(s, _)| s == text(lang, key)));
                    }
                    let tree = output
                        .platform_output
                        .accesskit_update
                        .as_ref()
                        .expect("controls");
                    for (_, node) in &tree.nodes {
                        if [
                            text(lang, Key::AudioShareSystem),
                            text(lang, Key::AudioReceiveSystem),
                        ]
                        .contains(&node.label().unwrap_or_default())
                        {
                            let bounds = node.bounds().expect("switch hit target");
                            assert!(bounds.x0 >= 12.0 && bounds.x1 <= f64::from(width) - 12.0);
                            assert!(bounds.height() >= 18.0);
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn queued_previous_failure_cannot_remove_new_switch_off_action_before_current_ack() {
    let mut vm = fixture(Lang::En, Theme::Dark, "ready");
    vm.audio_action(&AudioAction::Share(true), std::time::Instant::now());
    let (first_id, _) = vm.take_outgoing().pop().unwrap();
    vm.audio_result(
        first_id,
        bifrost_gui::audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: false,
            error_code: "AUDIO_CONSENT_REQUIRED".into(),
        },
    );
    vm.audio_action(&AudioAction::Share(true), std::time::Instant::now());
    let (new_on_id, _) = vm.take_outgoing().pop().unwrap();
    let mut queued = vm.audio.status.clone().unwrap();
    queued.revision += 1;
    queued.system_sharing_requested = Some(false);
    queued.system_sharing_error = "AUDIO_CONSENT_REQUIRED".into();
    vm.apply_audio_status(0, queued);
    assert!(vm.audio.pending);
    assert!(vm.audio.sharing_requested(ipc::AudioRole::Source));
    assert!(
        vm.audio.error_message(Lang::En).is_none(),
        "old failure is not this attempt"
    );
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(320.0, 1600.0);
    frame(&ctx, &vm, size, Vec::new());
    let (output, _) = frame(&ctx, &vm, size, Vec::new());
    let at = labels(&output)
        .into_iter()
        .find(|(s, _)| s == text(Lang::En, Key::AudioShareSystem))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(&ctx, &vm, size, click(at));
    assert_eq!(actions, [Action::Audio(AudioAction::Share(false))]);
    vm.audio_action(&AudioAction::Share(false), std::time::Instant::now());
    vm.audio_result(
        new_on_id,
        bifrost_gui::audio::AudioRequest::Mutation,
        ipc::OpResult {
            ok: true,
            error_code: String::new(),
        },
    );
    assert!(vm.audio.pending);
    assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
    assert!(!vm.audio_player.open);
}

#[test]
fn denied_switch_keeps_inline_recovery_message_across_unrelated_telemetry() {
    for lang in [Lang::En, Lang::Ru] {
        let mut vm = fixture(lang, Theme::Dark, "ready");
        vm.audio_action(&AudioAction::Share(true), std::time::Instant::now());
        let (id, _) = vm.take_outgoing().pop().unwrap();
        vm.audio_result(
            id,
            bifrost_gui::audio::AudioRequest::Mutation,
            ipc::OpResult {
                ok: false,
                error_code: "AUDIO_CONSENT_REQUIRED".into(),
            },
        );
        let mut telemetry = vm.audio.status.clone().unwrap();
        telemetry.revision += 1;
        telemetry.system_sharing_requested = Some(false);
        telemetry.system_sharing_error.clear();
        vm.apply_audio_status(0, telemetry);
        assert!(!vm.audio.sharing_requested(ipc::AudioRole::Source));
        assert_eq!(
            vm.audio.error_message(lang),
            Some(text(lang, Key::AudioSystemConsentRequired))
        );
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(320.0, 1600.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        assert!(labels(&output)
            .iter()
            .any(|(s, _)| s == text(lang, Key::AudioSystemConsentRequired)));
    }
}
