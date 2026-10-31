//! Pure IPC/view/render fixtures; no core, native audio, network or input.
//! Optional offscreen PNGs go to the external BIFROST_PATH_SHOTS directory.
mod support;

use bifrost_gui::audio;
use bifrost_gui::audio_path::AudioPathObservation;
use bifrost_gui::audio_player::{AudioPlayerAction, StreamState};
use bifrost_gui::egui::{self, Shape};
use bifrost_gui::gui_settings::Theme;
use bifrost_gui::ui::{self, audio_player, audio_settings, Palette};
use bifrost_gui::view_model::ViewModel;
use protocol::ipc::{self, gui_frame::Msg};
use ui_text::{fill_named, text, Key, Lang};

const LOCAL: &str = "WORKSTATION";
const PEER: &str = "LAPTOP";
const STALE_NAME: &str = "STALE SESSION NAME";

fn snapshot(role: ipc::AudioRole, diagnostic: i32) -> ipc::AudioStatus {
    let mut config = audio::default_config();
    config.enabled = true;
    config.role = role as i32;
    config.group_id = vec![0x11; 16];
    let source = role == ipc::AudioRole::Source;
    if source {
        config.listener_node_id = [0xbb, 0x22].repeat(16);
    }
    config.peers = vec![ipc::AudioPeerConfig {
        node_id: [0xbb, 0x22].repeat(16),
        allow_system_send: source,
        allow_system_receive: !source,
        volume_percent: 100,
        ..Default::default()
    }];
    // Same evidence as apps/bifrost/tests/audio_path.rs: failed agreement,
    // zero received media, no active capture/send, parent membership still up.
    ipc::AudioStatus {
        revision: 8,
        enabled: true,
        role: role as i32,
        mode: config.mode,
        config: Some(config),
        microphone: Some(ipc::AudioMicrophoneState {
            muted: true,
            ..Default::default()
        }),
        sessions: vec![ipc::AudioSessionStatus {
            session_id: vec![0x33; 16],
            peer: [0xbb, 0x22].repeat(16),
            source_name: STALE_NAME.into(),
            local_role: role as i32,
            state: ipc::AudioSessionState::Failed as i32,
            profile: ipc::AudioProfile::Ethernet as i32,
            health_state: ipc::AudioStreamHealth::Degraded as i32,
            kind: ipc::AudioKind::System as i32,
            gain_percent: 100,
            health: Some(ipc::AudioHealth::default()),
            consent: Some(ipc::AudioSessionConsent {
                local: Some(ipc::AudioConsentFlags {
                    allow_system_send: source,
                    allow_system_receive: !source,
                    ..Default::default()
                }),
                remote: Some(ipc::AudioConsentFlags {
                    allow_system_send: !source,
                    allow_system_receive: source,
                    ..Default::default()
                }),
            }),
            path_diagnostic: diagnostic,
            local_udp_port: 47903,
            peer_udp_port: 61000,
            capture_active: Some(false),
            sending: Some(false),
            ..Default::default()
        }],
        ..Default::default()
    }
}

fn fixture(lang: Lang, theme: Theme, status: ipc::AudioStatus) -> ViewModel {
    // Decode is also the real semantic boundary: all render fixtures must be valid IPC.
    bifrost_gui::audio_wire::decode_status(&status).expect("valid audio path snapshot");
    let mut vm = support::view();
    vm.look.language = lang;
    vm.look.theme = theme;
    let mut members = support::status_with("LOCAL");
    members.members[0].display_name = LOCAL.into();
    members.members[1].display_name = PEER.into();
    vm.apply(support::event(Msg::Status(members)));
    vm.apply(support::event(Msg::AudioStatus(status)));
    assert!(vm.audio.available);
    assert!(vm.take_outgoing().is_empty());
    vm
}

fn labels(output: &egui::FullOutput) -> Vec<(String, egui::Rect)> {
    fn visit(shape: &Shape, values: &mut Vec<(String, egui::Rect)>) {
        match shape {
            Shape::Text(t) => values.push((
                t.galley.text().into(),
                egui::Rect::from_min_size(t.pos, t.galley.size()),
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

fn frame(ctx: &egui::Context, vm: &ViewModel, width: f32, popup: bool) -> egui::FullOutput {
    let palette = Palette::of(vm.look.theme);
    ui::apply_theme(ctx, &palette);
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, if popup { 600.0 } else { 6000.0 }),
            )),
            ..Default::default()
        },
        |ui| {
            egui::Frame::NONE.fill(palette.bg).show(ui, |ui| {
                if popup {
                    assert!(audio_player::draw(
                        ui,
                        &vm.audio_player.cards,
                        vm.look.theme,
                        vm.look.language,
                    )
                    .is_empty());
                } else {
                    let mut actions = Vec::new();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .show(ui, |ui| {
                            audio_settings::body(ui, &palette, vm, &mut actions);
                        });
                    assert!(actions.is_empty());
                }
            });
        },
    );
    output.textures_delta.clear();
    output
}

#[test]
fn typed_reasons_zero_through_four_match_wire_and_model_and_localize() {
    let reasons = [
        (ipc::AudioPathDiagnostic::None, Key::AudioPathNone),
        (
            ipc::AudioPathDiagnostic::NoAuthenticatedPeerPackets,
            Key::AudioPathNoPackets,
        ),
        (
            ipc::AudioPathDiagnostic::PeerProbesMissing,
            Key::AudioPathPeerProbes,
        ),
        (
            ipc::AudioPathDiagnostic::ProbeResponsesMissing,
            Key::AudioPathResponses,
        ),
        (
            ipc::AudioPathDiagnostic::AuthenticationFailures,
            Key::AudioPathAuthentication,
        ),
    ];
    for (number, (diagnostic, key)) in reasons.into_iter().enumerate() {
        assert_eq!(diagnostic as i32, number as i32);
        for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
            let status = snapshot(role, diagnostic as i32);
            let model = bifrost_gui::audio_wire::decode_status(&status).unwrap();
            let observation = AudioPathObservation::from_wire(&status.sessions[0]).unwrap();
            assert_eq!(
                AudioPathObservation::from_model(&model.sessions[0]),
                Some(observation)
            );
            assert_eq!(observation.diagnostic, diagnostic);
            assert_eq!(
                (observation.local_udp_port, observation.peer_udp_port),
                (47903, 61000)
            );
            assert_eq!(observation.has_diagnostic(), number != 0);
            for lang in [Lang::En, Lang::Ru] {
                assert_eq!(
                    observation.reason(lang, PEER),
                    fill_named(lang, key, &[("peer", &PEER)])
                );
                let ports = observation.ports(lang, PEER);
                assert!(ports.contains(PEER) && ports.contains("47903") && ports.contains("61000"));
                assert_eq!(
                    observation.tooltip(lang, PEER),
                    format!("{}\n{ports}", observation.reason(lang, PEER))
                );
                let guidance = observation.guidance(lang, role, LOCAL, PEER);
                assert_eq!(guidance.is_some(), number != 0);
                if let Some(guidance) = guidance {
                    let (port, listener, source) = if role == ipc::AudioRole::Source {
                        ("61000", PEER, LOCAL)
                    } else {
                        ("47903", LOCAL, PEER)
                    };
                    assert_eq!(
                        guidance,
                        fill_named(
                            lang,
                            Key::AudioPathFirewall,
                            &[
                                ("port", &port),
                                ("listener", &listener),
                                ("source", &source),
                            ]
                        )
                    );
                    // Guidance is conditional and source-scoped, never a diagnosis of the firewall.
                    assert!(guidance.starts_with(if lang == Lang::En {
                        "If "
                    } else {
                        "Если "
                    }));
                    assert!(guidance.contains(if lang == Lang::En {
                        " only"
                    } else {
                        "только от"
                    }));
                    assert!(!guidance.contains(if port == "61000" { "47903" } else { "61000" }));
                }
            }
        }
    }
}

#[test]
fn legacy_zero_ports_show_unknown_without_inventing_firewall_guidance() {
    for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
        let mut status = snapshot(role, 0);
        status.sessions[0].local_udp_port = 0;
        status.sessions[0].peer_udp_port = 0;
        let vm = fixture(Lang::En, Theme::Dark, status);
        let observation =
            AudioPathObservation::from_wire(&vm.audio.status.as_ref().unwrap().sessions[0])
                .unwrap();
        assert!(!observation.has_diagnostic());
        for lang in [Lang::En, Lang::Ru] {
            assert!(observation
                .ports(lang, PEER)
                .contains(text(lang, Key::Unknown2)));
            assert!(observation.guidance(lang, role, LOCAL, PEER).is_none());
            let mut known = observation;
            known.diagnostic = ipc::AudioPathDiagnostic::NoAuthenticatedPeerPackets;
            assert!(known.guidance(lang, role, LOCAL, PEER).is_none());
        }
        assert!(vm.audio_player.cards.is_empty());
    }
}

#[test]
fn degradation_requires_path_evidence_and_failed_listener_never_enables_media_commands() {
    for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
        for diagnostic in 0..=4 {
            for health in [
                ipc::AudioStreamHealth::Ready,
                ipc::AudioStreamHealth::Degraded,
            ] {
                let mut status = snapshot(role, diagnostic);
                status.sessions[0].state = ipc::AudioSessionState::Running as i32;
                status.sessions[0].health_state = health as i32;
                let model = bifrost_gui::audio_wire::decode_status(&status).unwrap();
                let expected = diagnostic != 0 && health == ipc::AudioStreamHealth::Degraded;
                let observation = AudioPathObservation::from_wire(&status.sessions[0]);
                assert_eq!(observation.is_some(), expected);
                assert_eq!(
                    AudioPathObservation::from_model(&model.sessions[0]),
                    observation
                );
                let vm = fixture(Lang::En, Theme::Dark, status);
                if role == ipc::AudioRole::Listener {
                    assert_eq!(vm.audio_player.cards[0].path.is_some(), expected);
                }
            }
            let vm = fixture(Lang::En, Theme::Dark, snapshot(role, diagnostic));
            assert_eq!(
                vm.audio_player.cards.len(),
                usize::from(role == ipc::AudioRole::Listener && diagnostic != 0)
            );
            if let Some(card) = vm.audio_player.cards.first() {
                assert_eq!(card.state, StreamState::Failed);
                assert_eq!(card.source_name, PEER);
                assert_eq!(
                    card.path,
                    AudioPathObservation::from_wire(&vm.audio.status.as_ref().unwrap().sessions[0])
                );
                assert!(!card.has_media_session && !card.playing);
                assert!(!card.can_prev && !card.can_play_pause && !card.can_next);
                let status = vm.audio.status.as_ref().unwrap();
                for command in [
                    AudioPlayerAction::Previous(card.stream_id.clone()),
                    AudioPlayerAction::PlayPause(card.stream_id.clone()),
                    AudioPlayerAction::Next(card.stream_id.clone()),
                ] {
                    assert!(audio::player_request(status, &command).is_none());
                }
            }
        }
    }
}

#[test]
fn settings_and_popup_show_real_failed_path_in_both_languages_themes_and_widths() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Dark, Theme::Light] {
            for width in [320.0, 760.0] {
                for role in [ipc::AudioRole::Source, ipc::AudioRole::Listener] {
                    let vm = fixture(lang, theme, snapshot(role, 1));
                    let before = vm.audio.draft.clone();
                    let observation = AudioPathObservation::from_wire(
                        &vm.audio.status.as_ref().unwrap().sessions[0],
                    )
                    .unwrap();
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.enable_accesskit();
                    frame(&ctx, &vm, width, false);
                    let output = frame(&ctx, &vm, width, false);
                    let values = labels(&output);
                    for expected in [
                        observation.reason(lang, PEER),
                        observation.ports(lang, PEER),
                        observation.guidance(lang, role, LOCAL, PEER).unwrap(),
                    ] {
                        assert!(
                            values.iter().any(|(label, _)| label.contains(&expected)),
                            "missing {expected}: {lang:?}/{theme:?}/{width}/{role:?}"
                        );
                    }
                    for (label, rect) in &values {
                        assert!(!label.contains(STALE_NAME));
                        assert!(!label.contains(support::PEER_NODE));
                        assert!(!label.contains(support::SELF_NODE));
                        if label.contains(&observation.reason(lang, PEER))
                            || label.contains(&observation.ports(lang, PEER))
                        {
                            assert!(
                                rect.min.x >= 0.0 && rect.max.x <= width + 1.0,
                                "clipped {label}: {rect:?}"
                            );
                        }
                    }
                    assert_eq!(vm.audio.draft, before);
                    if role == ipc::AudioRole::Listener {
                        let ctx = egui::Context::default();
                        ui::install_fonts(&ctx);
                        ctx.enable_accesskit();
                        frame(&ctx, &vm, width, true);
                        let output = frame(&ctx, &vm, width, true);
                        let values = labels(&output);
                        assert!(values.iter().any(|(s, _)| s.contains(PEER)));
                        assert!(values
                            .iter()
                            .any(|(s, _)| s.contains(text(lang, Key::AudioPathUnavailable))));
                        assert!(
                            !values
                                .iter()
                                .any(|(s, _)| s.contains(STALE_NAME)
                                    || s.contains(support::PEER_NODE))
                        );
                        let tree = output
                            .platform_output
                            .accesskit_update
                            .expect("popup accessibility");
                        let tooltip = observation.tooltip(lang, PEER);
                        assert!(
                            tree.nodes.iter().any(|(_, node)| {
                                node.label()
                                    .or_else(|| node.value())
                                    .is_some_and(|label| label.contains(&tooltip))
                            }),
                            "popup health exposes reason and both ports"
                        );
                        for key in [
                            Key::AudioPrevious,
                            Key::AudioPlay,
                            Key::AudioPause,
                            Key::AudioNext,
                        ] {
                            assert!(!tree
                                .nodes
                                .iter()
                                .any(|(_, node)| node.label() == Some(text(lang, key))));
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn optional_offscreen_failed_path_pngs() {
    let Some(dir) = std::env::var_os("BIFROST_PATH_SHOTS") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    assert!(
        dir.is_absolute(),
        "BIFROST_PATH_SHOTS must be an absolute external directory"
    );
    let dir = std::path::absolute(dir).unwrap();
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .unwrap()
        .canonicalize()
        .unwrap();
    std::fs::create_dir_all(&dir).unwrap();
    let dir = dir.canonicalize().unwrap();
    assert!(
        !dir.starts_with(repo),
        "screenshots must stay outside the repository"
    );
    for (lang, language) in [(Lang::En, "en"), (Lang::Ru, "ru")] {
        for (theme, theme_name) in [(Theme::Light, "light"), (Theme::Dark, "dark")] {
            for (width, layout) in [(320.0, "narrow"), (760.0, "wide")] {
                for (role, role_name) in [
                    (ipc::AudioRole::Source, "source"),
                    (ipc::AudioRole::Listener, "listener"),
                ] {
                    let vm = fixture(lang, theme, snapshot(role, 1));
                    let mut harness = egui_kittest::Harness::builder()
                        .with_size(egui::vec2(width, 900.0))
                        .wgpu()
                        .build_ui(move |ui| {
                            install_shot_style(ui, theme);
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
                            "path-settings-failed-{role_name}-{language}-{theme_name}-{layout}.png"
                        )))
                        .unwrap();
                }
            }
            let vm = fixture(lang, theme, snapshot(ipc::AudioRole::Listener, 1));
            let size = egui::vec2(320.0, audio_player::height(&vm.audio_player.cards));
            let mut harness = egui_kittest::Harness::builder()
                .with_size(size)
                .wgpu()
                .build_ui(move |ui| {
                    install_shot_style(ui, theme);
                    audio_player::draw(ui, &vm.audio_player.cards, theme, lang);
                });
            harness.run_steps(3);
            harness
                .render()
                .unwrap()
                .save(dir.join(format!(
                    "path-popup-failed-listener-{language}-{theme_name}-320.png"
                )))
                .unwrap();
        }
    }
}

fn install_shot_style(ui: &mut egui::Ui, theme: Theme) {
    let id = egui::Id::new("path-shot-fonts");
    if !ui
        .data_mut(|data| data.get_temp::<bool>(id))
        .unwrap_or(false)
    {
        ui::install_fonts(ui.ctx());
        ui.data_mut(|data| data.insert_temp(id, true));
    }
    ui::apply_theme(ui.ctx(), &Palette::of(theme));
    ui.set_style(ui.ctx().global_style());
}

#[test]
fn unknown_listener_port_does_not_hide_a_known_role_specific_rule() {
    let mut observation = AudioPathObservation {
        diagnostic: ipc::AudioPathDiagnostic::NoAuthenticatedPeerPackets,
        local_udp_port: 0,
        peer_udp_port: 61000,
    };
    for lang in [Lang::En, Lang::Ru] {
        assert!(observation
            .guidance(lang, ipc::AudioRole::Listener, LOCAL, PEER)
            .is_none());
        assert!(observation
            .guidance(lang, ipc::AudioRole::Source, LOCAL, PEER)
            .is_some());
        assert!(observation
            .guidance(lang, ipc::AudioRole::Unspecified, LOCAL, PEER)
            .is_none());
    }
    observation.local_udp_port = 47903;
    observation.peer_udp_port = 0;
    for lang in [Lang::En, Lang::Ru] {
        assert!(observation
            .guidance(lang, ipc::AudioRole::Listener, LOCAL, PEER)
            .is_some());
        assert!(observation
            .guidance(lang, ipc::AudioRole::Source, LOCAL, PEER)
            .is_none());
    }
}

#[test]
fn settings_render_legacy_unknown_and_each_typed_reason_without_stale_identity() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for diagnostic in 0..=4 {
                for degraded in [false, true] {
                    let mut status = snapshot(ipc::AudioRole::Listener, diagnostic);
                    if diagnostic == 0 {
                        status.sessions[0].local_udp_port = 0;
                        status.sessions[0].peer_udp_port = 0;
                    }
                    if degraded {
                        status.sessions[0].state = ipc::AudioSessionState::Running as i32;
                    }
                    let observation = AudioPathObservation::from_wire(&status.sessions[0]);
                    let vm = fixture(lang, theme, status);
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    frame(&ctx, &vm, 320.0, false);
                    let output = frame(&ctx, &vm, 320.0, false);
                    let values = labels(&output);
                    assert!(!values
                        .iter()
                        .any(|(s, _)| s.contains(STALE_NAME) || s.contains(support::PEER_NODE)));
                    if let Some(observation) = observation {
                        assert!(values
                            .iter()
                            .any(|(s, _)| s.contains(&observation.reason(lang, PEER))));
                        assert!(values
                            .iter()
                            .any(|(s, _)| s.contains(&observation.ports(lang, PEER))));
                    }
                    let guidance = fill_named(
                        lang,
                        Key::AudioPathFirewall,
                        &[("port", &"47903"), ("listener", &LOCAL), ("source", &PEER)],
                    );
                    assert_eq!(
                        values.iter().any(|(s, _)| s.contains(&guidance)),
                        diagnostic != 0
                    );
                }
            }
        }
    }
}
