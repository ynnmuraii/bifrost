//! Display/GPU-free player behavior and production viewport bounds coverage.
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
        seek: None,
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
    draw_frame_at_width(ctx, cards, theme, lang, 240.0, events)
}

fn draw_frame_at_width(
    ctx: &egui::Context,
    cards: &[AudioPlayerCard],
    theme: Theme,
    lang: Lang,
    width: f32,
    events: Vec<egui::Event>,
) -> (egui::FullOutput, Vec<AudioPlayerAction>) {
    let mut actions = Vec::new();
    let mut output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(width, audio_player::height(cards)),
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

#[test]
fn player_volume_is_visible_bounded_and_operable_without_a_hover_popup() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Light, Theme::Dark] {
            for width in [225.0, 240.0, 400.0] {
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                ctx.enable_accesskit();
                let cards = fixtures("system");
                let client = egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, audio_player::height(&cards)),
                );
                let frame = |events| {
                    let mut actions = Vec::new();
                    let output = ctx.run_ui(
                        egui::RawInput {
                            screen_rect: Some(client),
                            events,
                            ..Default::default()
                        },
                        |ui| actions = audio_player::draw(ui, &cards, theme, lang),
                    );
                    (output, actions)
                };
                frame(Vec::new()).0.drop_without_applying_deltas();
                let (output, actions) = frame(Vec::new());
                assert!(actions.is_empty());
                let tree = output.platform_output.accesskit_update.as_ref().unwrap();
                let slider = tree
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.role() == egui::accesskit::Role::Slider
                            && node.label() == Some(text(lang, Key::AudioGain))
                    })
                    .expect("volume slider must be available without hovering");
                let bounds = slider.1.bounds().expect("volume hit target");
                let rect = egui::Rect::from_min_max(
                    egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
                    egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
                );
                assert!(
                    client.shrink(8.0).contains_rect(rect),
                    "{rect:?} {client:?}"
                );
                assert!(rect.width() >= 48.0 && rect.height() >= 20.0, "{rect:?}");
                let (percent, clip) =
                    text_geometry(&output.shapes, "75%").expect("visible numeric volume");
                assert!(client.shrink(8.0).contains_rect(percent), "{percent:?}");
                assert!(
                    clip.contains_rect(percent),
                    "clipped percentage: {percent:?} {clip:?}"
                );
                assert!(
                    rect.right() <= percent.left(),
                    "slider and percentage overlap"
                );
                output.drop_without_applying_deltas();
                let at = egui::pos2(rect.right() - 1.0, rect.center().y);
                let (_, actions) = frame(vec![
                    egui::Event::PointerMoved(at),
                    egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: true,
                        modifiers: Default::default(),
                    },
                ]);
                assert_eq!(
                    actions,
                    vec![AudioPlayerAction::SetGain {
                        stream_id: "stream-1".into(),
                        percent: 100,
                    }]
                );
                frame(vec![egui::Event::PointerButton {
                    pos: at,
                    button: egui::PointerButton::Primary,
                    pressed: false,
                    modifiers: Default::default(),
                }])
                .0
                .drop_without_applying_deltas();
            }
        }
    }
}

#[test]
fn active_player_has_a_distinct_hide_action_without_stopping_the_stream() {
    for lang in [Lang::En, Lang::Ru] {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        ctx.enable_accesskit();
        let cards = fixtures("system");
        draw_frame(&ctx, &cards, Theme::Dark, lang, Vec::new());
        let (output, _) = draw_frame(&ctx, &cards, Theme::Dark, lang, Vec::new());
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        let hide = tree
            .nodes
            .iter()
            .find(|(_, node)| {
                node.role() == egui::accesskit::Role::Button
                    && node.label() == Some(text(lang, Key::AudioHide))
            })
            .expect("an active player needs a separate hide control");
        let bounds = hide.1.bounds().unwrap();
        let at = egui::pos2(
            ((bounds.x0 + bounds.x1) / 2.0) as f32,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        output.drop_without_applying_deltas();
        let (_, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            lang,
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
            ],
        );
        assert_eq!(actions, vec![AudioPlayerAction::HidePlayer]);
    }
}

#[test]
fn player_header_press_starts_native_drag_without_stealing_controls() {
    for width in [225.0, 240.0] {
        for lang in [Lang::En, Lang::Ru] {
            for theme in [Theme::Dark, Theme::Light] {
                for target in [
                    "header-edge",
                    "header-text",
                    "settings",
                    "hide",
                    "stop",
                    "gain",
                ] {
                    let ctx = egui::Context::default();
                    ui::install_fonts(&ctx);
                    ctx.enable_accesskit();
                    let cards = fixtures("system");
                    let draw =
                        |events| draw_frame_at_width(&ctx, &cards, theme, lang, width, events);
                    draw(Vec::new());
                    let (output, _) = draw(Vec::new());
                    let at = if target == "header-edge" {
                        egui::pos2(50.0, 4.0)
                    } else if target == "header-text" {
                        visible_text_rect(&output.shapes, "WORKSTATION")
                            .expect("visible source title")
                            .center()
                    } else {
                        let key = match target {
                            "settings" => Key::AudioSettings,
                            "hide" => Key::AudioHide,
                            "stop" => Key::AudioStop,
                            _ => Key::AudioGain,
                        };
                        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
                        let bounds = tree
                            .nodes
                            .iter()
                            .find(|(_, node)| node.label() == Some(text(lang, key)))
                            .unwrap()
                            .1
                            .bounds()
                            .unwrap();
                        egui::pos2(
                            ((bounds.x0 + bounds.x1) / 2.0) as f32,
                            ((bounds.y0 + bounds.y1) / 2.0) as f32,
                        )
                    };
                    output.drop_without_applying_deltas();
                    let (pressed, mut actions) = draw(vec![
                        egui::Event::PointerMoved(at),
                        egui::Event::PointerButton {
                            pos: at,
                            button: egui::PointerButton::Primary,
                            pressed: true,
                            modifiers: Default::default(),
                        },
                    ]);
                    let native_drag = pressed.viewport_output.values().any(|viewport| {
                        viewport
                            .commands
                            .iter()
                            .any(|command| matches!(command, egui::ViewportCommand::StartDrag))
                    });
                    assert_eq!(
                        native_drag,
                        target.starts_with("header"),
                        "{target} {lang:?} {width}"
                    );
                    pressed.drop_without_applying_deltas();
                    let (_, released) = draw(vec![egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    }]);
                    actions.extend(released);
                    match target {
                        "header-edge" | "header-text" => assert!(actions.is_empty()),
                        "settings" => assert_eq!(actions, vec![AudioPlayerAction::OpenSettings]),
                        "hide" => assert_eq!(actions, vec![AudioPlayerAction::HidePlayer]),
                        "stop" => {
                            assert_eq!(actions, vec![AudioPlayerAction::Stop("stream-1".into())])
                        }
                        _ => assert!(!actions.is_empty() && actions.iter().all(|action|
                            matches!(action, AudioPlayerAction::SetGain { stream_id, .. }
                                if stream_id == "stream-1")), "{actions:?}"),
                    }
                }
            }
        }
    }
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
        assert!(audio_player::height(&cards) < audio_player::height(&fixtures("single")));
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

#[test]
fn empty_player_message_drags_and_close_only_hides() {
    for lang in [Lang::En, Lang::Ru] {
        for theme in [Theme::Dark, Theme::Light] {
            for hide in [false, true] {
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                ctx.enable_accesskit();
                draw_frame(&ctx, &[], theme, lang, Vec::new());
                let (output, _) = draw_frame(&ctx, &[], theme, lang, Vec::new());
                let at = if hide {
                    let tree = output.platform_output.accesskit_update.as_ref().unwrap();
                    let bounds = tree
                        .nodes
                        .iter()
                        .find(|(_, node)| node.label() == Some(text(lang, Key::AudioHide)))
                        .unwrap()
                        .1
                        .bounds()
                        .unwrap();
                    egui::pos2(
                        ((bounds.x0 + bounds.x1) / 2.0) as f32,
                        ((bounds.y0 + bounds.y1) / 2.0) as f32,
                    )
                } else {
                    visible_text_rect(&output.shapes, text(lang, Key::AudioEmpty))
                        .expect("visible empty-state message")
                        .center()
                };
                output.drop_without_applying_deltas();
                let (pressed, mut actions) = draw_frame(
                    &ctx,
                    &[],
                    theme,
                    lang,
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
                assert_eq!(
                    pressed.viewport_output.values().any(|viewport| viewport
                        .commands
                        .iter()
                        .any(|command| matches!(command, egui::ViewportCommand::StartDrag))),
                    !hide,
                    "empty message must not select text instead of dragging"
                );
                pressed.drop_without_applying_deltas();
                let (_, released) = draw_frame(
                    &ctx,
                    &[],
                    theme,
                    lang,
                    vec![egui::Event::PointerButton {
                        pos: at,
                        button: egui::PointerButton::Primary,
                        pressed: false,
                        modifiers: Default::default(),
                    }],
                );
                actions.extend(released);
                assert_eq!(
                    actions,
                    if hide {
                        vec![AudioPlayerAction::HidePlayer]
                    } else {
                        Vec::new()
                    }
                );
            }
        }
    }
}

fn visible_text_rect(shapes: &[egui::epaint::ClippedShape], value: &str) -> Option<egui::Rect> {
    text_geometry(shapes, value).map(|(bounds, clip)| bounds.intersect(clip))
}

fn text_geometry(
    shapes: &[egui::epaint::ClippedShape],
    value: &str,
) -> Option<(egui::Rect, egui::Rect)> {
    fn find(shape: &Shape, value: &str) -> Option<egui::Rect> {
        match shape {
            Shape::Text(text) if text.galley.text() == value => {
                Some(egui::Rect::from_min_size(text.pos, text.galley.size()))
            }
            Shape::Vec(shapes) => shapes.iter().find_map(|shape| find(shape, value)),
            _ => None,
        }
    }
    shapes
        .iter()
        .find_map(|shape| find(&shape.shape, value).map(|bounds| (bounds, shape.clip_rect)))
}

#[test]
fn player_volume_track_has_control_contrast_in_the_production_theme() {
    fn luminance(color: egui::Color32) -> f64 {
        let linear = |value: u8| {
            let value = f64::from(value) / 255.0;
            if value <= 0.04045 {
                value / 12.92
            } else {
                ((value + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * linear(color.r()) + 0.7152 * linear(color.g()) + 0.0722 * linear(color.b())
    }
    for theme in [Theme::Dark, Theme::Light] {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        ui::apply_theme(&ctx, &ui::Palette::of(theme));
        ctx.enable_accesskit();
        let cards = fixtures("system");
        draw_frame(&ctx, &cards, theme, Lang::En, Vec::new());
        let (output, _) = draw_frame(&ctx, &cards, theme, Lang::En, Vec::new());
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(Lang::En, Key::AudioGain)))
            .unwrap()
            .1
            .bounds()
            .unwrap();
        let slider = egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        );
        output.drop_without_applying_deltas();
        for events in [
            Vec::new(),
            vec![egui::Event::PointerMoved(slider.center())],
            vec![egui::Event::PointerButton {
                pos: slider.center(),
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: Default::default(),
            }],
        ] {
            let (output, _) = draw_frame(&ctx, &cards, theme, Lang::En, events);
            let card = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    Shape::Rect(rect) if rect.rect.height() > 100.0 => Some(rect.fill),
                    _ => None,
                })
                .expect("painted card surface");
            let rail = output
                .shapes
                .iter()
                .find_map(|shape| match &shape.shape {
                    Shape::Rect(rect)
                        if rect.rect.width() >= 48.0
                            && rect.rect.height() <= 12.0
                            && slider.contains_rect(rect.rect) =>
                    {
                        Some(rect.fill)
                    }
                    _ => None,
                })
                .expect("painted volume rail inside the actual slider");
            let (a, b) = (luminance(card), luminance(rail));
            let contrast = (a.max(b) + 0.05) / (a.min(b) + 0.05);
            assert!(
                contrast >= 3.0,
                "{theme:?}: rail/card contrast {contrast}:1"
            );
            output.drop_without_applying_deltas();
        }
    }
}

#[test]
fn track_click_seeks_once_on_release_and_cancels_changed_media() {
    use bifrost_gui::audio_player::AudioSeekTarget;
    let target = AudioSeekTarget {
        session_token: 7,
        media_revision: 3,
        min_ms: 10_000,
        max_ms: 90_000,
    };
    for change in [
        "none",
        "position-only",
        "token",
        "revision",
        "minimum",
        "maximum",
        "stream",
        "removed",
        "removed-then-return",
        "escape",
        "focus-loss",
        "capability",
        "capability-then-return",
    ] {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        ctx.enable_accesskit();
        let mut media = card("seek-source");
        media.position_ms = Some(25_000);
        media.duration_ms = Some(100_000);
        media.seek = Some(target);
        let mut cards = vec![media];
        draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        let (output, _) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        let tree = output.platform_output.accesskit_update.as_ref().unwrap();
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(Lang::En, Key::AudioSeek)))
            .expect("a permitted native timeline must be clickable")
            .1
            .bounds()
            .unwrap();
        let at = egui::pos2(
            bounds.x1 as f32 - 1.0,
            ((bounds.y0 + bounds.y1) / 2.0) as f32,
        );
        output.drop_without_applying_deltas();
        let (output, pressed) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
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
        assert!(pressed.is_empty(), "seek must not be emitted at press");
        output.drop_without_applying_deltas();
        let (output, held) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        assert!(
            held.is_empty(),
            "holding a click must not queue media commands"
        );
        output.drop_without_applying_deltas();
        match change {
            "position-only" => cards[0].position_ms = Some(26_000),
            "token" => cards[0].seek.as_mut().unwrap().session_token += 1,
            "revision" => cards[0].seek.as_mut().unwrap().media_revision += 1,
            "minimum" => cards[0].seek.as_mut().unwrap().min_ms += 1,
            "maximum" => cards[0].seek.as_mut().unwrap().max_ms -= 1,
            "stream" => cards[0].stream_id = "replacement".into(),
            "removed" => cards.clear(),
            "capability" => cards[0].seek = None,
            "capability-then-return" => {
                cards[0].seek = None;
                let (output, actions) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
                assert!(actions.is_empty());
                output.drop_without_applying_deltas();
                cards[0].seek = Some(target);
            }
            "removed-then-return" => {
                let (output, actions) = draw_frame(&ctx, &[], Theme::Dark, Lang::En, Vec::new());
                assert!(actions.is_empty());
                output.drop_without_applying_deltas();
            }
            "escape" => {
                let (output, actions) = draw_frame(
                    &ctx,
                    &cards,
                    Theme::Dark,
                    Lang::En,
                    vec![egui::Event::Key {
                        key: egui::Key::Escape,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Default::default(),
                    }],
                );
                assert!(actions.is_empty());
                output.drop_without_applying_deltas();
            }
            "focus-loss" => {
                let output = ctx.run_ui(
                    egui::RawInput {
                        screen_rect: Some(egui::Rect::from_min_size(
                            egui::Pos2::ZERO,
                            egui::vec2(240.0, audio_player::height(&cards)),
                        )),
                        focused: false,
                        ..Default::default()
                    },
                    |ui| {
                        assert!(audio_player::draw(ui, &cards, Theme::Dark, Lang::En).is_empty());
                    },
                );
                output.drop_without_applying_deltas();
            }
            _ => {}
        }
        let (output, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
            vec![egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: Default::default(),
            }],
        );
        output.drop_without_applying_deltas();
        let expected = if matches!(change, "none" | "position-only") {
            vec![AudioPlayerAction::Seek {
                stream_id: "seek-source".into(),
                target,
                position_ms: 90_000,
            }]
        } else {
            Vec::new()
        };
        assert_eq!(actions, expected, "{change}");
    }
}

#[test]
fn seek_intent_debug_redacts_private_timeline_and_range() {
    let action = AudioPlayerAction::Seek {
        stream_id: "opaque-stream".into(),
        target: bifrost_gui::audio_player::AudioSeekTarget {
            session_token: 7,
            media_revision: 3,
            min_ms: 123_456,
            max_ms: 987_654,
        },
        position_ms: 456_789,
    };
    let debug = format!("{action:?}");
    assert!(debug.contains("Seek") && debug.contains("[REDACTED]"));
    for private in ["123456", "987654", "456789"] {
        assert!(
            !debug.contains(private),
            "private seek metadata must not enter Debug output"
        );
    }
}

#[test]
fn read_only_timeline_explains_unavailability_without_seek_cursor_or_actions() {
    for lang in [Lang::Ru, Lang::En] {
        for theme in [Theme::Dark, Theme::Light] {
            for width in [225.0, 240.0] {
                let ctx = egui::Context::default();
                ui::install_fonts(&ctx);
                ctx.enable_accesskit();
                let mut media = card("read-only");
                media.title = None;
                media.position_ms = Some(25_000);
                media.duration_ms = Some(100_000);
                let cards = vec![media];
                draw_frame_at_width(&ctx, &cards, theme, lang, width, Vec::new());
                let (output, _) = draw_frame_at_width(&ctx, &cards, theme, lang, width, Vec::new());
                let tree = output.platform_output.accesskit_update.as_ref().unwrap();
                let node = &tree
                    .nodes
                    .iter()
                    .find(|(_, node)| {
                        node.role() == egui::accesskit::Role::Label
                            && node.supports_action(egui::accesskit::Action::Focus)
                            && !node.supports_action(egui::accesskit::Action::Click)
                    })
                    .expect("progress is accessible without pretending to support seek")
                    .1;
                let bounds = node.bounds().unwrap();
                let at = egui::pos2(
                    ((bounds.x0 + bounds.x1) / 2.0) as f32,
                    ((bounds.y0 + bounds.y1) / 2.0) as f32,
                );
                output.drop_without_applying_deltas();
                for events in [
                    vec![egui::Event::PointerMoved(at)],
                    vec![
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
                    ],
                ] {
                    let (output, actions) =
                        draw_frame_at_width(&ctx, &cards, theme, lang, width, events);
                    assert!(
                        actions.is_empty(),
                        "read-only progress must not issue commands"
                    );
                    assert_ne!(
                        output.platform_output.cursor_icon,
                        egui::CursorIcon::PointingHand
                    );
                    output.drop_without_applying_deltas();
                }
            }
        }
    }
}

#[test]
fn seek_keyboard_release_uses_the_same_captured_identity_and_cancels_capability_changes() {
    use egui::accesskit::{Action, ActionRequest};
    for change in ["none", "revision", "capability"] {
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        ctx.enable_accesskit();
        let mut media = card("keyboard-seek");
        media.position_ms = Some(25_000);
        media.duration_ms = Some(100_000);
        media.seek = Some(bifrost_gui::audio_player::AudioSeekTarget {
            session_token: 7,
            media_revision: 3,
            min_ms: 10_000,
            max_ms: 90_000,
        });
        let target = media.seek.unwrap();
        let mut cards = vec![media];
        draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        let (output, _) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
        let node = output
            .platform_output
            .accesskit_update
            .as_ref()
            .unwrap()
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(Lang::En, Key::AudioSeek)))
            .unwrap()
            .0;
        output.drop_without_applying_deltas();
        let (output, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action: Action::Focus,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: node,
                data: None,
            })],
        );
        assert!(actions.is_empty());
        output.drop_without_applying_deltas();
        let (output, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
            vec![egui::Event::Key {
                key: egui::Key::ArrowRight,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: Default::default(),
            }],
        );
        assert!(
            actions.is_empty(),
            "keyboard seek is emitted only on release"
        );
        output.drop_without_applying_deltas();
        match change {
            "revision" => cards[0].seek.as_mut().unwrap().media_revision += 1,
            "capability" => cards[0].seek = None,
            _ => {}
        }
        let (output, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
            vec![egui::Event::Key {
                key: egui::Key::ArrowRight,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: Default::default(),
            }],
        );
        output.drop_without_applying_deltas();
        let expected = if change == "none" {
            vec![AudioPlayerAction::Seek {
                stream_id: "keyboard-seek".into(),
                target,
                position_ms: 30_000,
            }]
        } else {
            Vec::new()
        };
        assert_eq!(actions, expected, "{change}");
    }
}

#[test]
fn seek_accessibility_actions_have_real_value_semantics_and_reject_nonfinite_values() {
    use egui::accesskit::{Action, ActionData, ActionRequest};
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    let mut media = card("accessible-seek");
    media.position_ms = Some(25_000);
    media.duration_ms = Some(100_000);
    media.seek = Some(bifrost_gui::audio_player::AudioSeekTarget {
        session_token: 7,
        media_revision: 3,
        min_ms: 10_000,
        max_ms: 90_000,
    });
    let target = media.seek.unwrap();
    let cards = vec![media];
    draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
    let (output, _) = draw_frame(&ctx, &cards, Theme::Dark, Lang::En, Vec::new());
    let (node_id, node) = output
        .platform_output
        .accesskit_update
        .as_ref()
        .unwrap()
        .nodes
        .iter()
        .find(|(_, node)| node.label() == Some(text(Lang::En, Key::AudioSeek)))
        .unwrap();
    let node_id = *node_id;
    assert_eq!(node.role(), egui::accesskit::Role::Slider);
    assert_eq!(node.numeric_value(), Some(25.0));
    assert!(node.supports_action(Action::SetValue));
    assert!(
        !node.supports_action(Action::Click),
        "no unsupported button activation"
    );
    output.drop_without_applying_deltas();
    for (action, data, expected) in [
        (
            Action::SetValue,
            Some(ActionData::NumericValue(42.0)),
            Some(42_000),
        ),
        (
            Action::SetValue,
            Some(ActionData::NumericValue(200.0)),
            Some(90_000),
        ),
        (
            Action::SetValue,
            Some(ActionData::NumericValue(f64::NAN)),
            None,
        ),
        (Action::Increment, None, Some(30_000)),
        (Action::Decrement, None, Some(20_000)),
    ] {
        let (output, actions) = draw_frame(
            &ctx,
            &cards,
            Theme::Dark,
            Lang::En,
            vec![egui::Event::AccessKitActionRequest(ActionRequest {
                action,
                target_tree: egui::accesskit::TreeId::ROOT,
                target_node: node_id,
                data,
            })],
        );
        output.drop_without_applying_deltas();
        let expected = expected
            .map(|position_ms| AudioPlayerAction::Seek {
                stream_id: "accessible-seek".into(),
                target,
                position_ms,
            })
            .into_iter()
            .collect::<Vec<_>>();
        assert_eq!(actions, expected);
        assert_eq!(
            cards[0].position_ms,
            Some(25_000),
            "accessibility does not invent observed movement"
        );
    }
}
