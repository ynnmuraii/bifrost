//! The remote-audio player: one opaque surface with a quiet native header, album art or an
//! honest fallback, title and artist, the timeline, the transport and one volume row.
//!
//! Pure drawing: every effect leaves as an [`Action`]. Nothing here owns playback state, so a
//! paused stream cannot grow synthetic progress, and no gesture here can turn audio on. The
//! native window itself is a resizable viewport (a real corner grip, no inner scrolling): the
//! layout adapts to whatever rectangle the window manager reports.

use eframe::egui::{
    self, Color32, CornerRadius, CursorIcon, FontFamily, Pos2, Rect, Sense, Shape, Stroke, Vec2,
};
use ui_text::{text, Key, Lang};

use super::{
    theme::{fam, font},
    Palette,
};
use crate::audio_player::{
    AudioPlayer, AudioPlayerAction as Action, AudioPlayerCard, AudioSeekTarget, PlayerMode,
    StreamState,
};
use crate::gui_settings::{
    PlayerGeometry, Theme, PLAYER_DEFAULT_HEIGHT, PLAYER_DEFAULT_WIDTH, PLAYER_MIN_HEIGHT,
    PLAYER_MIN_WIDTH,
};

/// The narrowest the player may be dragged: below this the header controls would collide.
pub const MIN_WIDTH: f32 = PLAYER_MIN_WIDTH as f32;
/// The shortest it may be dragged while still showing a title, transport and volume.
pub const MIN_HEIGHT: f32 = PLAYER_MIN_HEIGHT as f32;
/// The fresh-window size.
pub const DEFAULT_WIDTH: f32 = PLAYER_DEFAULT_WIDTH as f32;
/// The same.
pub const DEFAULT_HEIGHT: f32 = PLAYER_DEFAULT_HEIGHT as f32;

/// The single outer radius of the surface.
const RADIUS: u8 = 16;
/// The radius of the album square.
const ART_RADIUS: u8 = 8;
/// The inset of every edge.
const PAD: f32 = 14.0;
/// The space between the blocks of one card.
const GAP: f32 = 8.0;
/// One stream of the compact list that is not the active card.
const ROW_HEIGHT: f32 = 36.0;
/// The header strip.
const HEADER_HEIGHT: f32 = 26.0;
/// A header control's hit square.
const BUTTON: f32 = 30.0;
/// The corner grip, and the room kept free for it at the bottom right.
const GRIP: f32 = 16.0;
/// How long a resize must hold still before it is written to `gui.toml`.
const GEOMETRY_SETTLE_SECONDS: f64 = 0.4;

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

/// The preferred height of one card of the list: the compact player with a timeline, or the
/// version without one for a stream that has no media session.
fn card_height(card: &AudioPlayerCard) -> f32 {
    if has_progress(card) {
        236.0
    } else {
        198.0
    }
}

/// The preferred window height for `cards`, before any manual resize.
pub fn height(cards: &[AudioPlayerCard]) -> f32 {
    if cards.is_empty() {
        return 72.0;
    }
    2.0 * PAD
        + cards.iter().map(card_height).sum::<f32>()
        + GAP * cards.len().saturating_sub(1) as f32
}

/// How the surface is arranged at the rectangle it was given.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    /// Art beside the title, timeline under, transport, one volume row.
    Compact,
    /// Art on the left, the whole column of controls on the right.
    Wide,
    /// A large square above the title, timeline, transport and volume.
    Tall,
}

struct SeekRow {
    hit: Rect,
    from: Pos2,
    to: Pos2,
    elapsed: Rect,
    duration: Rect,
}

struct VolumeRow {
    slider: Rect,
    percent: Rect,
    mute: Rect,
}

struct CardLayout {
    source: Rect,
    expand: Rect,
    hide: Rect,
    stop: Rect,
    art: Option<Rect>,
    title: Rect,
    artist: Rect,
    title_size: f32,
    artist_size: f32,
    seek: Option<SeekRow>,
    prev: Rect,
    play: Rect,
    next: Rect,
    play_d: f32,
    volume: VolumeRow,
}

struct Stack {
    volume: Rect,
    controls: Rect,
    seek: Option<Rect>,
    text: Rect,
}

#[derive(Clone, Copy)]
struct StackMetrics {
    has_seek: bool,
    play_d: f32,
    volume_h: f32,
    gap: f32,
    tight: bool,
}

/// The layout follows the rectangle it is given, never the expand flag: a window dragged tall
/// is a portrait, a wide one puts the cover left. Only the transport and the cover grow.
fn mode_for(rect: Rect) -> Mode {
    if rect.height() >= 360.0 && rect.height() >= rect.width() * 1.12 {
        return Mode::Tall;
    }
    if rect.width() >= 560.0 && rect.width() >= rect.height() * 1.15 {
        return Mode::Wide;
    }
    if rect.height() >= 320.0 && rect.height() >= rect.width() * 1.02 {
        return Mode::Tall;
    }
    Mode::Compact
}

/// One column from `top`: a text band, then the timeline (when the stream has one), the
/// transport and one volume row, at fixed gaps. Every block is derived from the window's real
/// rectangle, so extra height never lands between the artist and the seek bar.
fn stack_at(top: f32, column: Rect, right_edge: f32, text_h: f32, metrics: StackMetrics) -> Stack {
    let StackMetrics {
        has_seek,
        play_d,
        volume_h,
        gap,
        tight,
    } = metrics;
    let text = Rect::from_min_max(
        Pos2::new(column.left(), top),
        Pos2::new(column.right(), top + text_h),
    );
    let seek_h = if tight { 26.0 } else { 30.0 };
    let inner_gap = if tight { 6.0 } else { 12.0 };
    let mut y = top + text_h + gap;
    let seek = has_seek.then(|| {
        let rect = Rect::from_min_size(
            Pos2::new(column.left(), y),
            Vec2::new(column.width(), seek_h),
        );
        y += seek_h + inner_gap;
        rect
    });
    let controls_h = play_d.max(44.0);
    let controls = Rect::from_min_size(
        Pos2::new(column.left(), y),
        Vec2::new(column.width(), controls_h),
    );
    y += controls_h + inner_gap;
    let volume = Rect::from_min_max(
        Pos2::new(column.left(), y),
        Pos2::new(right_edge, y + volume_h),
    );
    Stack {
        volume,
        controls,
        seek,
        text,
    }
}

/// Places every block of one card inside `rect` (the already-inset content rectangle), so that
/// no two blocks overlap at any size the window allows. The tight pass drops the artwork first,
/// then shrinks the transport and the volume row, never the controls themselves.
fn layout(rect: Rect, card: &AudioPlayerCard) -> CardLayout {
    let inner = rect;
    let header = Rect::from_min_size(inner.min, Vec2::new(inner.width(), HEADER_HEIGHT));
    let stop = Rect::from_center_size(
        Pos2::new(inner.right() - BUTTON / 2.0, header.center().y),
        Vec2::splat(BUTTON),
    );
    let hide = Rect::from_center_size(
        stop.center() - Vec2::new(BUTTON + 4.0, 0.0),
        Vec2::splat(BUTTON),
    );
    let expand = Rect::from_center_size(
        hide.center() - Vec2::new(BUTTON + 4.0, 0.0),
        Vec2::splat(BUTTON),
    );
    let source = Rect::from_min_max(
        Pos2::new(inner.left(), header.top()),
        Pos2::new((expand.left() - 8.0).max(inner.left()), header.bottom()),
    );
    let content = Rect::from_min_max(Pos2::new(inner.left(), header.bottom() + GAP), inner.max);
    let right_edge = inner.right() - GRIP;
    let has_seek = has_progress(card);
    let mode = mode_for(rect);
    let (title_size, artist_size) = match mode {
        Mode::Compact => (17.0, 13.0),
        Mode::Tall | Mode::Wide => (20.0, 14.0),
    };
    let text_h = title_size + 6.0 + artist_size + 4.0;
    let gap_meta = if mode == Mode::Compact { 14.0 } else { 24.0 };
    let mut play_d = if mode == Mode::Compact { 48.0 } else { 56.0 };
    let mut volume_h = 24.0;
    let mut tight = false;

    // The cover is the only block that may grow (or disappear); the rest keeps one readable
    // rhythm, and any spare height is left outside the whole group rather than inside it.
    let (art, column, stack) = loop {
        let gap_now = if tight { 6.0 } else { gap_meta };
        let metrics = StackMetrics {
            has_seek,
            play_d,
            volume_h,
            gap: gap_now,
            tight,
        };
        let seek_h = if tight { 26.0 } else { 30.0 };
        let inner_gap = if tight { 6.0 } else { 12.0 };
        let rest = |play_d: f32, volume_h: f32| {
            gap_now
                + if has_seek { seek_h + inner_gap } else { 0.0 }
                + play_d.max(44.0)
                + inner_gap
                + volume_h
        };
        let fits = |head: f32, play_d: f32, volume_h: f32| {
            head + rest(play_d, volume_h) <= content.height() + 0.5
        };
        let mut chosen = None;
        match mode {
            Mode::Compact => {
                for art_h in [72.0_f32, 56.0] {
                    if fits(art_h.max(text_h), play_d, volume_h) {
                        let row_h = art_h.max(text_h);
                        let group_h = row_h + rest(play_d, volume_h);
                        let top = content.top() + ((content.height() - group_h) / 2.0).max(0.0);
                        let art =
                            Rect::from_min_size(Pos2::new(content.left(), top), Vec2::splat(art_h));
                        let column = Rect::from_min_max(
                            Pos2::new(art.right() + 12.0, top),
                            Pos2::new(content.right(), top + row_h),
                        );
                        // Only metadata sits beside the cover; playback rows span the surface.
                        let stack = stack_at(top, content, right_edge, row_h, metrics);
                        chosen = Some((Some(art), column, stack));
                        break;
                    }
                }
                if chosen.is_none() && tight && fits(text_h, play_d, volume_h) {
                    let group_h = text_h + rest(play_d, volume_h);
                    let top = content.top() + ((content.height() - group_h) / 2.0).max(0.0);
                    let column = Rect::from_min_max(
                        Pos2::new(content.left(), top),
                        Pos2::new(content.right(), top + text_h),
                    );
                    let stack = stack_at(top, column, right_edge, text_h, metrics);
                    chosen = Some((None, column, stack));
                }
            }
            Mode::Tall => {
                let room = (content.height() - rest(play_d, volume_h) - 16.0 - text_h).max(0.0);
                let size = (content.height() * 0.62)
                    .min(inner.width())
                    .min(380.0)
                    .min(room);
                if size >= 72.0 {
                    let group_h = size + 16.0 + text_h + rest(play_d, volume_h);
                    let top = content.top() + ((content.height() - group_h) / 2.0).max(0.0);
                    let art = Rect::from_min_size(
                        Pos2::new(inner.center().x - size / 2.0, top),
                        Vec2::splat(size),
                    );
                    let column = Rect::from_min_max(
                        Pos2::new(inner.left(), art.bottom() + 16.0),
                        Pos2::new(inner.right(), art.bottom() + 16.0 + text_h),
                    );
                    let stack = stack_at(column.top(), column, right_edge, text_h, metrics);
                    chosen = Some((Some(art), column, stack));
                }
            }
            Mode::Wide => {
                let art_size = (inner.width() * 0.42).min(content.height()).min(260.0);
                let column_left = content.left() + art_size + 20.0;
                if art_size >= 72.0 && content.right() - column_left >= 180.0 {
                    let column_h = text_h + rest(play_d, volume_h);
                    let top = content.top() + ((content.height() - column_h) / 2.0).max(0.0);
                    let art = Rect::from_min_size(
                        Pos2::new(
                            content.left(),
                            content.top() + ((content.height() - art_size) / 2.0).max(0.0),
                        ),
                        Vec2::splat(art_size),
                    );
                    let column = Rect::from_min_max(
                        Pos2::new(column_left, top),
                        Pos2::new(content.right(), top + text_h),
                    );
                    let stack = stack_at(top, column, right_edge, text_h, metrics);
                    chosen = Some((
                        Some(art),
                        Rect::from_min_max(Pos2::new(column_left, top), content.max),
                        stack,
                    ));
                }
            }
        }
        if let Some(chosen) = chosen {
            break chosen;
        }
        if !tight {
            tight = true;
            play_d = 40.0;
            volume_h = 18.0;
            continue;
        }
        // Nothing else fits: keep the controls, give up the cover and the timeline.
        let top = content.top();
        let row_h = text_h.min(content.height().max(0.0));
        let stack = stack_at(
            top,
            content,
            right_edge,
            row_h,
            StackMetrics {
                has_seek: false,
                play_d: 40.0,
                volume_h: 20.0,
                gap: GAP,
                tight: true,
            },
        );
        break (None, content, stack);
    };

    let title_height = (title_size + 6.0).min(stack.text.height());
    let artist_height = (artist_size + 4.0).min((stack.text.height() - title_height).max(0.0));
    let text_left = match (mode, art) {
        (Mode::Tall, Some(art)) => art.left(),
        _ => column.left(),
    };
    let title = Rect::from_min_max(
        Pos2::new(text_left, stack.text.top()),
        Pos2::new(column.right(), stack.text.top() + title_height),
    );
    let artist = Rect::from_min_max(
        Pos2::new(text_left, title.bottom()),
        Pos2::new(column.right(), title.bottom() + artist_height),
    );

    let seek = stack.seek.map(|band| {
        let track_y = band.top() + 9.0;
        SeekRow {
            hit: Rect::from_min_max(
                Pos2::new(band.left(), band.top()),
                Pos2::new(band.right(), band.top() + 22.0),
            ),
            from: Pos2::new(band.left(), track_y),
            to: Pos2::new(band.right(), track_y),
            elapsed: Rect::from_min_max(
                Pos2::new(band.left(), band.top() + 15.0),
                Pos2::new(band.left() + 44.0, band.bottom()),
            ),
            duration: Rect::from_min_max(
                Pos2::new(band.right() - 44.0, band.top() + 15.0),
                Pos2::new(band.right(), band.bottom()),
            ),
        }
    });

    let controls = stack.controls;
    let step = (play_d / 2.0 + 48.0).min((controls.width() - play_d) / 2.0 - 20.0);
    let play = Rect::from_center_size(controls.center(), Vec2::splat(play_d));
    let prev = Rect::from_center_size(play.center() - Vec2::new(step, 0.0), Vec2::splat(40.0));
    let next = Rect::from_center_size(play.center() + Vec2::new(step, 0.0), Vec2::splat(40.0));

    let volume_rect = stack.volume;
    let mute = Rect::from_center_size(
        Pos2::new(volume_rect.left() + 15.0, volume_rect.center().y),
        Vec2::splat(30.0),
    );
    let percent = Rect::from_min_max(
        Pos2::new(volume_rect.right() - 36.0, volume_rect.top()),
        Pos2::new(volume_rect.right(), volume_rect.bottom()),
    );
    let slider = Rect::from_min_max(
        Pos2::new(mute.right() + 8.0, volume_rect.top() + 2.0),
        Pos2::new(percent.left() - 10.0, volume_rect.bottom() - 2.0),
    );

    CardLayout {
        source,
        expand,
        hide,
        stop,
        art,
        title,
        artist,
        title_size,
        artist_size,
        seek,
        prev,
        play,
        next,
        play_d,
        volume: VolumeRow {
            slider,
            percent,
            mute,
        },
    }
}

/// The rectangles of the cards inside `area`; one card fills the surface, a stack that does not
/// fit keeps its primary card full and lists the rest as rows — never a scrollbar. A plan is two
/// rectangles and a rule, yielded card by card: painting allocates nothing.
#[derive(Clone, Copy)]
struct Plan {
    inner: Rect,
    mode: PlanMode,
}

#[derive(Clone, Copy)]
enum PlanMode {
    /// The only card fills the surface at whatever size the window has.
    Fill,
    /// Every card at its preferred height.
    Full,
    /// The active card, then one row per remaining stream.
    Stack { primary: f32, row_h: f32 },
}

impl Plan {
    fn iter<'a>(&'a self, cards: &'a [AudioPlayerCard]) -> impl Iterator<Item = (Rect, bool)> + 'a {
        let mut y = self.inner.top();
        cards.iter().enumerate().map(move |(index, card)| {
            let (height, row) = match self.mode {
                PlanMode::Fill => (self.inner.height(), false),
                PlanMode::Full => (card_height(card), false),
                PlanMode::Stack { primary, row_h } => {
                    if index == 0 {
                        (primary, false)
                    } else {
                        (row_h, true)
                    }
                }
            };
            let rect = Rect::from_min_size(
                Pos2::new(self.inner.left(), y),
                Vec2::new(self.inner.width(), height),
            );
            y += height + GAP;
            (rect, row)
        })
    }
}

fn plan(area: Rect, cards: &[AudioPlayerCard]) -> Plan {
    let inner = area.shrink(PAD);
    let n = cards.len();
    if n == 1 {
        return Plan {
            inner,
            mode: PlanMode::Fill,
        };
    }
    let gaps = GAP * n.saturating_sub(1) as f32;
    let preferred: f32 = cards.iter().map(card_height).sum::<f32>() + gaps;
    if preferred <= inner.height() + 0.5 {
        return Plan {
            inner,
            mode: PlanMode::Full,
        };
    }
    let rows = n - 1;
    let primary_min = 150.0;
    let rows_room = (inner.height() - primary_min - gaps).max(0.0);
    let row_h = (rows_room / rows as f32).clamp(28.0, ROW_HEIGHT);
    let primary = (inner.height() - row_h * rows as f32 - gaps).max(120.0);
    Plan {
        inner,
        mode: PlanMode::Stack { primary, row_h },
    }
}

/// Draw the player at whatever rectangle it has; effects leave as typed intents.
pub fn draw(
    ui: &mut egui::Ui,
    cards: &[AudioPlayerCard],
    theme: Theme,
    lang: Lang,
    expanded: bool,
    mode: PlayerMode,
) -> Vec<Action> {
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
    let area = ui.max_rect().intersect(ui.clip_rect());
    // One opaque surface. There is no second card frame inside it: the window *is* the card.
    ui.painter().rect_filled(area, RADIUS, pal.card);
    prune_art_textures(ui.ctx(), cards);
    let mut actions = Vec::new();
    let seek_key = ui.id().with("seek-press");
    let mut seek_press = ui.data(|data| data.get_temp::<SeekPress>(seek_key));
    let cancel_seek = ui.input(|input| input.key_pressed(egui::Key::Escape) || !input.focused);
    if cancel_seek {
        seek_press = None;
    }
    let mut seek_seen = false;

    if cards.is_empty() {
        draw_empty(ui, area, &pal, lang, &mut actions);
    } else {
        let plan = plan(area, cards);
        for (index, (card, (rect, row))) in cards.iter().zip(plan.iter(cards)).enumerate() {
            if index > 0 {
                ui.painter().hline(
                    rect.x_range(),
                    rect.top() - GAP / 2.0,
                    Stroke::new(1.0, pal.hair),
                );
            }
            ui.push_id(&card.stream_id, |ui| {
                if row {
                    draw_row(ui, rect, card, &pal, lang, &mut actions);
                } else {
                    let mut card_palette = pal;
                    if card.background {
                        // Dim emphasis, not the surface: a background stream stays listed.
                        card_palette.ink = pal.ink2;
                        card_palette.accent_ink = pal.ink2;
                    }
                    seek_seen |= draw_card(
                        ui,
                        rect,
                        card,
                        CardPresentation {
                            pal: &card_palette,
                            lang,
                            expanded,
                            mode: (index == 0).then_some(mode),
                        },
                        &mut seek_press,
                        &mut actions,
                    );
                }
            });
        }
    }
    draw_grip(ui, area, &pal);
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

fn draw_empty(ui: &mut egui::Ui, area: Rect, pal: &Palette, lang: Lang, actions: &mut Vec<Action>) {
    let inner = area.shrink(PAD);
    let close = Rect::from_min_size(
        Pos2::new(inner.right() - BUTTON, inner.top()),
        Vec2::splat(BUTTON),
    );
    let drag = ui.interact(
        Rect::from_min_max(inner.min, Pos2::new(close.left() - 8.0, inner.bottom())),
        ui.id().with("empty-drag"),
        Sense::drag(),
    );
    if drag.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    let text_rect = Rect::from_min_max(
        Pos2::new(inner.left(), inner.center().y - 9.0),
        Pos2::new(close.left() - 12.0, inner.bottom()),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(text_rect), |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(text(lang, Key::AudioEmpty))
                    .font(font(13.0, fam::sans_medium()))
                    .color(pal.ink2),
            )
            .selectable(false)
            .wrap(),
        );
    });
    if icon_button(
        ui,
        close,
        Glyph::Hide,
        text(lang, Key::AudioHide),
        true,
        pal,
        Kind::Plain(18.0),
    )
    .clicked()
    {
        actions.push(Action::HidePlayer);
    }
}

/// One non-active stream: its identity, and the controls that still act on it.
fn draw_row(
    ui: &mut egui::Ui,
    rect: Rect,
    c: &AudioPlayerCard,
    pal: &Palette,
    lang: Lang,
    out: &mut Vec<Action>,
) {
    let title = c.title.as_deref().filter(|t| !t.trim().is_empty());
    let button = (rect.height() - 4.0).clamp(22.0, BUTTON);
    let gap = 4.0;
    let text_right = rect.right() - 3.0 * button - 2.0 * gap - 2.0;
    let text_rect = Rect::from_min_max(
        Pos2::new(rect.left(), rect.top() + 1.0),
        Pos2::new(text_right, rect.bottom() - 1.0),
    );
    let name = title.unwrap_or(&c.source_name);
    let sub = if title.is_some() {
        c.subtitle
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map_or_else(
                || c.source_name.clone(),
                |s| format!("{s} · {}", c.source_name),
            )
    } else {
        text(lang, Key::AudioSystem).to_owned()
    };
    let line_h = (text_rect.height() / 2.0).max(9.0);
    label_tooltip(
        ui,
        Rect::from_min_size(text_rect.min, Vec2::new(text_rect.width(), line_h)),
        name,
        11.5,
        fam::sans_medium(),
        if c.background { pal.ink2 } else { pal.ink },
    );
    label(
        ui,
        Rect::from_min_size(
            Pos2::new(text_rect.left(), text_rect.top() + line_h),
            Vec2::new(text_rect.width(), text_rect.height() - line_h),
        ),
        &sub,
        9.5,
        fam::sans(),
        pal.ink2,
    );
    let play = Rect::from_center_size(
        Pos2::new(rect.right() - button / 2.0, rect.center().y),
        Vec2::splat(button),
    );
    let mute = Rect::from_center_size(
        play.center() - Vec2::new(button + gap, 0.0),
        Vec2::splat(button),
    );
    let stop = Rect::from_center_size(
        mute.center() - Vec2::new(button + gap, 0.0),
        Vec2::splat(button),
    );
    if c.has_media_session
        && icon_button(
            ui,
            play,
            if c.playing { Glyph::Pause } else { Glyph::Play },
            text(
                lang,
                if c.playing {
                    Key::AudioPause
                } else {
                    Key::AudioPlay
                },
            ),
            c.can_play_pause,
            pal,
            Kind::Plain(16.0),
        )
        .clicked()
    {
        out.push(Action::PlayPause(c.stream_id.clone()));
    }
    if icon_button(
        ui,
        mute,
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
        Kind::Plain(16.0),
    )
    .clicked()
    {
        out.push(Action::SetMuted {
            stream_id: c.stream_id.clone(),
            muted: !c.muted,
        });
    }
    if icon_button(
        ui,
        stop,
        Glyph::Stop,
        text(lang, Key::AudioStop),
        true,
        pal,
        Kind::Plain(15.0),
    )
    .clicked()
    {
        out.push(Action::Stop(c.stream_id.clone()));
    }
}

struct CardPresentation<'a> {
    pal: &'a Palette,
    lang: Lang,
    expanded: bool,
    mode: Option<PlayerMode>,
}

fn draw_card(
    ui: &mut egui::Ui,
    rect: Rect,
    c: &AudioPlayerCard,
    presentation: CardPresentation<'_>,
    seek_press: &mut Option<SeekPress>,
    out: &mut Vec<Action>,
) -> bool {
    let CardPresentation {
        pal,
        lang,
        expanded,
        mode,
    } = presentation;
    let mut l = layout(rect, c);
    if let Some(mode) = mode {
        let hit =
            Rect::from_center_size(l.expand.center() - Vec2::new(39.0, 0.0), Vec2::splat(40.0));
        l.source.max.x = (hit.left() - 8.0).max(l.source.left());
        mode_button(ui, hit, mode, pal, lang, out);
    }
    let unavailable =
        c.state == StreamState::Failed && c.path.is_some_and(|path| path.has_diagnostic());
    let title = c
        .title
        .as_deref()
        .filter(|t| !t.trim().is_empty())
        .unwrap_or(text(lang, Key::AudioSystem));
    let artist = if unavailable {
        text(lang, Key::AudioPathUnavailable).to_owned()
    } else if c.title.as_deref().is_some_and(|t| !t.trim().is_empty()) {
        c.subtitle
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .map_or_else(|| c.source_name.clone(), str::to_owned)
    } else {
        c.source_name.clone()
    };

    // Header: the source, always draggable, and the three quiet window controls.
    let drag = ui.interact(
        Rect::from_min_max(rect.min, Pos2::new(l.source.right(), l.source.bottom())),
        ui.id().with("drag"),
        Sense::drag(),
    );
    if drag.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
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
    let path_tooltip = c.path.map(|path| {
        format!(
            "{}\n{}",
            text(lang, state_key),
            path.tooltip(lang, &c.source_name)
        )
    });
    let health_text = path_tooltip.as_deref().unwrap_or(text(lang, state_key));
    label(
        ui,
        l.source,
        &c.source_name,
        11.0,
        fam::sans_medium(),
        pal.ink2,
    );
    let source_response = ui.interact(l.source, ui.id().with("source"), Sense::hover());
    source_response
        .widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, health_text));
    source_response.on_hover_text(health_text);

    if icon_button(
        ui,
        l.expand,
        if expanded {
            Glyph::Collapse
        } else {
            Glyph::Expand
        },
        text(
            lang,
            if expanded {
                Key::AudioCollapse
            } else {
                Key::AudioExpand
            },
        ),
        true,
        pal,
        Kind::Plain(17.0),
    )
    .clicked()
    {
        out.push(Action::ToggleExpanded);
    }
    if icon_button(
        ui,
        l.hide,
        Glyph::Hide,
        text(lang, Key::AudioHide),
        true,
        pal,
        Kind::Plain(18.0),
    )
    .clicked()
    {
        out.push(Action::HidePlayer);
    }
    if icon_button(
        ui,
        l.stop,
        Glyph::Stop,
        text(lang, Key::AudioStop),
        true,
        pal,
        Kind::Plain(16.0),
    )
    .clicked()
    {
        out.push(Action::Stop(c.stream_id.clone()));
    }

    if let Some(rect) = l.art {
        ui.painter().rect_filled(rect, ART_RADIUS, pal.bg2);
        let decoded = c.artwork.as_ref().filter(|art| {
            art.id == c.artwork_id.unwrap_or(art.id)
                && art.width > 0
                && art.height > 0
                && art.rgba.len() == art.width as usize * art.height as usize * 4
        });
        if let Some(artwork) = decoded {
            let texture = artwork_texture(ui.ctx(), artwork);
            let image_size = Vec2::new(artwork.width as f32, artwork.height as f32);
            let scale = (rect.width() / image_size.x).min(rect.height() / image_size.y);
            let image_rect = Rect::from_center_size(rect.center(), image_size * scale);
            // The image is one textured rect, painted from the uploaded handle: no widget is
            // allocated per frame, and the hairline keeps the square seated on the surface.
            ui.painter().image(
                texture.id(),
                image_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
            ui.painter().rect_stroke(
                rect,
                ART_RADIUS,
                Stroke::new(1.0, pal.hair),
                egui::StrokeKind::Inside,
            );
        } else {
            // No image is not a reason to invent one: an honest neutral mark that scales with
            // the square (roughly a quarter of its edge, never larger than 64 points).
            super::widgets::paint_icon(
                ui.painter(),
                super::widgets::Icon::Headphones,
                rect.center(),
                (rect.width() * 0.24).clamp(18.0, 64.0),
                pal.ink2.gamma_multiply(0.65),
            );
        }
    }
    label_tooltip(
        ui,
        l.title,
        title,
        l.title_size,
        fam::sans_semibold(),
        pal.ink,
    );
    if l.artist.height() > 0.0 {
        label_tooltip(ui, l.artist, &artist, l.artist_size, fam::sans(), pal.ink2);
    }

    let mut seek_seen = false;
    if let Some(seek) = l.seek {
        seek_seen = draw_seek(ui, seek, c, pal, lang, seek_press, out);
    }

    if c.has_media_session {
        if icon_button(
            ui,
            l.prev,
            Glyph::Previous,
            text(lang, Key::AudioPrevious),
            c.can_prev,
            pal,
            Kind::Plain(20.0),
        )
        .clicked()
        {
            out.push(Action::Previous(c.stream_id.clone()));
        }
        if icon_button(
            ui,
            l.play,
            if c.playing { Glyph::Pause } else { Glyph::Play },
            text(
                lang,
                if c.playing {
                    Key::AudioPause
                } else {
                    Key::AudioPlay
                },
            ),
            c.can_play_pause,
            pal,
            Kind::Circle(l.play_d * 0.52),
        )
        .clicked()
        {
            out.push(Action::PlayPause(c.stream_id.clone()));
        }
        if icon_button(
            ui,
            l.next,
            Glyph::Next,
            text(lang, Key::AudioNext),
            c.can_next,
            pal,
            Kind::Plain(20.0),
        )
        .clicked()
        {
            out.push(Action::Next(c.stream_id.clone()));
        }
    }

    // One real volume row: the functional mute, the slider, the tabular percentage. There is
    // no second decorative speaker icon anywhere on the surface.
    let v = &l.volume;
    let gain = c.gain_percent.min(100);
    let mut new_gain = gain;
    ui.scope_builder(egui::UiBuilder::new().max_rect(v.slider), |ui| {
        ui.spacing_mut().slider_width = v.slider.width();
        ui.spacing_mut().interact_size.y = v.slider.height();
        let visuals = ui.visuals_mut();
        visuals.widgets.inactive.bg_fill = pal.ink2;
        visuals.widgets.hovered.bg_fill = pal.ink2;
        visuals.widgets.active.bg_fill = pal.ink2;
        visuals.selection.bg_fill = pal.accent;
        visuals.slider_trailing_fill = true;
        visuals.handle_shape = egui::style::HandleShape::Circle;
        let response = ui.add(egui::Slider::new(&mut new_gain, 0..=100).show_value(false));
        response.widget_info(|| {
            egui::WidgetInfo::slider(true, f64::from(gain), text(lang, Key::AudioGain))
        });
        response.on_hover_text(text(lang, Key::AudioGain));
    });
    if new_gain != gain {
        out.push(Action::SetGain {
            stream_id: c.stream_id.clone(),
            percent: new_gain,
        });
    }
    label_right(
        ui,
        v.percent,
        &format!("{gain}%"),
        11.0,
        fam::mono(),
        pal.ink2,
    );
    if icon_button(
        ui,
        v.mute,
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
        Kind::Plain(17.0),
    )
    .clicked()
    {
        out.push(Action::SetMuted {
            stream_id: c.stream_id.clone(),
            muted: !c.muted,
        });
    }
    if ui.rect_contains_pointer(v.slider.expand(6.0)) {
        let scroll = ui.input(|i| i.smooth_scroll_delta.y);
        if scroll != 0.0 {
            let stepped = (i32::from(gain) + if scroll > 0.0 { 5 } else { -5 }).clamp(0, 100) as u8;
            out.push(Action::SetGain {
                stream_id: c.stream_id.clone(),
                percent: stepped,
            });
        }
    }
    seek_seen
}

fn draw_seek(
    ui: &mut egui::Ui,
    seek: SeekRow,
    c: &AudioPlayerCard,
    pal: &Palette,
    lang: Lang,
    seek_press: &mut Option<SeekPress>,
    out: &mut Vec<Action>,
) -> bool {
    let duration = c.duration_ms.unwrap_or(1);
    let elapsed = c.position_ms.unwrap_or(0).min(duration);
    label(
        ui,
        seek.elapsed,
        &time(elapsed),
        10.0,
        fam::mono(),
        pal.ink2,
    );
    label_right(
        ui,
        seek.duration,
        &time(duration),
        10.0,
        fam::mono(),
        pal.ink2,
    );
    ui.painter()
        .line_segment([seek.from, seek.to], Stroke::new(3.0, pal.line));
    let played_end = seek.from.lerp(seek.to, elapsed as f32 / duration as f32);
    ui.painter()
        .line_segment([seek.from, played_end], Stroke::new(3.0, pal.accent));
    let target = c.seek.filter(|target| {
        target.min_ms <= target.max_ms
            && target.max_ms <= duration
            && c.position_ms.is_some_and(|position| position <= duration)
    });
    if let Some(target) = target {
        ui.painter().circle_filled(played_end, 4.5, pal.accent);
        let response = ui.interact(seek.hit, ui.id().with("seek"), Sense::click());
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
                    let width = (seek.to.x - seek.from.x).max(1.0);
                    let fraction = ((position.x - seek.from.x) / width).clamp(0.0, 1.0);
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
        let held = seek_press
            .is_some_and(|press| press.widget_id == response.id && press.target == target);
        response
            .on_hover_cursor(CursorIcon::PointingHand)
            .on_hover_text(text(lang, Key::AudioSeek));
        return held;
    }
    let hint = text(lang, Key::AudioSeekUnavailable);
    let response = ui.interact(
        seek.hit,
        ui.id().with("seek-unavailable"),
        Sense::focusable_noninteractive(),
    );
    response.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Label, true, hint));
    response.on_hover_text(hint);
    false
}

/// The native resize affordance: the window manager does the actual resizing, from a real
/// `BeginResize`, so the content never becomes an in-window imitation of a window.
fn draw_grip(ui: &mut egui::Ui, area: Rect, pal: &Palette) {
    let grip = Rect::from_min_max(
        Pos2::new(area.right() - GRIP, area.bottom() - GRIP),
        Pos2::new(area.right() - 2.0, area.bottom() - 2.0),
    );
    let response = ui.interact(grip, ui.id().with("resize-grip"), Sense::drag());
    if response.drag_started_by(egui::PointerButton::Primary) {
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::BeginResize(
                egui::viewport::ResizeDirection::SouthEast,
            ));
    }
    let ink = if response.hovered() || response.dragged() {
        pal.ink2
    } else {
        pal.ink2.gamma_multiply(0.45)
    };
    let p = ui.painter();
    for step in [0.0, 5.0] {
        let end = Pos2::new(grip.right() - step, grip.bottom() - step);
        p.line_segment(
            [
                Pos2::new(grip.right() - step - 6.0, grip.bottom() - step),
                end,
            ],
            Stroke::new(1.5, ink),
        );
        p.line_segment(
            [
                end,
                Pos2::new(grip.right() - step, grip.bottom() - step - 6.0),
            ],
            Stroke::new(1.5, ink),
        );
    }
    response.on_hover_cursor(CursorIcon::ResizeNwSe);
}

fn label(
    ui: &mut egui::Ui,
    rect: Rect,
    value: &str,
    size: f32,
    family: FontFamily,
    color: Color32,
) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
        ui.add(
            egui::Label::new(
                egui::RichText::new(value)
                    .font(font(size, family))
                    .color(color),
            )
            .selectable(false)
            .truncate(),
        );
    });
}

fn label_tooltip(
    ui: &mut egui::Ui,
    rect: Rect,
    value: &str,
    size: f32,
    family: FontFamily,
    color: Color32,
) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    let response = ui
        .scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.add(
                egui::Label::new(
                    egui::RichText::new(value)
                        .font(font(size, family))
                        .color(color),
                )
                .selectable(false)
                .truncate(),
            )
        })
        .inner;
    response.on_hover_text(value);
}

fn label_right(
    ui: &mut egui::Ui,
    rect: Rect,
    value: &str,
    size: f32,
    family: FontFamily,
    color: Color32,
) {
    if rect.width() < 1.0 || rect.height() < 1.0 {
        return;
    }
    // Laid out to the band and then placed by its own width: a right-aligned label must not
    // depend on a right-to-left cursor, which would paint it from the edge outwards.
    let galley = ui.fonts_mut(|fonts| {
        fonts.layout(
            value.to_owned(),
            font(size, family),
            color,
            rect.width().max(1.0),
        )
    });
    let pos = Pos2::new(
        rect.right() - galley.size().x,
        rect.center().y - galley.size().y / 2.0,
    );
    ui.painter().galley(pos, galley, color);
}

fn mode_button(
    ui: &mut egui::Ui,
    rect: Rect,
    mode: PlayerMode,
    pal: &Palette,
    lang: Lang,
    out: &mut Vec<Action>,
) {
    use protocol::ipc::AudioMode;
    let (icon, name, hint, next) = match mode.current {
        Some(AudioMode::MixAll) => (
            Glyph::Mix,
            Key::AudioMixAll,
            Key::AudioPlayerMixAllHint,
            Some(AudioMode::FollowFocus),
        ),
        Some(AudioMode::FollowFocus) => (
            Glyph::Focus,
            Key::AudioFollowFocus,
            Key::AudioPlayerFollowFocusHint,
            Some(AudioMode::MixAll),
        ),
        _ => (
            Glyph::ModeUnknown,
            Key::AudioMode,
            Key::AudioPlayerModeUnavailable,
            None,
        ),
    };
    let enabled = mode.enabled && next.is_some();
    let response = icon_control(
        ui,
        rect,
        icon,
        text(lang, name),
        enabled,
        pal,
        Kind::Plain(20.0),
    )
    .on_hover_ui(|ui| {
        let width =
            (ui.ctx().input(|input| input.content_rect().width()) - 48.0).clamp(100.0, 360.0);
        ui.set_width(width);
        ui.strong(text(lang, name));
        ui.add(
            egui::Label::new(text(
                lang,
                if enabled {
                    hint
                } else {
                    Key::AudioPlayerModeUnavailable
                },
            ))
            .wrap(),
        );
    });
    if response.clicked() {
        if let Some(next) = next {
            out.push(Action::SetMode(next));
        }
    }
}

#[derive(Clone, Copy)]
enum Kind {
    Plain(f32),
    Circle(f32),
}

#[derive(Clone, Copy)]
enum Glyph {
    Previous,
    Play,
    Pause,
    Next,
    Volume,
    Muted,
    Expand,
    Collapse,
    Hide,
    Stop,
    Mix,
    Focus,
    ModeUnknown,
}

fn icon_button(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: Glyph,
    name: &str,
    enabled: bool,
    pal: &Palette,
    kind: Kind,
) -> egui::Response {
    icon_control(ui, rect, icon, name, enabled, pal, kind).on_hover_text(name)
}

fn icon_control(
    ui: &mut egui::Ui,
    rect: Rect,
    icon: Glyph,
    name: &str,
    enabled: bool,
    pal: &Palette,
    kind: Kind,
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
    let active = response.hovered() || response.has_focus() || response.is_pointer_button_down_on();
    match kind {
        Kind::Circle(size) => {
            let fill = if !enabled {
                pal.line
            } else if active {
                pal.accent
            } else {
                pal.ink
            };
            ui.painter()
                .circle_filled(rect.center(), rect.width() / 2.0, fill);
            let ink = if !enabled {
                pal.ink2.gamma_multiply(0.45)
            } else if active {
                pal.on_accent
            } else {
                pal.bg
            };
            glyph(ui, &rect, icon, size, ink);
            if response.has_focus() {
                ui.painter().circle_stroke(
                    rect.center(),
                    rect.width() / 2.0 - 1.5,
                    Stroke::new(2.0, pal.accent),
                );
            }
        }
        Kind::Plain(size) => {
            if enabled && active {
                ui.painter().rect_filled(rect, 9, pal.accent_soft);
                if response.has_focus() {
                    ui.painter().rect_stroke(
                        rect,
                        9,
                        Stroke::new(1.5, pal.accent),
                        egui::StrokeKind::Inside,
                    );
                }
            }
            glyph(
                ui,
                &rect,
                icon,
                size,
                if enabled {
                    pal.ink
                } else {
                    pal.ink2.gamma_multiply(0.4)
                },
            );
        }
    }
    response.on_hover_cursor(if enabled {
        CursorIcon::PointingHand
    } else {
        CursorIcon::Default
    })
}

fn glyph(ui: &egui::Ui, rect: &Rect, icon: Glyph, size: f32, color: Color32) {
    let p = ui.painter();
    let center = rect.center();
    let s = size / 20.0;
    let point = |x: f32, y: f32| center + Vec2::new(x * s, y * s);
    let stroke = Stroke::new((1.7 * s).max(1.1), color);
    let triangle = |points: [(f32, f32); 3]| {
        p.add(Shape::convex_polygon(
            points.iter().map(|(x, y)| point(*x, *y)).collect(),
            color,
            Stroke::NONE,
        ));
    };
    match icon {
        Glyph::Mix => {
            for y in [-5.0, 0.0, 5.0] {
                p.line_segment([point(-7.0, y), point(-2.0, y)], stroke);
                p.line_segment([point(-2.0, y), point(3.0, 0.0)], stroke);
            }
            p.line_segment([point(3.0, 0.0), point(8.0, 0.0)], stroke);
        }
        Glyph::Focus => {
            let cursor = [
                (-5.0, -7.0),
                (-5.0, 6.0),
                (0.0, 2.0),
                (4.0, 8.0),
                (7.0, 6.0),
                (3.0, 0.0),
                (8.0, -1.0),
                (-5.0, -7.0),
            ];
            for pair in cursor.windows(2) {
                p.line_segment(
                    [point(pair[0].0, pair[0].1), point(pair[1].0, pair[1].1)],
                    stroke,
                );
            }
        }
        Glyph::ModeUnknown => {
            p.circle_stroke(center, 7.0 * s, stroke);
            p.line_segment([point(-3.0, 0.0), point(3.0, 0.0)], stroke);
        }
        // The triangle carries half a pixel of optical offset to the right.
        Glyph::Play => triangle([(-3.0, -5.5), (6.5, 0.0), (-3.0, 5.5)]),
        Glyph::Pause => {
            for x in [-2.8, 2.8] {
                p.rect_filled(
                    Rect::from_center_size(point(x, 0.0), Vec2::new(3.0 * s, 11.0 * s)),
                    CornerRadius::same((1.0 * s).max(1.0) as u8),
                    color,
                );
            }
        }
        Glyph::Previous | Glyph::Next => {
            // Previous is a bar on the LEFT with the triangle pointing LEFT; next mirrors it.
            let f = if matches!(icon, Glyph::Previous) {
                1.0
            } else {
                -1.0
            };
            triangle([(4.5 * f, -5.5), (-3.5 * f, 0.0), (4.5 * f, 5.5)]);
            p.line_segment([point(-5.5 * f, -5.5), point(-5.5 * f, 5.5)], stroke);
        }
        Glyph::Hide => {
            p.line_segment([point(-5.0, 0.0), point(5.0, 0.0)], stroke);
        }
        Glyph::Stop => {
            p.line_segment([point(-4.5, -4.5), point(4.5, 4.5)], stroke);
            p.line_segment([point(4.5, -4.5), point(-4.5, 4.5)], stroke);
        }
        Glyph::Expand => {
            // Two arrows pushing out of the centre.
            p.line_segment([point(-1.5, 1.5), point(-6.0, 6.0)], stroke);
            p.line_segment([point(-6.0, 6.0), point(-1.5, 6.0)], stroke);
            p.line_segment([point(-6.0, 6.0), point(-6.0, 1.5)], stroke);
            p.line_segment([point(1.5, -1.5), point(6.0, -6.0)], stroke);
            p.line_segment([point(6.0, -6.0), point(1.5, -6.0)], stroke);
            p.line_segment([point(6.0, -6.0), point(6.0, -1.5)], stroke);
        }
        Glyph::Collapse => {
            p.line_segment([point(-1.5, 1.5), point(-6.0, 6.0)], stroke);
            p.line_segment([point(-1.5, 1.5), point(-1.5, 4.5)], stroke);
            p.line_segment([point(-1.5, 1.5), point(-4.5, 1.5)], stroke);
            p.line_segment([point(1.5, -1.5), point(6.0, -6.0)], stroke);
            p.line_segment([point(1.5, -1.5), point(1.5, -4.5)], stroke);
            p.line_segment([point(1.5, -1.5), point(4.5, -1.5)], stroke);
        }
        Glyph::Volume | Glyph::Muted => {
            p.add(Shape::convex_polygon(
                vec![
                    point(-6.0, -2.2),
                    point(-3.0, -2.2),
                    point(0.5, -5.2),
                    point(0.5, 5.2),
                    point(-3.0, 2.2),
                    point(-6.0, 2.2),
                ],
                color,
                Stroke::NONE,
            ));
            if matches!(icon, Glyph::Muted) {
                p.line_segment([point(3.5, -3.0), point(7.5, 3.0)], stroke);
                p.line_segment([point(7.5, -3.0), point(3.5, 3.0)], stroke);
            } else {
                // One arc each side: the speaker is audible, not an EQ decoration.
                for radius in [3.5_f32, 6.5] {
                    let steps = 6;
                    let mut pts = Vec::with_capacity(steps + 1);
                    for i in 0..=steps {
                        let a = (-48.0 + 96.0 * i as f32 / steps as f32).to_radians();
                        pts.push(point(1.0 + radius * a.cos() * 0.9, radius * a.sin()));
                    }
                    p.add(Shape::line(pts, stroke));
                }
            }
        }
    }
}

/// How many covers may stay uploaded at once; the player shows at most a handful of streams.
const ART_TEXTURES: usize = 8;

/// Drops the uploads of covers that left the player. Runs without allocating in the common
/// case (no artwork, or the same set), and only while every card is painted.
fn prune_art_textures(ctx: &egui::Context, cards: &[AudioPlayerCard]) {
    let mut used = [None; ART_TEXTURES];
    let mut len = 0;
    for card in cards {
        if let Some(id) = card.artwork.as_ref().map(|art| art.id) {
            if len < ART_TEXTURES && !used[..len].contains(&Some(id)) {
                used[len] = Some(id);
                len += 1;
            }
        }
    }
    let key = egui::Id::new("audio-art-textures");
    let previous = ctx.data(|data| data.get_temp::<([Option<[u8; 32]>; ART_TEXTURES], usize)>(key));
    if previous.is_some_and(|(old, old_len)| old_len == len && old[..len] == used[..len]) {
        return;
    }
    if let Some((old, old_len)) = previous {
        for id in old[..old_len].iter().flatten() {
            if !used[..len].contains(&Some(*id)) {
                ctx.data_mut(|data| {
                    data.remove::<egui::TextureHandle>(egui::Id::new(("audio-art-texture", *id)))
                });
            }
        }
    }
    ctx.data_mut(|data| data.insert_temp(key, (used, len)));
}

/// The one upload for a cover hash: a second frame with the same hash reuses the texture.
fn artwork_texture(ctx: &egui::Context, art: &crate::audio_player::Artwork) -> egui::TextureHandle {
    let id = egui::Id::new(("audio-art-texture", art.id));
    if let Some(handle) = ctx.data(|data| data.get_temp::<egui::TextureHandle>(id)) {
        return handle;
    }
    let image = egui::ColorImage::from_rgba_unmultiplied(
        [art.width as usize, art.height as usize],
        &art.rgba,
    );
    let handle = ctx.load_texture("audio-artwork", image, egui::TextureOptions::LINEAR);
    ctx.data_mut(|data| data.insert_temp(id, handle.clone()));
    handle
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

/// A separate native viewport; the window is resizable from its real corner, its size is
/// remembered in `gui.toml`, and immediate mode keeps every action on the GUI thread.
pub fn viewport(
    ctx: &egui::Context,
    model: &mut AudioPlayer,
    geometry: &mut PlayerGeometry,
    theme: Theme,
    lang: Lang,
    mode: PlayerMode,
) -> Vec<Action> {
    let initialised_id = egui::Id::new("audio-player-initialised");
    let pending_id = egui::Id::new("audio-player-pending-size");
    if !model.open {
        ctx.data_mut(|d| {
            d.remove::<bool>(initialised_id);
            d.remove::<Vec2>(pending_id);
        });
        return Vec::new();
    }
    let applied = ctx
        .data(|d| d.get_temp::<bool>(initialised_id))
        .unwrap_or(false);
    let pending = ctx.data(|d| d.get_temp::<Vec2>(pending_id));
    let active = geometry.active();
    let size = Vec2::new(active[0] as f32, active[1] as f32);
    let force = !applied || pending.is_some();
    let max_height = work_area(ctx).map_or(720.0, |work| (work.height() - 32.0).max(MIN_HEIGHT));
    let target = Vec2::new(size.x.max(MIN_WIDTH), size.y.clamp(MIN_HEIGHT, max_height));

    let mut builder = egui::ViewportBuilder::default()
        .with_title(text(lang, Key::AudioPlayer))
        .with_min_inner_size([MIN_WIDTH, MIN_HEIGHT])
        .with_resizable(true)
        .with_decorations(false)
        .with_transparent(false)
        .with_always_on_top()
        .with_taskbar(false);
    if force {
        builder = builder.with_inner_size(target);
    }

    let mut actions = Vec::new();
    ctx.show_viewport_immediate(
        egui::ViewportId::from_hash_of("audio-player"),
        builder,
        |ui, class| {
            if ui.input(|i| i.viewport().close_requested()) {
                if let Some(actual) =
                    ui.data(|d| d.get_temp::<[u16; 2]>(egui::Id::new("audio-player-last-size")))
                {
                    geometry.set_active(actual);
                }
                model.open = false;
            }
            if class != egui::ViewportClass::EmbeddedWindow {
                let positioned = ui
                    .data_mut(|d| d.get_temp::<bool>(egui::Id::new("audio-player-positioned")))
                    .unwrap_or(false);
                if !positioned {
                    if let Some(work) = work_area(ui.ctx()) {
                        ui.ctx()
                            .send_viewport_cmd(egui::ViewportCommand::OuterPosition(Pos2::new(
                                (work.right() - target.x - 16.0).max(work.left()),
                                (work.bottom() - target.y - 16.0).max(work.top()),
                            )));
                        ui.data_mut(|d| {
                            d.insert_temp(egui::Id::new("audio-player-positioned"), true)
                        });
                    }
                }
                if force {
                    ui.ctx()
                        .send_viewport_cmd(egui::ViewportCommand::InnerSize(target));
                }
                if let Some(size) = ui.input(|i| i.viewport().inner_rect.map(|rect| rect.size())) {
                    if size.x >= MIN_WIDTH - 0.5 && size.y >= MIN_HEIGHT - 0.5 {
                        // Compare and store in bounded space: a WM larger than the documented
                        // maximum must not look "new" on every frame and re-write the file.
                        let actual = PlayerGeometry::clamp_size([
                            size.x.round() as u16,
                            size.y.round() as u16,
                        ]);
                        let pending_size = ui.data(|d| d.get_temp::<Vec2>(pending_id));
                        let geometry_id = egui::Id::new("audio-player-geometry-pending");
                        let last_id = egui::Id::new("audio-player-last-size");
                        ui.data_mut(|d| d.insert_temp(last_id, actual));
                        if let Some(pending_size) = pending_size {
                            if (Vec2::new(actual[0] as f32, actual[1] as f32) - pending_size)
                                .length()
                                <= 1.5
                            {
                                ui.data_mut(|d| {
                                    d.remove::<Vec2>(pending_id);
                                    d.remove::<(u16, u16, f64)>(geometry_id);
                                });
                            }
                        } else if *geometry.active_mut() != actual {
                            // Visual size is immediate; the file is written once the drag has
                            // settled, so a 60 fps resize cannot become 60 atomic TOML writes.
                            let now = ui.input(|input| input.time);
                            let seen = ui.data(|d| d.get_temp::<(u16, u16, f64)>(geometry_id));
                            match seen {
                                Some((w, h, since)) if [w, h] == actual => {
                                    if now - since >= GEOMETRY_SETTLE_SECONDS {
                                        geometry.set_active(actual);
                                        ui.data_mut(|d| d.remove::<(u16, u16, f64)>(geometry_id));
                                    }
                                }
                                _ => {
                                    ui.data_mut(|d| {
                                        d.insert_temp(geometry_id, (actual[0], actual[1], now))
                                    });
                                }
                            }
                        } else {
                            ui.data_mut(|d| d.remove::<(u16, u16, f64)>(geometry_id));
                        }
                    }
                }
            }
            egui::CentralPanel::default()
                .frame(egui::Frame::new().fill(Palette::of(theme).card))
                .show(ui, |ui| {
                    actions = draw(ui, &model.cards, theme, lang, geometry.expanded_state, mode);
                });
        },
    );
    ctx.data_mut(|d| d.insert_temp(initialised_id, true));
    if actions
        .iter()
        .any(|action| matches!(action, Action::HidePlayer))
    {
        if let Some(actual) =
            ctx.data(|d| d.get_temp::<[u16; 2]>(egui::Id::new("audio-player-last-size")))
        {
            geometry.set_active(actual);
            ctx.data_mut(|d| {
                d.remove::<(u16, u16, f64)>(egui::Id::new("audio-player-geometry-pending"))
            });
        }
    }
    if let Some(index) = actions
        .iter()
        .position(|action| matches!(action, Action::ToggleExpanded))
    {
        actions.remove(index);
        geometry.expanded_state = !geometry.expanded_state;
        let active = geometry.active();
        let target = Vec2::new(active[0] as f32, active[1] as f32);
        ctx.data_mut(|d| d.insert_temp(pending_id, target));
        ctx.send_viewport_cmd_to(
            egui::ViewportId::from_hash_of("audio-player"),
            egui::ViewportCommand::InnerSize(target),
        );
    }
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
