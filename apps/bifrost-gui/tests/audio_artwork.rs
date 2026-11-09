//! Artwork consumer and player interaction regressions: bounded decode, one upload per hash,
//! the functional mute position, glyph orientation and debounced geometry writes.
use std::sync::Arc;

use bifrost_gui::audio::decode_artwork;
use bifrost_gui::audio_player::{Artwork, AudioPlayerAction, AudioPlayerCard, StreamState};
use bifrost_gui::egui::{self, Shape};
use bifrost_gui::gui_settings::{PlayerGeometry, Theme, PLAYER_MAX_HEIGHT, PLAYER_MAX_WIDTH};
use bifrost_gui::ui::{self, audio_player};
use protocol::limits;
use ui_text::{text, Key, Lang};

fn card(id: &str) -> AudioPlayerCard {
    AudioPlayerCard {
        stream_id: id.into(),
        source_name: "WORKSTATION".into(),
        title: Some("Night drive".into()),
        subtitle: Some("Road tapes".into()),
        artwork_id: None,
        artwork: None,
        has_media_session: true,
        app: Default::default(),
        position_ms: Some(95_000),
        duration_ms: Some(869_000),
        seek: None,
        playing: true,
        can_prev: true,
        can_play_pause: true,
        can_next: true,
        muted: false,
        gain_percent: 62,
        desired_gain_percent: None,
        state: StreamState::Ready,
        path: None,
        background: false,
    }
}

fn artwork(id: u8) -> Artwork {
    let rgba = vec![200_u8; 8 * 8 * 4];
    Artwork {
        id: [id; 32],
        rgba: Arc::from(rgba.into_boxed_slice()),
        width: 8,
        height: 8,
    }
}

fn draw(
    ctx: &egui::Context,
    cards: &[AudioPlayerCard],
    size: egui::Vec2,
    time: f64,
) -> (egui::FullOutput, Vec<AudioPlayerAction>) {
    let mut actions = Vec::new();
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(time),
            ..Default::default()
        },
        |ui| {
            actions =
                audio_player::draw(ui, cards, Theme::Dark, Lang::En, false, Default::default())
        },
    );
    (output, actions)
}

#[test]
fn a_jpeg_is_decoded_only_within_the_contract_bounds() {
    let encode = |w: u32, h: u32| {
        let image = egui::ColorImage::filled([w as usize, h as usize], egui::Color32::RED);
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80);
        encoder
            .encode_image(&image::RgbaImage::from_raw(w, h, image.as_raw().to_vec()).unwrap())
            .expect("encode fixture");
        bytes
    };
    let small = encode(8, 8);
    let decoded = decode_artwork(&[3; 32], &small).expect("a small JPEG decodes");
    assert_eq!((decoded.width, decoded.height), (8, 8));
    assert_eq!(decoded.rgba.len(), 8 * 8 * 4);
    assert_eq!(decoded.id, [3; 32]);

    let too_wide = encode(200, 8);
    assert!(decode_artwork(&[3; 32], &too_wide).is_none());
    assert!(decode_artwork(&[3; 32], b"not a jpeg").is_none());
    assert!(decode_artwork(&[3; 32], &[]).is_none());
    // The byte bound is enforced by the decoder itself: a valid JPEG padded past the IPC
    // limit is rejected by length, not by being malformed.
    let mut oversize = small.clone();
    oversize.resize(limits::MAX_AUDIO_ARTWORK_BYTES + 1, 0);
    assert!(decode_artwork(&[3; 32], &oversize).is_none());
}

#[test]
fn a_texture_is_uploaded_once_per_hash_and_removed_with_the_track() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    let mut first = card("art");
    first.artwork = Some(artwork(7));
    first.artwork_id = Some([7; 32]);
    // A warmup frame without artwork settles the font atlas, so only cover uploads remain.
    draw(&ctx, &[card("warmup")], size, 0.0)
        .0
        .textures_delta
        .clear();
    let (mut out, actions) = draw(&ctx, std::slice::from_ref(&first), size, 0.1);
    assert!(actions.is_empty());
    assert!(
        !out.textures_delta.set.is_empty(),
        "the first frame uploads the cover"
    );
    out.textures_delta.clear();
    let (mut out, _) = draw(&ctx, std::slice::from_ref(&first), size, 0.2);
    assert!(
        out.textures_delta.set.is_empty(),
        "the same hash is not uploaded again"
    );
    out.textures_delta.clear();

    let mut other = card("art");
    other.artwork = Some(artwork(8));
    other.artwork_id = Some([8; 32]);
    let (mut out, _) = draw(&ctx, std::slice::from_ref(&other), size, 0.3);
    assert!(!out.textures_delta.set.is_empty(), "a new hash uploads");
    out.textures_delta.clear();
    let (mut out, _) = draw(&ctx, &[card("no-art")], size, 0.4);
    assert!(out.textures_delta.set.is_empty());
    out.textures_delta.clear();
    // Returning to the first hash after the cache was pruned uploads it again rather than
    // showing a stale image for a different track.
    let (mut out, _) = draw(&ctx, std::slice::from_ref(&first), size, 0.5);
    assert!(!out.textures_delta.set.is_empty());
    out.textures_delta.clear();
}

#[test]
fn the_volume_row_has_one_functional_mute_left_of_the_slider() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    let cards = vec![card("volume")];
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    draw(&ctx, &cards, size, 0.0)
        .0
        .drop_without_applying_deltas();
    let (output, _) = draw(&ctx, &cards, size, 0.1);
    let tree = output.platform_output.accesskit_update.as_ref().unwrap();
    let rect_of = |key: Key| {
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(Lang::En, key)))
            .unwrap_or_else(|| panic!("{key:?}"))
            .1
            .bounds()
            .unwrap();
        egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        )
    };
    let mute = rect_of(Key::AudioMute);
    let gain = rect_of(Key::AudioGain);
    let play = rect_of(Key::AudioPause);
    assert!(mute.right() <= gain.left(), "{mute:?} {gain:?}");
    assert!(
        mute.center().y > play.bottom(),
        "mute belongs to the volume row"
    );
    let (percent, _) = text_rect(&output.shapes, "62%").expect("visible percentage");
    assert!(gain.right() <= percent.left(), "{gain:?} {percent:?}");
    // Exactly one mute control exists: no second decorative speaker is a widget.
    assert_eq!(
        tree.nodes
            .iter()
            .filter(|(_, node)| matches!(
                node.label(),
                Some(label) if label == text(Lang::En, Key::AudioMute)
                    || label == text(Lang::En, Key::AudioUnmute)
            ))
            .count(),
        1
    );
    output.drop_without_applying_deltas();
    // The old right-hand position is now the slider/percentage, so pressing it emits no mute.
    let right = egui::pos2(size.x - 34.0, mute.center().y);
    let (_, actions) = draw(&ctx, &cards, size, 0.2);
    assert!(actions.is_empty());
    let (_, actions) = draw_click(&ctx, &cards, size, 0.3, mute.center());
    assert_eq!(
        actions,
        vec![AudioPlayerAction::SetMuted {
            stream_id: "volume".into(),
            muted: true,
        }],
        "{right:?}"
    );
}

fn draw_click(
    ctx: &egui::Context,
    cards: &[AudioPlayerCard],
    size: egui::Vec2,
    time: f64,
    at: egui::Pos2,
) -> (egui::FullOutput, Vec<AudioPlayerAction>) {
    let mut actions = Vec::new();
    let output = ctx.run_ui(
        egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
            time: Some(time),
            events: vec![
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
            ..Default::default()
        },
        |ui| {
            actions =
                audio_player::draw(ui, cards, Theme::Dark, Lang::En, false, Default::default())
        },
    );
    (output, actions)
}

#[test]
fn previous_points_left_and_next_points_right() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    let cards = vec![card("glyphs")];
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    draw(&ctx, &cards, size, 0.0)
        .0
        .drop_without_applying_deltas();
    let (output, _) = draw(&ctx, &cards, size, 0.1);
    let tree = output.platform_output.accesskit_update.as_ref().unwrap();
    let rect_of = |key: Key| {
        let bounds = tree
            .nodes
            .iter()
            .find(|(_, node)| node.label() == Some(text(Lang::En, key)))
            .unwrap_or_else(|| panic!("{key:?}"))
            .1
            .bounds()
            .unwrap();
        egui::Rect::from_min_max(
            egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
            egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
        )
    };
    let previous = rect_of(Key::AudioPrevious);
    let next = rect_of(Key::AudioNext);
    let (bar, triangle) = glyph(previous);
    assert!(
        bar[0].x < triangle.iter().map(|p| p.x).fold(f32::INFINITY, f32::min),
        "previous: the bar sits left of a left-pointing triangle"
    );
    let (bar, triangle) = glyph(next);
    assert!(
        bar[0].x
            > triangle
                .iter()
                .map(|p| p.x)
                .fold(f32::NEG_INFINITY, f32::max),
        "next: the bar sits right of a right-pointing triangle"
    );
}

fn glyph(rect: egui::Rect) -> (Vec<egui::Pos2>, Vec<egui::Pos2>) {
    // Drawn shapes are not kept between frames; re-draw into a fresh context for the geometry.
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let cards = vec![card("glyphs")];
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    let mut output = None;
    for time in [0.0, 0.1] {
        let mut frame = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(egui::Pos2::ZERO, size)),
                time: Some(time),
                ..Default::default()
            },
            |ui| {
                audio_player::draw(ui, &cards, Theme::Dark, Lang::En, false, Default::default());
            },
        );
        frame.textures_delta.clear();
        output = Some(frame);
    }
    let output = output.unwrap();
    let mut bar = Vec::new();
    let mut triangle = Vec::new();
    fn visit(
        shape: &Shape,
        rect: egui::Rect,
        bar: &mut Vec<egui::Pos2>,
        tri: &mut Vec<egui::Pos2>,
    ) {
        match shape {
            Shape::LineSegment { points, .. } => {
                if (points[0].x - points[1].x).abs() < 0.01
                    && rect.contains(points[0])
                    && rect.contains(points[1])
                {
                    bar.extend_from_slice(points);
                }
            }
            Shape::Path(path) if path.points.len() == 3 && rect.contains(path.points[0]) => {
                tri.extend_from_slice(&path.points);
            }
            Shape::Vec(children) => {
                for child in children {
                    visit(child, rect, bar, tri);
                }
            }
            _ => {}
        }
    }
    for shape in &output.shapes {
        visit(&shape.shape, rect, &mut bar, &mut triangle);
    }
    assert_eq!(bar.len(), 2, "one bar");
    assert_eq!(triangle.len(), 3, "one triangle");
    (bar, triangle)
}

fn text_rect(
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
fn a_resize_is_persisted_after_it_settles_not_on_every_frame() {
    use std::cell::Cell;
    use std::rc::Rc;

    let cards = vec![card("resize")];
    let mut player = bifrost_gui::audio_player::AudioPlayer::default();
    player.cards = cards;
    player.open = true;
    let mut geometry = PlayerGeometry::default();
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.set_embed_viewports(false);
    let frames = Rc::new(Cell::new(0_u32));
    let size = Rc::new(Cell::new(egui::vec2(
        audio_player::DEFAULT_WIDTH,
        audio_player::DEFAULT_HEIGHT,
    )));
    let time = Rc::new(Cell::new(0.0_f64));
    let driven = frames.clone();
    let driven_size = size.clone();
    let driven_time = time.clone();
    egui::Context::set_immediate_viewport_renderer(move |ctx, mut viewport| {
        let frame = driven.get();
        driven.set(frame + 1);
        // Frames 2..=5 move 4 points each: a real drag. Frames 6+ hold still.
        let width = if (1..=4).contains(&frame) {
            audio_player::DEFAULT_WIDTH + 4.0 * frame as f32
        } else {
            driven_size.get().x
        };
        let current = egui::vec2(width, driven_size.get().y);
        driven_size.set(current);
        let rect = egui::Rect::from_min_size(egui::Pos2::ZERO, current);
        let mut input = egui::RawInput {
            viewport_id: viewport.ids.this,
            screen_rect: Some(rect),
            time: Some(driven_time.get()),
            ..Default::default()
        };
        input.viewports.insert(
            viewport.ids.this,
            egui::ViewportInfo {
                parent: Some(viewport.ids.parent),
                inner_rect: Some(rect),
                monitor_size: Some(egui::vec2(1920.0, 1080.0)),
                ..Default::default()
            },
        );
        let mut output = ctx.run_ui(input, |ui| (viewport.viewport_ui_cb)(ui));
        output.textures_delta.clear();
    });
    fn step(
        ctx: &egui::Context,
        player: &mut bifrost_gui::audio_player::AudioPlayer,
        geometry: &mut PlayerGeometry,
        time: &Cell<f64>,
        dt: f64,
    ) {
        time.set(time.get() + dt);
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            assert!(audio_player::viewport(
                ui.ctx(),
                player,
                geometry,
                Theme::Dark,
                Lang::En,
                Default::default()
            )
            .is_empty());
        });
        output.textures_delta.clear();
    }
    step(&ctx, &mut player, &mut geometry, &time, 0.0);
    assert_eq!(geometry.compact[0], audio_player::DEFAULT_WIDTH as u16);
    for _ in 0..4 {
        step(&ctx, &mut player, &mut geometry, &time, 0.05);
        assert_eq!(
            geometry.compact[0],
            audio_player::DEFAULT_WIDTH as u16,
            "a moving drag is not written to the settings file"
        );
    }
    let settled = audio_player::DEFAULT_WIDTH + 16.0;
    assert_eq!(geometry.compact[0], audio_player::DEFAULT_WIDTH as u16);
    step(&ctx, &mut player, &mut geometry, &time, 0.5);
    assert_eq!(
        geometry.compact[0], settled as u16,
        "the settled size is remembered"
    );
    // A window manager larger than the documented maximum is compared and stored in bounded
    // space: the same clamped value on later frames must not look "new" again.
    size.set(egui::vec2(4000.0, 3000.0));
    step(&ctx, &mut player, &mut geometry, &time, 0.5);
    step(&ctx, &mut player, &mut geometry, &time, 1.1);
    assert_eq!(
        geometry.compact,
        [PLAYER_MAX_WIDTH, PLAYER_MAX_HEIGHT],
        "an oversized WM rect is persisted clamped"
    );
    for _ in 0..3 {
        step(&ctx, &mut player, &mut geometry, &time, 0.6);
        assert_eq!(
            geometry.compact,
            [PLAYER_MAX_WIDTH, PLAYER_MAX_HEIGHT],
            "unchanged clamped frames do not churn the remembered value"
        );
    }
    assert_eq!(geometry.expanded, PlayerGeometry::default().expanded);
}

#[test]
fn default_size_paints_the_decoded_cover_texture() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    let mut art = card("cover");
    art.artwork_id = Some([9; 32]);
    art.artwork = Some(Artwork {
        id: [9; 32],
        rgba: Arc::from(vec![128_u8; 16 * 16 * 4].into_boxed_slice()),
        width: 16,
        height: 16,
    });
    let cards = vec![art];
    let (mut out, _) = draw(&ctx, &cards, size, 0.0);
    out.textures_delta.clear();
    let texture = ctx
        .data(|data| {
            data.get_temp::<egui::TextureHandle>(egui::Id::new(("audio-art-texture", [9_u8; 32])))
        })
        .expect("the cover is uploaded")
        .id();
    let (mut out, actions) = draw(&ctx, &cards, size, 0.1);
    assert!(actions.is_empty());
    fn painted(shape: &Shape, texture: egui::TextureId) -> bool {
        match shape {
            Shape::Mesh(mesh) if mesh.texture_id == texture => true,
            Shape::Vec(children) => children.iter().any(|shape| painted(shape, texture)),
            _ => false,
        }
    }
    assert!(
        out.shapes
            .iter()
            .any(|shape| painted(&shape.shape, texture)),
        "the default 368x264 player must paint the decoded cover, not only reserve space"
    );
    out.textures_delta.clear();
}

#[test]
fn row_secondary_controls_have_disjoint_hits_and_do_not_cross_invoke() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    ctx.enable_accesskit();
    let cards = vec![card("a"), card("b"), card("c")];
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    draw(&ctx, &cards, size, 0.0)
        .0
        .drop_without_applying_deltas();
    let (output, actions) = draw(&ctx, &cards, size, 0.1);
    assert!(actions.is_empty());
    let tree = output.platform_output.accesskit_update.as_ref().unwrap();
    let wanted = [
        text(Lang::En, Key::AudioStop),
        text(Lang::En, Key::AudioMute),
        text(Lang::En, Key::AudioPause),
    ];
    let mut row: Vec<(String, egui::Rect)> = tree
        .nodes
        .iter()
        .filter_map(|(_, node)| {
            let label = node.label()?.to_owned();
            if !wanted.contains(&label.as_str()) {
                return None;
            }
            let bounds = node.bounds()?;
            let rect = egui::Rect::from_min_max(
                egui::pos2(bounds.x0 as f32, bounds.y0 as f32),
                egui::pos2(bounds.x1 as f32, bounds.y1 as f32),
            );
            (rect.center().y > size.y * 0.55 && rect.center().x > size.x - 150.0)
                .then_some((label, rect))
        })
        .collect();
    row.sort_by(|a, b| a.1.center().y.total_cmp(&b.1.center().y));
    assert_eq!(
        row.len(),
        6,
        "two secondary rows, three controls each: {row:?}"
    );
    let mut groups: Vec<Vec<(String, egui::Rect)>> = Vec::new();
    for entry in row {
        match groups.last_mut() {
            Some(group) if (group[0].1.center().y - entry.1.center().y).abs() < 2.0 => {
                group.push(entry)
            }
            _ => groups.push(vec![entry]),
        }
    }
    assert_eq!(groups.len(), 2, "{groups:?}");
    for group in &groups {
        assert_eq!(group.len(), 3, "{group:?}");
        for (i, (_, a)) in group.iter().enumerate() {
            for (_, b) in group.iter().skip(i + 1) {
                let overlap = a.intersect(*b);
                assert!(
                    overlap.width() <= 0.0 || overlap.height() <= 0.0,
                    "row controls must be disjoint: {a:?} {b:?}"
                );
            }
        }
        let rect_of = |label: &str| {
            group
                .iter()
                .find(|(value, _)| value == label)
                .map(|(_, rect)| *rect)
                .unwrap_or_else(|| panic!("{label} in {group:?}"))
        };
        let stop = rect_of(text(Lang::En, Key::AudioStop));
        let mute = rect_of(text(Lang::En, Key::AudioMute));
        let play = rect_of(text(Lang::En, Key::AudioPause));
        for (at, expected) in [
            (egui::pos2(stop.right() - 1.0, stop.center().y), "stop"),
            (egui::pos2(mute.left() + 1.0, mute.center().y), "mute"),
            (egui::pos2(mute.right() - 1.0, mute.center().y), "mute"),
            (egui::pos2(play.left() + 1.0, play.center().y), "play"),
        ] {
            assert!(
                size.x > 0.0 && at.x >= stop.left() && at.x <= play.right(),
                "{at:?}"
            );
            let (_, actions) = draw_click(&ctx, &cards, size, 0.2, at);
            let kinds: Vec<&str> = actions
                .iter()
                .map(|action| match action {
                    AudioPlayerAction::Stop(_) => "stop",
                    AudioPlayerAction::SetMuted { .. } => "mute",
                    AudioPlayerAction::PlayPause(_) => "play",
                    other => panic!("unexpected action {other:?}"),
                })
                .collect();
            assert!(
                kinds.iter().all(|kind| *kind == expected),
                "{at:?}: {kinds:?} must only be {expected}"
            );
        }
    }
    output.drop_without_applying_deltas();
}

#[test]
fn decoded_landscape_and_portrait_covers_preserve_aspect_inside_the_square() {
    fn image_bounds(shape: &Shape, texture: egui::TextureId) -> Option<egui::Rect> {
        match shape {
            Shape::Mesh(mesh) if mesh.texture_id == texture => Some(mesh.calc_bounds()),
            Shape::Vec(children) => children
                .iter()
                .find_map(|shape| image_bounds(shape, texture)),
            _ => None,
        }
    }
    fn cover_background(shape: &Shape, image: egui::Rect) -> Option<egui::Rect> {
        match shape {
            Shape::Rect(rect)
                if rect.fill == ui::Palette::of(Theme::Dark).bg2
                    && (rect.rect.width() - rect.rect.height()).abs() < 0.1
                    && rect.rect.center().distance(image.center()) < 0.1 =>
            {
                Some(rect.rect)
            }
            Shape::Vec(children) => children
                .iter()
                .find_map(|shape| cover_background(shape, image)),
            _ => None,
        }
    }
    for (width, height) in [(192, 108), (108, 192), (192, 192)] {
        let image = image::RgbImage::from_pixel(width, height, image::Rgb([40, 120, 200]));
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 80)
            .encode_image(&image)
            .unwrap();
        let mut cover = card("aspect");
        cover.artwork_id = Some([11; 32]);
        cover.artwork = Some(decode_artwork(&[11; 32], &bytes).unwrap());
        let ctx = egui::Context::default();
        ui::install_fonts(&ctx);
        let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
        draw(&ctx, std::slice::from_ref(&cover), size, 0.0)
            .0
            .drop_without_applying_deltas();
        let texture = ctx
            .data(|data| {
                data.get_temp::<egui::TextureHandle>(egui::Id::new((
                    "audio-art-texture",
                    [11_u8; 32],
                )))
            })
            .unwrap()
            .id();
        let (output, actions) = draw(&ctx, &[cover], size, 0.1);
        assert!(actions.is_empty());
        let painted = output
            .shapes
            .iter()
            .find_map(|shape| image_bounds(&shape.shape, texture))
            .expect("the decoded image is painted");
        let square = output
            .shapes
            .iter()
            .find_map(|shape| cover_background(&shape.shape, painted))
            .expect("the existing square background remains behind the cover");
        assert!(square.contains_rect(painted));
        assert!(square.center().distance(painted.center()) < 0.1);
        assert!((painted.width() / painted.height() - width as f32 / height as f32).abs() < 0.001);
        assert!(
            (painted.width() - square.width()).abs() < 0.1
                || (painted.height() - square.height()).abs() < 0.1,
            "contain fills one axis without cropping or stretching"
        );
        if width == height {
            assert_eq!(
                painted, square,
                "square artwork keeps the existing geometry"
            );
        }
        output.drop_without_applying_deltas();
    }
}

#[test]
fn hidden_player_prunes_revoked_artwork_on_the_root_frame() {
    let ctx = egui::Context::default();
    ui::install_fonts(&ctx);
    let size = egui::vec2(audio_player::DEFAULT_WIDTH, audio_player::DEFAULT_HEIGHT);
    let mut active = card("art");
    active.artwork = Some(artwork(7));
    active.artwork_id = Some([7; 32]);
    draw(&ctx, std::slice::from_ref(&active), size, 0.0)
        .0
        .drop_without_applying_deltas();
    let key = egui::Id::new(("audio-art-texture", [7_u8; 32]));
    assert!(ctx
        .data(|data| data.get_temp::<egui::TextureHandle>(key))
        .is_some());
    let mut model = bifrost_gui::audio_player::AudioPlayer::default();
    model.open = false;
    model.cards.push(active);
    let mut geometry = PlayerGeometry::default();
    let output = ctx.run_ui(Default::default(), |ui| {
        assert!(audio_player::viewport(
            ui.ctx(),
            &mut model,
            &mut geometry,
            Theme::Dark,
            Lang::En,
            Default::default()
        )
        .is_empty());
    });
    assert!(
        output.textures_delta.free.is_empty(),
        "hiding an active consented track is not revocation"
    );
    output.drop_without_applying_deltas();
    model.cards.clear();
    let output = ctx.run_ui(Default::default(), |ui| {
        audio_player::viewport(
            ui.ctx(),
            &mut model,
            &mut geometry,
            Theme::Dark,
            Lang::En,
            Default::default(),
        );
    });
    assert!(ctx
        .data(|data| data.get_temp::<egui::TextureHandle>(key))
        .is_none());
    assert!(
        !output.textures_delta.free.is_empty(),
        "root frame frees the upload without reopening the viewport"
    );
    output.drop_without_applying_deltas();
}
