//! Offscreen PNGs of the real player surface, from synthetic fixtures only.
//!
//! ```text
//! cargo run -p bifrost-gui --example player_shots
//! cargo run -p bifrost-gui --example player_shots -- --only responsive --output .notes/ui-player/shots
//! cargo run -p bifrost-gui --example player_shots -- --only tooltip --output .notes/ui-player/tooltips
//! ```
//! This renders the production [`ui::audio_player::draw`] body through egui_kittest/wgpu into
//! transparent-background PNGs under `.notes/ui-player/shots`. It never opens a native window,
//! never talks to the core, never starts audio, and never synthesises desktop input. This
//! maintained reproducer uses invented metadata and optional synthetic JPEG covers decoded
//! through the bounded production decoder; it never fetches external artwork. These PNGs
//! prove offscreen drawing, not native-window behavior or backend/audio acceptance.

use std::{path::Path, process::ExitCode};

use bifrost_gui::audio_player::{AppKind, AudioPlayerCard, StreamState};
use bifrost_gui::egui;
use bifrost_gui::gui_settings::Theme;
use bifrost_gui::ui::{self, audio_player};
use egui_kittest::{kittest::Queryable, Harness};
use ui_text::{text, Key, Lang};

const DEFAULT_OUT: &str = ".notes/ui-player/shots";

fn card(id: &str, source: &str, title: &str, artist: &str, playing: bool) -> AudioPlayerCard {
    AudioPlayerCard {
        stream_id: id.into(),
        source_name: source.into(),
        title: (!title.is_empty()).then(|| title.to_owned()),
        subtitle: (!artist.is_empty()).then(|| artist.to_owned()),
        artwork_id: None,
        artwork: None,
        has_media_session: true,
        app: AppKind::System,
        position_ms: Some(95_000),
        duration_ms: Some(869_000),
        seek: Some(bifrost_gui::audio_player::AudioSeekTarget {
            session_token: 9,
            media_revision: 3,
            min_ms: 30_000,
            max_ms: 869_000,
        }),
        playing,
        can_prev: true,
        can_play_pause: true,
        can_next: true,
        muted: false,
        gain_percent: 62,
        state: StreamState::Ready,
        path: None,
        background: false,
    }
}

/// A synthetic gradient JPEG, decoded through the production bounded decoder. It exists only
/// to exercise the real texture path in a capture; no live provider or fake cover is claimed.
fn fixture_artwork() -> bifrost_gui::audio_player::Artwork {
    let mut image = image::RgbImage::new(192, 192);
    for (x, y, pixel) in image.enumerate_pixels_mut() {
        *pixel = image::Rgb([(x * 255 / 191) as u8, (y * 200 / 191 + 40) as u8, 90]);
    }
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 85)
        .encode_image(&image)
        .expect("encode fixture jpeg");
    bifrost_gui::audio::decode_artwork(&[0x5a; 32], &bytes).expect("decode fixture jpeg")
}

fn art_card(id: &str, title: &str, artist: &str) -> AudioPlayerCard {
    let mut card = card(id, "WORKSTATION", title, artist, true);
    card.artwork_id = Some([0x5a; 32]);
    card.artwork = Some(fixture_artwork());
    card
}

struct Case {
    name: &'static str,
    size: [f32; 2],
    expanded: bool,
    theme: Theme,
    lang: Lang,
    cards: Vec<AudioPlayerCard>,
}

fn cases() -> Vec<Case> {
    let mut system = card("sys", "WORKSTATION", "", "", true);
    system.has_media_session = false;
    system.title = None;
    system.subtitle = None;
    system.position_ms = None;
    system.duration_ms = None;
    system.seek = None;

    let mut paused = card("paused", "WORKSTATION", "Night drive", "Road tapes", false);
    paused.can_prev = false;
    paused.can_next = false;

    let mut long = card(
        "long",
        "WORKSTATION",
        "A deliberately long title that must be ellipsized without touching any control",
        "An equally long artist and album line",
        true,
    );
    long.gain_percent = 100;

    let mut second = card("s2", "LAPTOP", "Second stream", "Background", true);
    second.background = true;
    second.state = StreamState::Buffering;
    let mut third = card("s3", "HTPC", "Third stream", "Podcast", false);
    third.muted = true;
    third.state = StreamState::Degraded;

    let mut cases = vec![
        Case {
            name: "compact-dark-en",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![card("a", "WORKSTATION", "Night drive", "Road tapes", true)],
        },
        Case {
            name: "compact-light-ru",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Light,
            lang: Lang::Ru,
            cards: vec![card(
                "a",
                "WORKSTATION",
                "Ночная поездка",
                "Дорожные записи",
                true,
            )],
        },
        Case {
            name: "compact-system-audio",
            size: [368.0, 226.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![system.clone()],
        },
        Case {
            name: "compact-paused-disabled",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Light,
            lang: Lang::En,
            cards: vec![paused],
        },
        Case {
            name: "minimum-long-title",
            size: [320.0, 228.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![long],
        },
        Case {
            name: "stacked-three-compact",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![
                card("a", "WORKSTATION", "Night drive", "Road tapes", true),
                second,
                third,
            ],
        },
        Case {
            name: "stacked-three-tall-window",
            size: [368.0, 620.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![
                card("a", "WORKSTATION", "Night drive", "Road tapes", true),
                card("s2", "LAPTOP", "Second stream", "Background", true),
                card("s3", "HTPC", "Third stream", "Podcast", false),
            ],
        },
        Case {
            name: "empty",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: Vec::new(),
        },
        Case {
            name: "expanded-tall-dark",
            size: [560.0, 680.0],
            expanded: true,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![card("a", "WORKSTATION", "Night drive", "Road tapes", true)],
        },
        Case {
            name: "expanded-tall-light-ru",
            size: [560.0, 680.0],
            expanded: true,
            theme: Theme::Light,
            lang: Lang::Ru,
            cards: vec![card(
                "a",
                "WORKSTATION",
                "Ночная поездка",
                "Дорожные записи",
                true,
            )],
        },
        Case {
            name: "expanded-wide-dark",
            size: [900.0, 420.0],
            expanded: true,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![card("a", "WORKSTATION", "Night drive", "Road tapes", true)],
        },
        Case {
            name: "expanded-wide-light",
            size: [900.0, 420.0],
            expanded: true,
            theme: Theme::Light,
            lang: Lang::En,
            cards: vec![card("a", "WORKSTATION", "Night drive", "Road tapes", true)],
        },
        Case {
            name: "compact-art-dark",
            size: [368.0, 264.0],
            expanded: false,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![art_card("art-c", "Night drive", "Road tapes")],
        },
        Case {
            name: "expanded-tall-art-dark",
            size: [560.0, 680.0],
            expanded: true,
            theme: Theme::Dark,
            lang: Lang::En,
            cards: vec![art_card("art-t", "Night drive", "Road tapes")],
        },
        Case {
            name: "expanded-wide-art-light",
            size: [900.0, 420.0],
            expanded: true,
            theme: Theme::Light,
            lang: Lang::En,
            cards: vec![art_card("art-w", "Night drive", "Road tapes")],
        },
    ];
    for (name, width, height, theme, lang, artwork) in [
        (
            "responsive-mode-tooltip-ru-dark",
            428.0,
            339.0,
            Theme::Dark,
            Lang::Ru,
            false,
        ),
        (
            "responsive-mode-tooltip-en-light",
            428.0,
            339.0,
            Theme::Light,
            Lang::En,
            true,
        ),
        (
            "responsive-428-ru-dark-no-art",
            428.0,
            339.0,
            Theme::Dark,
            Lang::Ru,
            false,
        ),
        (
            "responsive-428-en-light-art",
            428.0,
            339.0,
            Theme::Light,
            Lang::En,
            true,
        ),
        (
            "responsive-428-ru-light-art",
            428.0,
            339.0,
            Theme::Light,
            Lang::Ru,
            true,
        ),
        (
            "responsive-428-en-dark-no-art",
            428.0,
            339.0,
            Theme::Dark,
            Lang::En,
            false,
        ),
        (
            "responsive-400-en-dark",
            400.0,
            339.0,
            Theme::Dark,
            Lang::En,
            false,
        ),
        (
            "responsive-460-en-dark",
            460.0,
            339.0,
            Theme::Dark,
            Lang::En,
            true,
        ),
        (
            "responsive-520-ru-light",
            520.0,
            339.0,
            Theme::Light,
            Lang::Ru,
            false,
        ),
        (
            "responsive-559-en-dark",
            559.0,
            339.0,
            Theme::Dark,
            Lang::En,
            true,
        ),
        (
            "responsive-560-en-dark",
            560.0,
            339.0,
            Theme::Dark,
            Lang::En,
            false,
        ),
        (
            "responsive-632-ru-dark",
            632.0,
            332.0,
            Theme::Dark,
            Lang::Ru,
            false,
        ),
        (
            "responsive-368-en-light",
            368.0,
            264.0,
            Theme::Light,
            Lang::En,
            true,
        ),
        (
            "responsive-320-ru-dark",
            320.0,
            228.0,
            Theme::Dark,
            Lang::Ru,
            false,
        ),
        (
            "responsive-560-tall-ru-dark",
            560.0,
            680.0,
            Theme::Dark,
            Lang::Ru,
            true,
        ),
    ] {
        let (title, artist) = if lang == Lang::Ru {
            (
                "Франсе — длинное название живого выступления",
                "Воскресенский — запись концерта",
            )
        } else {
            (
                "A long title from a live performance",
                "Artist — live concert recording",
            )
        };
        cases.push(Case {
            name,
            size: [width, height],
            expanded: height > 400.0,
            theme,
            lang,
            cards: vec![if artwork {
                art_card("responsive", title, artist)
            } else {
                card("responsive", "WORKSTATION", title, artist, true)
            }],
        });
    }
    cases
}

/// The rail/settings icon set at the three real sizes, drawn by the production painter.
fn shoot_icons(dir: &Path, theme: Theme) -> Result<(), String> {
    let name = match theme {
        Theme::Dark => "icons-dark",
        Theme::Light => "icons-light",
    };
    let size = egui::vec2(520.0, 400.0);
    let mut harness = Harness::builder()
        .with_size(size + egui::vec2(16.0, 16.0))
        .wgpu()
        .build_ui(move |ui| {
            let pal = ui::Palette::of(theme);
            ui.painter().rect_filled(ui.max_rect(), 0.0, pal.bg);
            let icons = [
                (ui::RailIcon::Headphones, "Headphones"),
                (ui::RailIcon::Audio, "Audio"),
                (ui::RailIcon::Pairs, "Pairs"),
                (ui::RailIcon::Keys, "Keys"),
                (ui::RailIcon::Clip, "Clip"),
                (ui::RailIcon::Diag, "Diag"),
                (ui::RailIcon::Settings, "Settings"),
                (ui::RailIcon::Appearance, "Appearance"),
                (ui::RailIcon::Close, "Close"),
                (ui::RailIcon::Theme, "Theme"),
            ];
            let painter = ui.painter().clone();
            for (row, (icon, label)) in icons.iter().enumerate() {
                let y = 24.0 + row as f32 * 36.0;
                painter.text(
                    egui::pos2(16.0, y),
                    egui::Align2::LEFT_CENTER,
                    *label,
                    ui::theme_font(11.0),
                    pal.ink2,
                );
                for (column, size) in [16.0_f32, 20.0, 24.0].iter().enumerate() {
                    let center = egui::pos2(260.0 + column as f32 * 88.0, y);
                    ui::paint_rail_icon(&painter, *icon, center, *size, pal.ink);
                    painter.text(
                        egui::pos2(center.x, y + 18.0),
                        egui::Align2::CENTER_TOP,
                        format!("{size:.0}"),
                        ui::theme_font(9.0),
                        pal.ink2,
                    );
                }
            }
        });
    ui::install_fonts(&harness.ctx);
    harness.run_steps(3);
    let image = harness.render()?;
    let cropped = image::imageops::crop_imm(&image, 8, 8, 520, 400).to_image();
    cropped
        .save(dir.join(format!("{name}.png")))
        .map_err(|error| error.to_string())?;
    println!("{}", dir.join(format!("{name}.png")).display());
    Ok(())
}

fn shoot(case: &Case, dir: &Path) -> Result<(), String> {
    let (cards, theme, lang, expanded) = (case.cards.clone(), case.theme, case.lang, case.expanded);
    let size = egui::vec2(case.size[0], case.size[1]);
    let tooltip = case.name.contains("tooltip");
    // Popups must see the same viewport that is exported; ordinary fixtures keep their
    // established 8-point harness inset/crop.
    let mut harness = Harness::builder()
        .with_size(if tooltip {
            size
        } else {
            size + egui::vec2(16.0, 16.0)
        })
        .wgpu()
        .build_ui(move |ui| {
            let draw_surface = |ui: &mut egui::Ui| {
                // A neutral desktop behind the opaque surface, so the capture reads like a window.
                ui.painter().rect_filled(
                    ui.max_rect(),
                    0.0,
                    egui::Color32::from_rgb(0x23, 0x23, 0x26),
                );
                audio_player::draw(
                    ui,
                    &cards,
                    theme,
                    lang,
                    expanded,
                    bifrost_gui::audio_player::PlayerMode {
                        current: Some(if theme == Theme::Dark {
                            protocol::ipc::AudioMode::FollowFocus
                        } else {
                            protocol::ipc::AudioMode::MixAll
                        }),
                        enabled: true,
                    },
                );
            };
            if tooltip {
                let client = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
                let mut surface = ui.new_child(egui::UiBuilder::new().max_rect(client));
                surface.set_clip_rect(client);
                egui::Frame::NONE.show(&mut surface, draw_surface);
            } else {
                draw_surface(ui);
            }
        });
    ui::install_fonts(&harness.ctx);
    harness.run_steps(3);
    if tooltip {
        let key = if theme == Theme::Dark {
            Key::AudioFollowFocus
        } else {
            Key::AudioMixAll
        };
        harness.get_by_label(text(lang, key)).hover();
        harness.run_steps(60);
        let client = egui::Rect::from_min_size(egui::Pos2::ZERO, size);
        assert_eq!(
            harness.ctx.content_rect(),
            client,
            "tooltip viewport must match the PNG"
        );
        let roots: Vec<_> = harness.ctx.memory(|memory| {
            memory
                .areas()
                .visible_layer_ids()
                .into_iter()
                .filter(|layer| layer.order == egui::Order::Tooltip)
                .collect()
        });
        assert_eq!(roots.len(), 1, "one mode tooltip");
        let frames: Vec<_> = roots
            .into_iter()
            .map(|layer| {
                egui::AreaState::load(&harness.ctx, layer.id)
                    .unwrap()
                    .rect()
            })
            .collect();
        for frame in &frames {
            assert!(
                client.contains_rect(*frame),
                "tooltip frame outside actual viewport: {frame:?}"
            );
        }
        let viewport = harness.ctx.content_rect();
        let geometry = serde_json::json!({
            "units": "egui points; capture uses one pixel per point",
            "viewport": [viewport.left(), viewport.top(), viewport.right(), viewport.bottom()],
            "popup_frames": frames.iter().map(|frame|
                [frame.left(), frame.top(), frame.right(), frame.bottom()]).collect::<Vec<_>>(),
        });
        std::fs::write(
            dir.join(format!("{}.geometry.json", case.name)),
            serde_json::to_vec_pretty(&geometry).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
    }
    let image = harness.render()?;
    let image = if tooltip {
        image
    } else {
        image::imageops::crop_imm(&image, 8, 8, case.size[0] as u32, case.size[1] as u32).to_image()
    };
    let file = dir.join(format!("{}.png", case.name));
    image.save(&file).map_err(|error| error.to_string())?;
    println!("{}", file.display());
    Ok(())
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut output = DEFAULT_OUT.to_owned();
    let mut only: Option<String> = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" => output = args.next().unwrap_or(output),
            "--only" => only = args.next(),
            other => {
                eprintln!("unknown argument {other}; use --output DIR");
                return ExitCode::FAILURE;
            }
        }
    }
    let dir = Path::new(&output);
    if let Err(error) = std::fs::create_dir_all(dir) {
        eprintln!("cannot create {}: {error}", dir.display());
        return ExitCode::FAILURE;
    }
    let mut failed = false;
    for case in cases() {
        if only
            .as_deref()
            .is_some_and(|name| !case.name.contains(name))
        {
            continue;
        }
        match shoot(&case, dir) {
            Ok(()) => {}
            Err(error) => {
                eprintln!("{}: {error}", case.name);
                failed = true;
            }
        }
    }
    for theme in [Theme::Dark, Theme::Light] {
        if let Err(error) = shoot_icons(dir, theme) {
            eprintln!("icons {theme:?}: {error}");
            failed = true;
        }
    }
    if failed {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}
