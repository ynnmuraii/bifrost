//! No core, audio endpoint or native input. Optional PNGs use BIFROST_SETTINGS_SHOTS.
mod support;

use bifrost_gui::audio::AudioAction;
use bifrost_gui::egui::{self, Shape};
use bifrost_gui::gui_settings::Theme;
use bifrost_gui::ui::{self, audio_settings, Palette};
use bifrost_gui::view_model::{Action, ViewModel};
use protocol::ipc;
use ui_text::{text, Key, Lang};

fn fixture(lang: Lang, theme: Theme, scene: &str) -> ViewModel {
    let mut vm = support::view();
    vm.look.language = lang;
    vm.look.theme = theme;
    vm.status = Some(support::status_with("LOCAL"));
    vm.connection = bifrost_gui::view_model::Connection::Connected;
    if scene == "unavailable" {
        return vm;
    }
    vm.audio.available = true;
    vm.audio.draft.group_id = vec![0x11; 16];
    vm.audio.status = Some(ipc::AudioStatus {
        revision: 8,
        enabled: false,
        role: ipc::AudioRole::Source as i32,
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
    if scene == "active" {
        let status = vm.audio.status.as_mut().unwrap();
        status.enabled = true;
        let config = status.config.as_mut().unwrap();
        config.enabled = true;
        config.listener_node_id = [0xbb, 0x22].repeat(16);
        config.peers = vec![ipc::AudioPeerConfig {
            node_id: config.listener_node_id.clone(),
            allow_system_send: true,
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
            health: Some(ipc::AudioHealth::default()),
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
        });
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
            events,
            ..Default::default()
        },
        |ui| {
            egui::Frame::NONE
                .fill(Palette::of(vm.look.theme).bg)
                .show(ui, |ui| {
                    audio_settings::body(ui, &Palette::of(vm.look.theme), vm, &mut actions);
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

#[test]
fn settings_render_localized_without_mutating_drafts_in_both_themes() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for width in [320.0, 760.0] {
                for scene in ["unavailable", "ready", "pending", "active", "error"] {
                    let vm = fixture(lang, theme, scene);
                    let before = vm.audio.draft.clone();
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.enable_accesskit();
                    let size = egui::vec2(width, 800.0);
                    frame(&ctx, &vm, size, Vec::new());
                    let (output, actions) = frame(&ctx, &vm, size, Vec::new());
                    assert!(actions.is_empty(), "{scene}");
                    assert_eq!(vm.audio.draft, before);
                    let labels = labels(&output);
                    assert!(labels
                        .iter()
                        .any(|(s, _)| s == text(lang, Key::AudioDiagnostics)));
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
                            .any(|(s, _)| s == text(lang, Key::AudioMicControlActive)));
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
fn enabled_checkbox_emits_only_draft_and_unavailable_checkbox_emits_nothing() {
    for available in [true, false] {
        let vm = fixture(
            Lang::En,
            Theme::Dark,
            if available { "ready" } else { "unavailable" },
        );
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(760.0, 1000.0);
        frame(&ctx, &vm, size, Vec::new());
        let (output, _) = frame(&ctx, &vm, size, Vec::new());
        let at = labels(&output)
            .iter()
            .find(|(s, _)| s == text(Lang::En, Key::AudioEnabled))
            .unwrap()
            .1
            .center();
        let (_, actions) = frame(&ctx, &vm, size, click(at));
        if available {
            assert_eq!(actions.len(), 1);
            assert!(
                matches!(&actions[0], Action::Audio(AudioAction::EditConfig(config)) if config.enabled)
            );
            assert!(!vm.audio.draft.enabled);
        } else {
            assert!(actions.is_empty());
        }
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
        for key in [Key::AudioStopAll, Key::AudioPttRelease, Key::AudioMicMute] {
            let at = labels(&output)
                .iter()
                .find(|(s, _)| s == text(Lang::En, key))
                .unwrap()
                .1
                .center();
            let (_, actions) = frame(&ctx, &vm, size, click(at));
            assert_eq!(actions.len(), 1);
            match key {
                Key::AudioStopAll => {
                    assert!(matches!(actions[0], Action::Audio(AudioAction::StopAll)))
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
                for scene in ["unavailable", "ready", "pending", "active", "error"] {
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
                                    audio_settings::body(
                                        ui,
                                        &Palette::of(theme),
                                        &vm,
                                        &mut Vec::new(),
                                    );
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
fn listener_group_setup_is_an_explicit_intent_and_deferred_fields_are_visible() {
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
        Key::AudioMicStreaming,
        Key::AudioExperimentalAcknowledged,
        Key::AudioVirtualDevice,
    ] {
        assert!(values.iter().any(|(s, _)| s == text(Lang::En, key)));
    }
    let at = values
        .iter()
        .find(|(s, _)| s == text(Lang::En, Key::AudioCreateGroup))
        .unwrap()
        .1
        .center();
    let (_, actions) = frame(&ctx, &vm, size, click(at));
    assert!(matches!(
        actions.as_slice(),
        [Action::Audio(AudioAction::CreateGroup)]
    ));
    assert_eq!(vm.audio.draft, before);
}
