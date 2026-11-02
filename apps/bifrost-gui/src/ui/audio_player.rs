//! Compact remote-audio cards. Pure drawing: all effects leave as typed intents.
use eframe::egui::{self, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use ui_text::{text, Key, Lang};

use super::{
    theme::{fam, font},
    Palette,
};
use crate::audio_player::{
    AppKind, AudioPlayer, AudioPlayerAction as Action, AudioPlayerCard, AudioSeekTarget,
    StreamState,
};
use crate::gui_settings::Theme;

pub const WIDTH: f32 = 240.0;

#[derive(Clone, Copy)]
struct SeekPress {
    widget_id: egui::Id,
    target: AudioSeekTarget,
    key: Option<egui::Key>,
}

fn has_progress(card: &AudioPlayerCard) -> bool {
    card.has_media_session
        && card.position_ms.is_some()
        && card.duration_ms.is_some_and(|ms| ms > 0)
}

fn seek_without_pointer(
    ui: &egui::Ui,
    response: &egui::Response,
    target: AudioSeekTarget,
    position: u64,
    press: &mut Option<SeekPress>,
) -> Option<u64> {
    use egui::accesskit::{Action as AccessibleAction, ActionData};
    let step = |forward| {
        if forward {
            position.saturating_add(5_000)
        } else {
            position.saturating_sub(5_000)
        }
        .clamp(target.min_ms, target.max_ms)
    };
    let focused = response.has_focus();
    if focused {
        ui.memory_mut(|memory| {
            memory.set_focus_lock_filter(
                response.id,
                egui::EventFilter {
                    horizontal_arrows: true,
                    ..Default::default()
                },
            );
        });
    }
    if focused && !press.is_some_and(|press| press.key.is_none()) {
        if press.is_none() {
            if let Some(key) = ui.input(|input| {
                input.events.iter().find_map(|event| match event {
                    egui::Event::Key {
                        key: key @ (egui::Key::ArrowLeft | egui::Key::ArrowRight),
                        pressed: true,
                        repeat: false,
                        ..
                    } => Some(*key),
                    _ => None,
                })
            }) {
                ui.memory_mut(|memory| memory.move_focus(egui::FocusDirection::None));
                *press = Some(SeekPress {
                    widget_id: response.id,
                    target,
                    key: Some(key),
                });
            }
        }
        if let Some(captured) = *press {
            if captured.widget_id == response.id && captured.target == target {
                if let Some(key) = captured
                    .key
                    .filter(|key| ui.input(|input| input.key_released(*key)))
                {
                    *press = None;
                    return Some(step(key == egui::Key::ArrowRight));
                }
            }
        }
    }
    ui.input(|input| {
        for request in input.accesskit_action_requests(response.id, AccessibleAction::SetValue) {
            if let Some(ActionData::NumericValue(value)) = request.data {
                if value.is_finite() && value >= 0.0 {
                    *press = None;
                    return Some(
                        ((value * 1_000.0).round() as u64).clamp(target.min_ms, target.max_ms),
                    );
                }
            }
        }
        let increment =
            input.num_accesskit_action_requests(response.id, AccessibleAction::Increment);
        let decrement =
            input.num_accesskit_action_requests(response.id, AccessibleAction::Decrement);
        if increment > 0 || decrement > 0 {
            *press = None;
            Some(step(increment > 0))
        } else {
            None
        }
    })
}

pub fn height(cards: &[AudioPlayerCard]) -> f32 {
    if cards.is_empty() {
        return 48.0;
    }
    24.0 + cards.iter().map(card_height).sum::<f32>() + 8.0 * cards.len().saturating_sub(1) as f32
}

fn card_height(card: &AudioPlayerCard) -> f32 {
    if has_progress(card) {
        160.0
    } else {
        136.0
    }
}

fn empty_height(ctx: &egui::Context, width: f32, lang: Lang) -> f32 {
    ctx.fonts_mut(|fonts| {
        fonts
            .layout(
                text(lang, Key::AudioEmpty).to_owned(),
                font(13.0, fam::sans_medium()),
                Color32::WHITE,
                (width - 36.0).max(1.0),
            )
            .size()
            .y
            .ceil()
            .max(24.0)
    })
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
    let seek_key = ui.id().with("seek-press");
    let mut seek_press = ui.data(|data| data.get_temp::<SeekPress>(seek_key));
    let cancel_seek = ui.input(|input| input.key_pressed(egui::Key::Escape) || !input.focused);
    if cancel_seek {
        seek_press = None;
    }
    let mut seek_seen = false;
    if cards.is_empty() {
        let size = Vec2::new(
            ui.available_width(),
            empty_height(ui.ctx(), ui.available_width(), lang),
        );
        let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
        let close = Rect::from_min_size(
            Pos2::new(rect.right() - 24.0, rect.top()),
            Vec2::splat(24.0),
        );
        let drag = ui.interact(
            Rect::from_min_max(rect.min, Pos2::new(close.left() - 8.0, rect.bottom())),
            ui.id().with("empty-drag"),
            Sense::drag(),
        );
        if drag.drag_started_by(egui::PointerButton::Primary) {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
        }
        ui.scope_builder(
            egui::UiBuilder::new().max_rect(Rect::from_min_max(
                rect.min,
                Pos2::new(close.left() - 12.0, rect.bottom()),
            )),
            |ui| {
                ui.add(
                    egui::Label::new(
                        egui::RichText::new(text(lang, Key::AudioEmpty))
                            .font(font(13.0, fam::sans_medium()))
                            .color(pal.ink2),
                    )
                    .selectable(false)
                    .wrap(),
                );
            },
        );
        if button(
            ui,
            close,
            Glyph::Close,
            text(lang, Key::AudioHide),
            true,
            &pal,
        )
        .clicked()
        {
            actions.push(Action::HidePlayer);
        }
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
                seek_seen |= draw_card(
                    ui,
                    rect,
                    card,
                    &card_palette,
                    lang,
                    &mut seek_press,
                    &mut actions,
                );
            });
        });
    }
    let release_seek = !seek_seen || cancel_seek || ui.input(|input| input.pointer.any_released());
    ui.data_mut(|data| {
        if release_seek {
            data.remove::<SeekPress>(seek_key);
        } else if let Some(press) = seek_press {
            data.insert_temp(seek_key, press);
        }
    });
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
            .selectable(false)
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
    seek_press: &mut Option<SeekPress>,
    out: &mut Vec<Action>,
) -> bool {
    ui.painter()
        .rect(rect, 16, pal.card, pal.hairline(), egui::StrokeKind::Inside);
    let left = rect.left() + 14.0;
    let right = rect.right() - 14.0;
    let top = rect.top() + 12.0;
    let title = c.title.as_deref().filter(|t| !t.trim().is_empty());
    let title_rect = Rect::from_min_max(Pos2::new(left, top), Pos2::new(right - 76.0, top + 20.0));
    let header = Rect::from_min_max(rect.min, Pos2::new(title_rect.right(), title_rect.bottom()));
    let drag = ui.interact(header, ui.id().with("drag"), Sense::drag());
    if drag.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    label(
        ui,
        title_rect,
        title.unwrap_or(&c.source_name),
        13.0,
        pal.ink,
    );
    let expand = Rect::from_min_size(Pos2::new(right - 68.0, top - 3.0), Vec2::splat(24.0));
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
    let hide = Rect::from_min_size(Pos2::new(right - 44.0, top - 3.0), Vec2::splat(24.0));
    if button(ui, hide, Glyph::Hide, text(lang, Key::AudioHide), true, pal).clicked() {
        out.push(Action::HidePlayer);
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
    let mut seek_seen = false;
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
        let hit = Rect::from_min_max(Pos2::new(start.x, y - 12.0), Pos2::new(end.x, y + 12.0));
        if let Some(target) = c.seek.filter(|target| {
            target.min_ms <= target.max_ms
                && target.max_ms <= duration
                && c.position_ms.is_some_and(|position| position <= duration)
        }) {
            let response = ui.interact(hit, ui.id().with("seek"), Sense::click());
            response.widget_info(|| {
                egui::WidgetInfo::slider(true, elapsed as f64 / 1_000.0, text(lang, Key::AudioSeek))
            });
            ui.ctx().accesskit_node_builder(response.id, |node| {
                use egui::accesskit::Action;
                node.set_min_numeric_value(target.min_ms as f64 / 1_000.0);
                node.set_max_numeric_value(target.max_ms as f64 / 1_000.0);
                node.remove_action(Action::Click);
                node.add_action(Action::SetValue);
                if elapsed < target.max_ms {
                    node.add_action(Action::Increment);
                }
                if elapsed > target.min_ms {
                    node.add_action(Action::Decrement);
                }
            });
            let mut seek_to = None;
            if (response.is_pointer_button_down_on()
                || response.clicked_by(egui::PointerButton::Primary))
                && ui.input(|input| input.pointer.button_pressed(egui::PointerButton::Primary))
            {
                *seek_press = Some(SeekPress {
                    widget_id: response.id,
                    target,
                    key: None,
                });
            }
            if response.clicked_by(egui::PointerButton::Primary) {
                let press = seek_press.take();
                if let (Some(press), Some(position)) = (press, response.interact_pointer_pos()) {
                    if press.widget_id == response.id && press.target == target {
                        let fraction = ((position.x - start.x) / (end.x - start.x)).clamp(0.0, 1.0);
                        let position_ms = (f64::from(fraction) * duration as f64).round() as u64;
                        seek_to = Some(position_ms.clamp(target.min_ms, target.max_ms));
                    }
                }
            }
            if let Some(position_ms) =
                seek_to.or_else(|| seek_without_pointer(ui, &response, target, elapsed, seek_press))
            {
                out.push(Action::Seek {
                    stream_id: c.stream_id.clone(),
                    target,
                    position_ms,
                });
            }
            seek_seen = seek_press
                .is_some_and(|press| press.widget_id == response.id && press.target == target);
            response
                .on_hover_cursor(egui::CursorIcon::PointingHand)
                .on_hover_text(text(lang, Key::AudioSeek));
        } else {
            let hint = text(lang, Key::AudioSeekUnavailable);
            let response = ui.interact(
                hit,
                ui.id().with("seek-unavailable"),
                Sense::focusable_noninteractive(),
            );
            response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, hint));
            response.on_hover_text(hint);
        }
    }
    let y = rect.bottom() - 73.0;
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
    let gain_top = rect.bottom() - 37.0;
    let gain_rect = Rect::from_min_max(
        Pos2::new(left, gain_top),
        Pos2::new(right - 48.0, gain_top + 24.0),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(gain_rect), |ui| {
        ui.spacing_mut().slider_width = gain_rect.width();
        ui.spacing_mut().interact_size.y = 24.0;
        ui.visuals_mut().widgets.inactive.bg_fill = pal.ink2;
        let mut gain = c.gain_percent.min(100);
        let response = ui.add(egui::Slider::new(&mut gain, 0..=100).show_value(false));
        response.widget_info(|| {
            egui::WidgetInfo::slider(true, f64::from(gain), text(lang, Key::AudioGain))
        });
        if response.changed() {
            out.push(Action::SetGain {
                stream_id: c.stream_id.clone(),
                percent: gain,
            });
        }
        response.on_hover_text(text(lang, Key::AudioGain));
    });
    label(
        ui,
        Rect::from_min_size(
            Pos2::new(right - 40.0, gain_top + 4.0),
            Vec2::new(40.0, 18.0),
        ),
        &format!("{}%", c.gain_percent.min(100)),
        11.0,
        pal.ink2,
    );
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
    seek_seen
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
    Hide,
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
        Glyph::Hide => line(point(-4.0, 0.0), point(4.0, 0.0)),
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
    let max_height = work_area(ctx).map_or(720.0, |work| (work.height() - 32.0).max(48.0));
    let content_height = if model.cards.is_empty() {
        24.0 + empty_height(ctx, WIDTH - 24.0, lang)
    } else {
        height(&model.cards)
    };
    let size = Vec2::new(WIDTH, content_height.min(max_height));
    let mut actions = Vec::new();
    let builder = egui::ViewportBuilder::default()
        .with_title(text(lang, Key::AudioPlayer))
        .with_inner_size(size)
        .with_min_inner_size([225.0, 48.0])
        .with_resizable(false)
        .with_decorations(false)
        .with_transparent(false)
        .with_always_on_top()
        .with_taskbar(false);
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of("audio-player"),
        builder,
        |ui, class| {
            if ui.input(|i| i.viewport().close_requested()) {
                model.open = false;
            }
            if class != egui::ViewportClass::EmbeddedWindow {
                let positioned = ui.data_mut(|d| {
                    d.get_temp::<bool>(egui::Id::new("audio-positioned"))
                        .unwrap_or(false)
                });
                if !positioned {
                    if let Some(work) = work_area(ui.ctx()) {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                                (work.right() - size.x - 16.0).max(work.left()),
                                (work.bottom() - size.y - 16.0).max(work.top()),
                            )));
                        ui.data_mut(|d| d.insert_temp(egui::Id::new("audio-positioned"), true));
                    }
                }
                ui.ctx()
                    .send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
            // egui 0.36 already supplies the viewport's root Ui (and an embedded window
            // on unsupported backends). Render into it, never create another window.
            egui::CentralPanel::default()
                .frame(
                    egui::Frame::new()
                        .fill(Palette::of(theme).card)
                        .inner_margin(12.0),
                )
                .show(ui, |ui| {
                    if content_height > size.y {
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, false])
                            .show(ui, |ui| {
                                actions = draw(ui, &model.cards, theme, lang);
                            });
                    } else {
                        actions = draw(ui, &model.cards, theme, lang);
                    }
                });
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
