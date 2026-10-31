//! Compact remote-audio cards. Pure drawing: all effects leave as typed intents.
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use ui_text::{text, Key, Lang};

use super::{
    theme::{fam, font},
    Palette,
};
use crate::audio_player::{
    AppKind, AudioPlayer, AudioPlayerAction as Action, AudioPlayerCard, StreamState,
};
use crate::gui_settings::Theme;

pub const WIDTH: f32 = 240.0;

fn has_progress(card: &AudioPlayerCard) -> bool {
    card.has_media_session
        && card.title.as_ref().is_some_and(|s| !s.trim().is_empty())
        && card.position_ms.is_some()
        && card.duration_ms.is_some_and(|ms| ms > 0)
}

pub fn height(cards: &[AudioPlayerCard]) -> f32 {
    if cards.is_empty() {
        return 92.0;
    }
    16.0 + cards.iter().map(card_height).sum::<f32>() + 8.0 * cards.len().saturating_sub(1) as f32
}

fn card_height(card: &AudioPlayerCard) -> f32 {
    if has_progress(card) {
        128.0
    } else {
        104.0
    }
}

/// Draw at any width >= 225 logical pixels; the wide single-card reference uses the same layout.
pub fn draw(ui: &mut egui::Ui, cards: &[AudioPlayerCard], theme: Theme, lang: Lang) -> Vec<Action> {
    if !ui.ctx().fonts(|fonts| {
        fonts
            .definitions()
            .families
            .contains_key(&fam::sans_medium())
    }) {
        super::install_fonts(ui.ctx());
        ui.ctx().request_repaint();
        return Vec::new();
    }
    let pal = Palette::of(theme);
    let mut actions = Vec::new();
    ui.spacing_mut().item_spacing = Vec2::new(0.0, 8.0);
    if cards.is_empty() {
        egui::Frame::new()
            .fill(pal.card)
            .corner_radius(16)
            .inner_margin(16.0)
            .show(ui, |ui| {
                ui.label(egui::RichText::new(text(lang, Key::AudioEmpty)).color(pal.ink2));
                if ui.button(text(lang, Key::AudioHide)).clicked() {
                    actions.push(Action::HidePlayer);
                }
            });
    }
    for card in cards {
        ui.push_id(&card.stream_id, |ui| {
            let size = Vec2::new(ui.available_width(), card_height(card));
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                let mut card_palette = pal;
                if card.background {
                    // Dim emphasis, not the surface: transparent white becomes grey over a dark desktop.
                    card_palette.ink = pal.ink2;
                    card_palette.accent_ink = pal.ink2;
                    card_palette.card = pal.bg;
                }
                draw_card(ui, rect, card, &card_palette, lang, &mut actions);
            });
        });
    }
    actions
}

fn label(ui: &mut egui::Ui, rect: Rect, value: &str, size: f32, color: Color32) {
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(value)
                    .font(font(size, fam::sans_medium()))
                    .color(color),
            )
            .truncate(),
        );
    });
}

fn draw_card(
    ui: &mut egui::Ui,
    rect: Rect,
    c: &AudioPlayerCard,
    pal: &Palette,
    lang: Lang,
    out: &mut Vec<Action>,
) {
    ui.painter()
        .rect(rect, 16, pal.card, pal.hairline(), egui::StrokeKind::Inside);
    let left = rect.left() + 14.0;
    let right = rect.right() - 14.0;
    let top = rect.top() + 12.0;
    let title = c.title.as_deref().filter(|t| !t.trim().is_empty());
    let title_rect = Rect::from_min_max(Pos2::new(left, top), Pos2::new(right - 48.0, top + 20.0));
    let drag = ui.interact(title_rect, ui.id().with("drag"), Sense::drag());
    if drag.drag_started() {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    label(
        ui,
        title_rect,
        title.unwrap_or(&c.source_name),
        13.0,
        pal.ink,
    );
    let expand = Rect::from_min_size(Pos2::new(right - 44.0, top - 3.0), Vec2::splat(24.0));
    if button(
        ui,
        expand,
        Glyph::Expand,
        text(lang, Key::AudioSettings),
        true,
        pal,
    )
    .clicked()
    {
        out.push(Action::OpenSettings);
    }
    let close = Rect::from_min_size(Pos2::new(right - 20.0, top - 3.0), Vec2::splat(24.0));
    if button(
        ui,
        close,
        Glyph::Close,
        text(lang, Key::AudioStop),
        true,
        pal,
    )
    .clicked()
    {
        out.push(Action::Stop(c.stream_id.clone()));
    }
    let unavailable =
        c.state == StreamState::Failed && c.path.is_some_and(|path| path.has_diagnostic());
    let subtitle = if unavailable {
        if title.is_some() {
            format!(
                "{} · {}",
                text(lang, Key::AudioPathUnavailable),
                c.source_name
            )
        } else {
            text(lang, Key::AudioPathUnavailable).to_owned()
        }
    } else if title.is_some() {
        c.subtitle
            .as_ref()
            .filter(|s| !s.trim().is_empty())
            .map_or_else(
                || c.source_name.clone(),
                |s| format!("{s} · {}", c.source_name),
            )
    } else {
        text(lang, Key::AudioSystem).to_owned()
    };
    label(
        ui,
        Rect::from_min_max(
            Pos2::new(left, top + 23.0),
            Pos2::new(right - 12.0, top + 41.0),
        ),
        &subtitle,
        11.0,
        pal.ink2,
    );
    let state_key = match c.state {
        StreamState::Ready => {
            if c.background {
                Key::AudioBackground
            } else {
                Key::AudioPrimary
            }
        }
        StreamState::Buffering => Key::AudioBuffering,
        StreamState::Degraded => Key::AudioDegraded,
        StreamState::Failed if unavailable => Key::AudioPathUnavailable,
        StreamState::Failed => Key::AudioDegraded,
    };
    let dot = Pos2::new(right - 2.0, top + 31.0);
    ui.painter().circle_filled(
        dot,
        2.5,
        if c.state == StreamState::Ready {
            pal.ink2
        } else {
            pal.accent
        },
    );
    let health = ui.interact(
        Rect::from_min_max(
            Pos2::new(left, top + 23.0),
            Pos2::new(right + 4.0, top + 41.0),
        ),
        ui.id().with("health"),
        Sense::hover(),
    );
    let path_tooltip = c.path.map(|path| {
        format!(
            "{}\n{}",
            text(lang, state_key),
            path.tooltip(lang, &c.source_name)
        )
    });
    let health_text = path_tooltip.as_deref().unwrap_or(text(lang, state_key));
    health.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, health_text));
    health.on_hover_text(health_text);
    if has_progress(c) {
        let y = top + 57.0;
        let elapsed = c.position_ms.unwrap_or(0).min(c.duration_ms.unwrap_or(0));
        let duration = c.duration_ms.unwrap_or(1);
        label(
            ui,
            Rect::from_min_size(Pos2::new(left, y - 7.0), Vec2::new(40.0, 16.0)),
            &time(elapsed),
            10.0,
            pal.ink2,
        );
        label(
            ui,
            Rect::from_min_size(Pos2::new(right - 36.0, y - 7.0), Vec2::new(40.0, 16.0)),
            &time(duration),
            10.0,
            pal.accent_ink,
        );
        let start = Pos2::new(left + 40.0, y);
        let end = Pos2::new(right - 43.0, y);
        ui.painter()
            .line_segment([start, end], Stroke::new(2.0, pal.line));
        let played_end = start.lerp(end, elapsed as f32 / duration as f32);
        ui.painter()
            .line_segment([start, played_end], Stroke::new(2.0, pal.accent));
        if elapsed > 0 {
            ui.painter().circle_filled(played_end, 1.5, pal.accent);
        }
    }
    let y = rect.bottom() - 41.0;
    let step = (right - left - 28.0) / 4.0;
    let app_rect = Rect::from_min_size(Pos2::new(left, y), Vec2::splat(28.0));
    if c.app == AppKind::YouTube {
        ui.painter().rect_filled(
            app_rect.shrink2(Vec2::new(1.0, 5.0)),
            5,
            Color32::from_rgb(230, 45, 55),
        );
        glyph(ui, app_rect, Glyph::Play, Color32::WHITE);
    } else {
        glyph(
            ui,
            app_rect,
            if c.has_media_session {
                Glyph::Volume
            } else {
                Glyph::SystemAudio
            },
            pal.ink2,
        );
    }
    let controls = [
        (
            Glyph::Previous,
            Key::AudioPrevious,
            c.can_prev,
            Action::Previous as fn(String) -> Action,
        ),
        (
            if c.playing { Glyph::Pause } else { Glyph::Play },
            if c.playing {
                Key::AudioPause
            } else {
                Key::AudioPlay
            },
            c.can_play_pause,
            Action::PlayPause as fn(String) -> Action,
        ),
        (
            Glyph::Next,
            Key::AudioNext,
            c.can_next,
            Action::Next as fn(String) -> Action,
        ),
    ];
    if c.has_media_session {
        for (i, (icon, key, enabled, action)) in controls.into_iter().enumerate() {
            let r = Rect::from_min_size(
                Pos2::new(left + step * (i + 1) as f32, y),
                Vec2::splat(28.0),
            );
            if button(ui, r, icon, text(lang, key), enabled, pal).clicked() {
                out.push(action(c.stream_id.clone()));
            }
        }
    } else {
        // No transport controls or vacant transport slots for a system-only stream.
        let level = format!("{}%", c.gain_percent.min(100));
        label(
            ui,
            Rect::from_min_size(
                Pos2::new(rect.center().x - 16.0, y + 6.0),
                Vec2::new(40.0, 18.0),
            ),
            &level,
            11.0,
            pal.ink2,
        );
    }
    let r = Rect::from_min_size(Pos2::new(right - 28.0, y), Vec2::splat(28.0));
    let response = button(
        ui,
        r,
        if c.muted { Glyph::Muted } else { Glyph::Volume },
        text(
            lang,
            if c.muted {
                Key::AudioUnmute
            } else {
                Key::AudioMute
            },
        ),
        true,
        pal,
    );
    if response.clicked() {
        out.push(Action::SetMuted {
            stream_id: c.stream_id.clone(),
            muted: !c.muted,
        });
    }
    response.on_hover_ui(|ui| {
        let mut gain = c.gain_percent.min(100);
        if ui
            .add(
                egui::Slider::new(&mut gain, 0..=100)
                    .text(text(lang, Key::AudioGain))
                    .suffix("%"),
            )
            .changed()
        {
            out.push(Action::SetGain {
                stream_id: c.stream_id.clone(),
                percent: gain,
            });
        }
    });
    if ui.rect_contains_pointer(r) {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let gain =
                (i32::from(c.gain_percent) + if scroll > 0.0 { 5 } else { -5 }).clamp(0, 100) as u8;
            out.push(Action::SetGain {
                stream_id: c.stream_id.clone(),
                percent: gain,
            });
        }
    }
}

#[derive(Clone, Copy)]
enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
    Volume,
    SystemAudio,
    Muted,
    Expand,
    Close,
}

fn button(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: Glyph,
    name: &str,
    enabled: bool,
    pal: &Palette,
) -> egui::Response {
    let response = ui.interact(
        rect,
        ui.id().with(name),
        if enabled {
            Sense::click()
        } else {
            Sense::hover()
        },
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, enabled, name));
    if enabled && (response.hovered() || response.has_focus()) {
        ui.painter().rect_filled(rect, 6, pal.accent_soft);
        if response.has_focus() {
            ui.painter().rect_stroke(
                rect,
                6,
                Stroke::new(1.0, pal.accent),
                egui::StrokeKind::Inside,
            );
        }
    }
    glyph(
        ui,
        rect,
        icon,
        if enabled {
            pal.ink2
        } else {
            pal.ink2.gamma_multiply(0.35)
        },
    );
    response.on_hover_text(name)
}

fn glyph(ui: &egui::Ui, rect: Rect, icon: Glyph, color: Color32) {
    let p = ui.painter();
    let center = rect.center();
    let point = |x: f32, y: f32| center + Vec2::new(x, y);
    let line = |a, b| {
        p.line_segment([a, b], Stroke::new(1.5, color));
    };
    let triangle = |flip: f32| {
        p.add(egui::Shape::convex_polygon(
            vec![
                point(-3.0 * flip, -5.0),
                point(5.0 * flip, 0.0),
                point(-3.0 * flip, 5.0),
            ],
            color,
            Stroke::NONE,
        ));
    };
    match icon {
        Glyph::SystemAudio => {
            p.rect_stroke(
                Rect::from_min_max(point(-7.0, -6.0), point(7.0, 3.0)),
                1,
                Stroke::new(1.5, color),
                egui::StrokeKind::Inside,
            );
            line(point(0.0, 3.0), point(0.0, 6.0));
            line(point(-4.0, 6.0), point(4.0, 6.0));
        }
        Glyph::Play => triangle(1.0),
        Glyph::Previous | Glyph::Next => {
            let f = if matches!(icon, Glyph::Previous) {
                -1.0
            } else {
                1.0
            };
            triangle(f);
            line(point(5.0 * f, -5.0), point(5.0 * f, 5.0));
        }
        Glyph::Pause => {
            for x in [-3.0, 3.0] {
                p.rect_filled(
                    Rect::from_center_size(point(x, 0.0), Vec2::new(2.5, 10.0)),
                    0,
                    color,
                );
            }
        }
        Glyph::Close => {
            line(point(-4.0, -4.0), point(4.0, 4.0));
            line(point(4.0, -4.0), point(-4.0, 4.0));
        }
        Glyph::Expand => {
            line(point(-4.0, 4.0), point(4.0, -4.0));
            line(point(-1.0, -4.0), point(4.0, -4.0));
            line(point(4.0, -4.0), point(4.0, 1.0));
            line(point(-5.0, -2.0), point(-5.0, 5.0));
            line(point(-5.0, 5.0), point(2.0, 5.0));
        }
        Glyph::Volume | Glyph::Muted => {
            p.add(egui::Shape::closed_line(
                vec![
                    point(-6.0, -2.0),
                    point(-3.0, -2.0),
                    point(1.0, -5.0),
                    point(1.0, 5.0),
                    point(-3.0, 2.0),
                    point(-6.0, 2.0),
                ],
                Stroke::new(1.4, color),
            ));
            if matches!(icon, Glyph::Muted) {
                line(point(4.0, -3.0), point(8.0, 3.0));
                line(point(8.0, -3.0), point(4.0, 3.0));
            } else {
                line(point(5.0, -4.0), point(7.0, 0.0));
                line(point(7.0, 0.0), point(5.0, 4.0));
            }
        }
    }
}

fn time(ms: u64) -> String {
    let seconds = ms / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}

/// A separate native viewport; immediate mode keeps actions on the GUI thread.
pub fn viewport(
    ctx: &egui::Context,
    model: &mut AudioPlayer,
    theme: Theme,
    lang: Lang,
) -> Vec<Action> {
    if !model.open {
        return Vec::new();
    }
    let max_height = work_area(ctx).map_or(720.0, |work| (work.height() - 32.0).max(80.0));
    let size = Vec2::new(WIDTH, height(&model.cards).min(max_height));
    let mut actions = Vec::new();
    let builder = egui::ViewportBuilder::default()
        .with_title(text(lang, Key::AudioPlayer))
        .with_inner_size(size)
        .with_min_inner_size([225.0, 80.0])
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(true)
        .with_always_on_top()
        .with_taskbar(false);
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of("audio-player"),
        builder,
        |ctx, class| {
            if ctx.input(|i| i.viewport().close_requested()) {
                model.open = false;
            }
            if class == egui::ViewportClass::EmbeddedWindow {
                egui::Window::new(text(lang, Key::AudioPlayer))
                    .open(&mut model.open)
                    .show(ctx, |ui| {
                        actions = draw(ui, &model.cards, theme, lang);
                    });
            } else {
                let positioned = ctx.data_mut(|d| {
                    d.get_temp::<bool>(egui::Id::new("audio-positioned"))
                        .unwrap_or(false)
                });
                if !positioned {
                    if let Some(work) = work_area(ctx) {
                        ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                            (work.right() - size.x - 16.0).max(work.left()),
                            (work.bottom() - size.y - 16.0).max(work.top()),
                        )));
                        ctx.data_mut(|d| d.insert_temp(egui::Id::new("audio-positioned"), true));
                    }
                }
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
                egui::CentralPanel::default()
                    .frame(
                        egui::Frame::new()
                            .fill(Color32::TRANSPARENT)
                            .inner_margin(8.0),
                    )
                    .show(ctx, |ui| {
                        egui::ScrollArea::vertical().show(ui, |ui| {
                            actions = draw(ui, &model.cards, theme, lang);
                        });
                    });
            }
        },
    );
    actions
}

fn work_area(ctx: &egui::Context) -> Option<Rect> {
    // egui exposes the monitor, not the OS work area. Keep clear of a conventional dock;
    // exact taskbar exclusion needs a safe platform work-area adapter.
    ctx.input(|i| {
        i.viewport()
            .monitor_size
            .map(|size| Rect::from_min_size(Pos2::ZERO, size - Vec2::new(0.0, 48.0)))
    })
}
