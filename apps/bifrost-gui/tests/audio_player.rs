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
        path: None,
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
fn system_audio_app_glyph_is_distinct_from_volume_without_changing_media_cards() {
    fn monitors(shape: &Shape) -> usize {
        match shape {
            Shape::Rect(rect) if rect.rect.width() == 14.0 && rect.rect.height() == 9.0 => 1,
            Shape::Vec(children) => children.iter().map(monitors).sum(),
            _ => 0,
        }
    }
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for media in [true, false] {
                let mut card = card("system");
                card.app = AppKind::System;
                card.has_media_session = media;
                if !media {
                    card.title = None;
                    card.subtitle = None;
                }
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                draw_frame(&ctx, &[card.clone()], theme, lang, Vec::new());
                let (output, _) = draw_frame(&ctx, &[card], theme, lang, Vec::new());
                assert_eq!(
                    output
                        .shapes
                        .iter()
                        .map(|shape| monitors(&shape.shape))
                        .sum::<usize>(),
                    usize::from(!media),
                );
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
    player.replace_cards(vec![card("one")], true);
    assert!(player.open);
    player.open = false;
    player.replace_cards(vec![card("one"), card("two")], true);
    assert!(!player.open);
    player.replace_cards(Vec::new(), false);
    player.replace_cards(vec![card("three")], true);
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
        assert_eq!(audio_player::height(&cards), 128.0);
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

#[test]
fn production_viewport_keeps_empty_and_three_cards_inside_opaque_client_at_native_scales() {
    use std::cell::RefCell;
    use std::rc::Rc;

    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for scale in [1.0, 1.5, 2.0] {
                for count in [0, 1, 3] {
                    let cards = (0..count)
                        .map(|i| card(&format!("viewport-{i}")))
                        .collect::<Vec<_>>();
                    let mut player = AudioPlayer::default();
                    player.cards = cards;
                    player.open = true;
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.set_embed_viewports(false);
                    ctx.enable_accesskit();
                    let frames = Rc::new(RefCell::new(Vec::new()));
                    let captured = frames.clone();
                    egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
                        let size = viewport.builder.inner_size.expect("client size");
                        assert_eq!(viewport.builder.transparent, Some(false));
                        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                        let mut input = egui::RawInput {
                            viewport_id: viewport.ids.this,
                            screen_rect: Some(rect),
                            ..Default::default()
                        };
                        input.viewports.insert(
                            viewport.ids.this,
                            egui::ViewportInfo {
                                parent: Some(viewport.ids.parent),
                                native_pixels_per_point: Some(scale),
                                monitor_size: Some(egui::vec2(1920.0, 1080.0)),
                                inner_rect: Some(rect),
                                ..Default::default()
                            },
                        );
                        let mut output = ctx.run_ui(input, |ui| (viewport.viewport_ui_cb)(ui));
                        output.textures_delta.clear();
                        captured.borrow_mut().push((rect, output));
                    });
                    for _ in 0..3 {
                        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
                            assert!(audio_player::viewport(ui.ctx(), &mut player, theme, lang)
                                .is_empty());
                        });
                        output.textures_delta.clear();
                    }
                    let frames = frames.borrow();
                    let (client, output) = frames.last().expect("real viewport callback");
                    if count > 0 {
                        assert_eq!(client.height(), audio_player::height(&player.cards));
                    }
                    fn check_text(shape: &Shape, client: egui::Rect, clip: egui::Rect) {
                        match shape {
                            Shape::Text(text) => {
                                let bounds =
                                    egui::Rect::from_min_size(text.pos, text.galley.size());
                                assert!(
                                    client.contains_rect(bounds),
                                    "text outside client: {bounds:?}"
                                );
                                assert!(
                                    clip.expand(0.5).contains_rect(bounds),
                                    "clipped text: {bounds:?} {clip:?}"
                                );
                            }
                            Shape::Vec(shapes) => {
                                for shape in shapes {
                                    check_text(shape, client, clip);
                                }
                            }
                            _ => {}
                        }
                    }
                    for shape in &output.shapes {
                        check_text(&shape.shape, *client, shape.clip_rect);
                    }
                    let values = texts(&output.shapes);
                    assert_eq!(
                        values.iter().any(|s| s == text(lang, Key::AudioEmpty)),
                        count == 0
                    );
                    assert!(!values.iter().any(|s| s == text(lang, Key::AudioHide)));
                    let tree = output
                        .platform_output
                        .accesskit_update
                        .as_ref()
                        .expect("hit targets");
                    for (_, node) in &tree.nodes {
                        if let Some(bounds) = node.bounds() {
                            if matches!(
                                node.role(),
                                egui::accesskit::Role::Button | egui::accesskit::Role::Slider
                            ) {
                                assert!(bounds.x0 >= 12.0 && bounds.y0 >= 12.0);
                                assert!(bounds.x1 <= f64::from(client.right()) - 8.0);
                                assert!(bounds.y1 <= f64::from(client.bottom()) - 8.0);
                            }
                        }
                    }
                }
            }
        }
    }
}
