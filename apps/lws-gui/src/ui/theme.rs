//! The design tokens of `mdf.css`, for both themes, and the embedded fonts.
//!
//! Every colour, radius and size here is a value of the mockup's `.mdf` / `.mdf[data-theme=dark]`
//! blocks; nothing is derived from egui's own palette, so the window cannot drift from the
//! design by an egui upgrade.

use std::sync::Arc;

use eframe::egui::{
    self, Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke,
};

use crate::gui_settings::Theme;

/// `rgba(r, g, b, a)` of CSS.
pub fn rgba(r: u8, g: u8, b: u8, a: f32) -> Color32 {
    Color32::from_rgba_unmultiplied(r, g, b, (a * 255.0).round() as u8)
}

fn rgb(hex: u32) -> Color32 {
    Color32::from_rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

/// `src` of alpha `a` painted over the opaque `under`: what a translucent card looks like on a
/// plain background, as one opaque colour.
fn over(under: Color32, r: u8, g: u8, b: u8, a: f32) -> Color32 {
    let mix = |u: u8, s: u8| (f32::from(s) * a + f32::from(u) * (1.0 - a)).round() as u8;
    Color32::from_rgb(
        mix(under.r(), r),
        mix(under.g(), g),
        mix(under.b(), b),
    )
}

/// One theme's tokens.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Palette {
    /// Which theme this is.
    pub theme: Theme,
    /// `--bg`.
    pub bg: Color32,
    /// `--bg-2`.
    pub bg2: Color32,
    /// `--card` over a plain background (top bar, dock, rail, dialog).
    pub card: Color32,
    /// `--card` as a translucent colour (cards that float over the field).
    pub card_glass: Color32,
    /// `--ink`.
    pub ink: Color32,
    /// `--ink-2`.
    pub ink2: Color32,
    /// `--hair`.
    pub hair: Color32,
    /// `--line`.
    pub line: Color32,
    /// `--scrim`.
    pub scrim: Color32,
    /// `--accent`.
    pub accent: Color32,
    /// `--accent-soft`.
    pub accent_soft: Color32,
    /// `--glow`.
    pub glow: Color32,
    /// `--on-accent`.
    pub on_accent: Color32,
    /// The green of `.dot.ok`.
    pub ok: Color32,
    /// `--r`.
    pub r: u8,
    /// `--r-sm`.
    pub r_sm: u8,
    /// `--r-btn`; 99 is the pill.
    pub r_btn: u8,
}

impl Palette {
    /// The tokens of `theme`.
    pub fn of(theme: Theme) -> Self {
        match theme {
            Theme::Light => {
                let bg = rgb(0xFBFCFE);
                Palette {
                    theme,
                    bg,
                    bg2: rgb(0xF1F4F9),
                    card: over(bg, 255, 255, 255, 0.86),
                    card_glass: rgba(255, 255, 255, 0.86),
                    ink: rgb(0x111827),
                    ink2: rgb(0x5B6475),
                    hair: rgba(15, 23, 42, 0.09),
                    line: rgba(15, 23, 42, 0.2),
                    scrim: rgba(241, 244, 249, 0.66),
                    accent: rgb(0x2563EB),
                    accent_soft: rgba(37, 99, 235, 0.10),
                    glow: rgba(37, 99, 235, 0.22),
                    on_accent: rgb(0xFFFFFF),
                    ok: rgb(0x10B981),
                    r: 16,
                    r_sm: 10,
                    r_btn: 99,
                }
            }
            Theme::Dark => {
                let bg = rgb(0x0B0B0A);
                Palette {
                    theme,
                    bg,
                    bg2: rgb(0x18181A),
                    card: over(bg, 21, 21, 23, 0.84),
                    card_glass: rgba(21, 21, 23, 0.84),
                    ink: rgb(0xEEECE4),
                    ink2: rgb(0x9C9A92),
                    hair: rgba(238, 236, 228, 0.10),
                    line: rgba(238, 236, 228, 0.28),
                    scrim: rgba(8, 8, 8, 0.62),
                    accent: rgb(0xFF5A2E),
                    accent_soft: rgba(255, 90, 46, 0.13),
                    glow: rgba(255, 90, 46, 0.28),
                    on_accent: rgb(0x0B0B0A),
                    ok: rgb(0x10B981),
                    r: 10,
                    r_sm: 8,
                    r_btn: 8,
                }
            }
        }
    }

    /// `--r` as a corner radius.
    pub fn rad(&self) -> CornerRadius {
        CornerRadius::same(self.r)
    }

    /// `--r-sm` as a corner radius.
    pub fn rad_sm(&self) -> CornerRadius {
        CornerRadius::same(self.r_sm)
    }

    /// The corner radius of a control `height` tall: `--r-btn`, which is the full pill in the
    /// light theme.
    pub fn rad_btn(&self, height: f32) -> CornerRadius {
        CornerRadius::same(self.r_btn.min((height / 2.0) as u8))
    }

    /// A 1 px stroke of `--hair`.
    pub fn hairline(&self) -> Stroke {
        Stroke::new(1.0, self.hair)
    }
}

/// The font families of the design.
pub mod fam {
    use super::FontFamily;

    /// Geist 400.
    pub fn sans() -> FontFamily {
        FontFamily::Proportional
    }
    /// Geist 500.
    pub fn sans_medium() -> FontFamily {
        FontFamily::Name("geist-medium".into())
    }
    /// Geist 600.
    pub fn sans_semibold() -> FontFamily {
        FontFamily::Name("geist-semibold".into())
    }
    /// Geist Mono 400.
    pub fn mono() -> FontFamily {
        FontFamily::Monospace
    }
    /// Geist Mono 500.
    pub fn mono_medium() -> FontFamily {
        FontFamily::Name("geist-mono-medium".into())
    }
    /// Silkscreen (Latin only; the fallback is Geist Mono).
    pub fn pixel() -> FontFamily {
        FontFamily::Name("silkscreen".into())
    }
}

/// `FontId` shorthands.
pub fn font(size: f32, family: FontFamily) -> FontId {
    FontId::new(size, family)
}

/// Installs the six embedded faces and nothing else.
///
/// Only the weights the mockup uses are embedded (`assets/fonts`, subset to Latin, Cyrillic and
/// the arrows the UI uses), so the binary carries about 135 KB of fonts.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = FontDefinitions::empty();
    let faces: [(&str, &'static [u8]); 6] = [
        (
            "geist",
            include_bytes!("../../assets/fonts/Geist-Regular.ttf"),
        ),
        (
            "geist-medium",
            include_bytes!("../../assets/fonts/Geist-Medium.ttf"),
        ),
        (
            "geist-semibold",
            include_bytes!("../../assets/fonts/Geist-SemiBold.ttf"),
        ),
        (
            "geist-mono",
            include_bytes!("../../assets/fonts/GeistMono-Regular.ttf"),
        ),
        (
            "geist-mono-medium",
            include_bytes!("../../assets/fonts/GeistMono-Medium.ttf"),
        ),
        (
            "silkscreen",
            include_bytes!("../../assets/fonts/Silkscreen-Regular.ttf"),
        ),
    ];
    for (name, bytes) in faces {
        fonts
            .font_data
            .insert(name.to_owned(), Arc::new(FontData::from_static(bytes)));
    }
    let chain = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect::<Vec<_>>();
    fonts
        .families
        .insert(fam::sans(), chain(&["geist", "geist-mono"]));
    fonts
        .families
        .insert(fam::sans_medium(), chain(&["geist-medium", "geist"]));
    fonts
        .families
        .insert(fam::sans_semibold(), chain(&["geist-semibold", "geist"]));
    fonts
        .families
        .insert(fam::mono(), chain(&["geist-mono", "geist"]));
    fonts
        .families
        .insert(fam::mono_medium(), chain(&["geist-mono-medium", "geist-mono"]));
    fonts
        .families
        .insert(fam::pixel(), chain(&["silkscreen", "geist-mono", "geist"]));
    ctx.set_fonts(fonts);
}

/// Paints egui's own widgets (text fields, drop-downs, scroll bars) with the theme's tokens.
pub fn apply(ctx: &egui::Context, pal: &Palette) {
    let mut visuals = match pal.theme {
        Theme::Light => egui::Visuals::light(),
        Theme::Dark => egui::Visuals::dark(),
    };
    visuals.override_text_color = Some(pal.ink);
    visuals.panel_fill = pal.bg;
    visuals.window_fill = pal.bg;
    visuals.extreme_bg_color = pal.bg;
    visuals.faint_bg_color = pal.bg2;
    visuals.window_stroke = Stroke::new(1.0, pal.line);
    visuals.selection.bg_fill = pal.accent_soft;
    visuals.selection.stroke = Stroke::new(1.0, pal.accent);
    visuals.hyperlink_color = pal.accent;
    visuals.window_corner_radius = pal.rad();
    visuals.menu_corner_radius = pal.rad_sm();
    visuals.striped = false;
    let widgets = &mut visuals.widgets;
    for w in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        w.corner_radius = pal.rad_sm();
        w.fg_stroke = Stroke::new(1.0, pal.ink);
    }
    widgets.noninteractive.bg_stroke = pal.hairline();
    widgets.inactive.bg_fill = pal.bg;
    widgets.inactive.weak_bg_fill = pal.bg;
    widgets.inactive.bg_stroke = Stroke::new(1.0, pal.line);
    widgets.hovered.bg_fill = pal.bg2;
    widgets.hovered.weak_bg_fill = pal.bg2;
    widgets.hovered.bg_stroke = Stroke::new(1.0, pal.accent);
    widgets.active.bg_fill = pal.bg2;
    widgets.active.weak_bg_fill = pal.bg2;
    widgets.active.bg_stroke = Stroke::new(1.0, pal.accent);
    widgets.open.bg_fill = pal.bg;
    widgets.open.weak_bg_fill = pal.bg;
    widgets.open.bg_stroke = Stroke::new(1.0, pal.accent);
    ctx.set_visuals(visuals);
    ctx.all_styles_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(12.0, 0.0);
        style.spacing.interact_size = egui::vec2(40.0, 44.0);
        style.spacing.scroll.floating = true;
        style.spacing.scroll.bar_width = 6.0;
        style.animation_time = 0.12;
        style.text_styles.insert(
            egui::TextStyle::Body,
            font(14.0, fam::sans()),
        );
        style.text_styles.insert(
            egui::TextStyle::Button,
            font(13.0, fam::sans_medium()),
        );
        style.text_styles.insert(
            egui::TextStyle::Monospace,
            font(13.0, fam::mono()),
        );
        style.text_styles.insert(
            egui::TextStyle::Small,
            font(11.0, fam::mono()),
        );
        style.text_styles.insert(
            egui::TextStyle::Heading,
            font(24.0, fam::sans_semibold()),
        );
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tokens_are_the_ones_of_mdf_css() {
        let light = Palette::of(Theme::Light);
        assert_eq!(light.bg, Color32::from_rgb(0xFB, 0xFC, 0xFE));
        assert_eq!(light.accent, Color32::from_rgb(0x25, 0x63, 0xEB));
        assert_eq!((light.r, light.r_sm, light.r_btn), (16, 10, 99));
        let dark = Palette::of(Theme::Dark);
        assert_eq!(dark.bg, Color32::from_rgb(0x0B, 0x0B, 0x0A));
        assert_eq!(dark.accent, Color32::from_rgb(0xFF, 0x5A, 0x2E));
        assert_eq!(dark.ink, Color32::from_rgb(0xEE, 0xEC, 0xE4));
        assert_eq!((dark.r, dark.r_sm, dark.r_btn), (10, 8, 8));
    }

    #[test]
    fn a_pill_is_half_its_height_and_a_dark_button_is_square_ish() {
        assert_eq!(Palette::of(Theme::Light).rad_btn(44.0), CornerRadius::same(22));
        assert_eq!(Palette::of(Theme::Dark).rad_btn(44.0), CornerRadius::same(8));
    }
}
