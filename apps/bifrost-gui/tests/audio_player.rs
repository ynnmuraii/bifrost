//! Display/GPU-free player coverage. Set BIFROST_PLAYER_SHOTS to an external directory
//! to additionally render fixture PNGs through the existing offscreen wgpu harness.
use bifrost_gui::audio_player::{
    AppKind, AudioPlayer, AudioPlayerAction, AudioPlayerCard, StreamState,
};
use bifrost_gui::egui::{self, Shape};
use bifrost_gui::gui_settings::Theme;
use bifrost_gui::ui::{self, audio_player};
use ui_text::{text, Key, Lang};

fn card(id: &str) -> AudioPlayerCard {
    AudioPlayerCard {
        stream_id: id.into(),
        source_name: "WORKSTATION".into(),
        title: Some("A quiet evening — live session".into()),
        subtitle: Some("Studio sessions".into()),
        has_media_session: true,
        app: AppKind::YouTube,
        position_ms: Some(5_000),
        duration_ms: Some(869_000),
        playing: true,
        can_prev: true,
        can_play_pause: true,
        can_next: true,
        muted: false,
        gain_percent: 75,
        state: StreamState::Ready,
        background: false,
    }
}

fn fixtures(scene: &str) -> Vec<AudioPlayerCard> {
    let mut first = card("stream-1");
    match scene {
        "stacked" => {
            let mut second = card("stream-2");
            second.source_name = "LAPTOP".into();
            second.title = Some(
                "A deliberately long title that must be ellipsized without touching controls"
                    .into(),
            );
            second.state = StreamState::Buffering;
            second.background = true;
            let mut third = card("stream-3");
            third.source_name = "HTPC".into();
            third.muted = true;
            third.state = StreamState::Degraded;
            third.playing = false;
            vec![first, second, third]
        }
        "system" => {
            first.title = None;
            first.subtitle = None;
            first.has_media_session = false;
            first.app = AppKind::System;
            vec![first]
        }
        "disabled" => {
            first.can_prev = false;
            first.can_play_pause = false;
            first.can_next = false;
            vec![first]
        }
        _ => vec![first],
    }
}

fn draw_frame(
    ctx: &egui::Context,
    cards: &[AudioPlayerCard],
    theme: Theme,
    lang: Lang,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, Vec<AudioPlayerAction>) {
    let mut actions = Vec::new();
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(240.0, audio_player::height(cards)),
            )),
            events,
            ..Default::default()
        },
        |ui| {
            actions = audio_player::draw(ui, cards, theme, lang);
        },
    );
    output.textures_delta.clear();
    (output, actions)
}

fn texts(shapes: &[egui::epaint::ClippedShape]) -> Vec<String> {
    fn visit(shape: &Shape, values: &mut Vec<String>) {
        match shape {
            Shape::Text(t) => values.push(t.galley.text().into()),
            Shape::Vec(children) => {
                for s in children {
                    visit(s, values);
                }
            }
            _ => {}
        }
    }
    let mut values = Vec::new();
    for s in shapes {
        visit(&s.shape, &mut values);
    }
    values
}

#[test]
fn cards_render_in_both_languages_and_themes_without_a_display() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for scene in ["single", "stacked", "system", "disabled"] {
                let cards = fixtures(scene);
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                ctx.enable_accesskit();
                draw_frame(&ctx, &cards, theme, lang, Vec::new());
                let (output, actions) = draw_frame(&ctx, &cards, theme, lang, Vec::new());
                assert!(actions.is_empty());
                let values = texts(&output.shapes);
                if scene == "system" {
                    assert!(values.iter().any(|s| s == "WORKSTATION"));
                    assert!(values.iter().any(|s| s == text(lang, Key::AudioSystem)));
                    assert!(!values.iter().any(|s| s == "0:05"));
                } else {
                    assert!(values.iter().any(|s| s == "0:05"));
                    assert!(values.iter().any(|s| s == "14:29"));
                    assert!(values.iter().any(|s| s.contains("WORKSTATION")));
                }
                let tree = output
                    .platform_output
                    .accesskit_update
                    .expect("AccessKit update");
                assert!(tree
                    .nodes
                    .iter()
                    .any(|(_, n)| n.label() == Some(text(lang, Key::AudioStop))));
                for key in [Key::AudioPrevious, Key::AudioPause, Key::AudioNext] {
                    assert_eq!(
                        tree.nodes
                            .iter()
                            .any(|(_, n)| n.label() == Some(text(lang, key))),
                        scene != "system",
                    );
                }
            }
        }
    }
}

#[test]
fn disabled_media_controls_never_emit_commands_but_local_mute_does() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let cards = fixtures("disabled");
    draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
    // Default run_ui margin = 8; compact card controls start 87 points below the card top.
    for (x, expected) in [
        (68.5, None),
        (115.0, None),
        (161.5, None),
        (
            204.0,
            Some(AudioPlayerAction::SetMuted {
                stream_id: "stream-1".into(),
                muted: true,
            }),
        ),
    ] {
        let at = egui::pos2(x, 109.0);
        let events = vec![
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
        ];
        let (_, actions) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, events);
        assert_eq!(actions, expected.into_iter().collect::<Vec<_>>());
    }
}

#[test]
fn authoritative_snapshots_open_only_on_first_stream_transition() {
    let mut player = AudioPlayer::default();
    assert!(!player.open);
    player.replace_cards(vec![card("one")]);
    assert!(player.open);
    player.open = false;
    player.replace_cards(vec![card("one"), card("two")]);
    assert!(!player.open);
    player.replace_cards(Vec::new());
    player.replace_cards(vec![card("three")]);
    assert!(player.open);
}

#[test]
fn zero_or_unknown_duration_hides_progress_without_hiding_session_controls() {
    for duration in [Some(0), None] {
        let mut card = card("live");
        card.duration_ms = duration;
        let cards = [card];
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        ctx.enable_accesskit();
        draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        let (output, _) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        assert!(!texts(&output.shapes)
            .iter()
            .any(|value| value == "0:05" || value == "0:00"));
        assert_eq!(audio_player::height(&cards), 120.0);
        assert!(output
            .platform_output
            .accesskit_update
            .unwrap()
            .nodes
            .iter()
            .any(|(_, node)| node.label() == Some(text(Lang::En, Key::AudioPause))));
    }
}

#[test]
fn optional_offscreen_fixture_pngs() {
    let Some(dir) = std::env::var_os("BIFROST_PLAYER_SHOTS") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    for (lang, lang_tag) in [(Lang::En, "en"), (Lang::Ru, "ru")] {
        for (theme, theme_tag) in [(Theme::Dark, "dark"), (Theme::Light, "light")] {
            for scene in ["single", "stacked", "system", "disabled", "wide"] {
                let cards = fixtures(scene);
                let size = egui::vec2(
                    if scene == "wide" { 500.0 } else { 240.0 },
                    audio_player::height(&cards),
                );
                let mut harness = egui_kittest::Harness::builder()
                    .with_size(size)
                    .wgpu()
                    .build_ui(move |ui| {
                        audio_player::draw(ui, &cards, theme, lang);
                    });
                harness.run_steps(3);
                harness
                    .render()
                    .unwrap()
                    .save(dir.join(format!("player-{scene}-{lang_tag}-{theme_tag}.png")))
                    .unwrap();
            }
        }
    }
}
