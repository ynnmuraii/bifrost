//! The design tokens of `mdf.css`, for both themes, and the embedded fonts.
//!
//! Every colour here is a value of the mockup's `.mdf` / `.mdf[data-theme=dark]` blocks, and
//! every radius and size a value of `.mdf` alone: the two themes are one design in two sets of
//! colours. Nothing is derived from egui's own palette, so the window cannot drift from the
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
    Color32::from_rgb(mix(under.r(), r), mix(under.g(), g), mix(under.b(), b))
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
    /// `--shadow`: the drop shadow of a card that is standing still.
    pub shadow: Color32,
    /// `--shadow`: the deeper one a card takes while it is being dragged.
    pub shadow_drag: Color32,
    /// `--shadow-lg`: the shadow under a dialog, which sits above the page as well as the map.
    pub shadow_dialog: Color32,
}

/// `--r`: the radius of a card or a dialog. One style for both themes — the themes change
/// colours and nothing else.
pub const R: u8 = 16;
/// `--r-sm`: the radius of a small control.
pub const R_SM: u8 = 10;
/// `--r-btn`: the radius of a button, which is the full pill.
pub const R_BTN: u8 = 99;

impl Palette {
    /// The tokens of `theme`.
    pub fn of(theme: Theme) -> Self {
        match theme {
            // The Swiss light theme: paper, near-black ink, hairlines, and one accent — the
            // dark theme's orange. No colour of it is blue-dominant; the tests below hold it
            // to that, and to the contrast of its ink on the paper and the surface.
            Theme::Light => {
                let bg = rgb(0xFAFAF8);
                Palette {
                    theme,
                    bg,
                    bg2: rgb(0xF1F0EB),
                    card: over(bg, 255, 255, 255, 0.86),
                    card_glass: rgba(255, 255, 255, 0.86),
                    ink: rgb(0x111111),
                    ink2: rgb(0x5B5A55),
                    hair: rgba(17, 17, 14, 0.11),
                    line: rgba(17, 17, 14, 0.26),
                    scrim: rgba(250, 250, 248, 0.66),
                    accent: rgb(0xFF5A2E),
                    accent_soft: rgba(255, 90, 46, 0.12),
                    glow: rgba(255, 90, 46, 0.2),
                    on_accent: rgb(0x111111),
                    ok: rgb(0x0E7A56),
                    shadow: rgba(17, 17, 17, 0.07),
                    shadow_drag: rgba(17, 17, 17, 0.16),
                    shadow_dialog: rgba(17, 17, 17, 0.16),
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
                    // The dark background swallows a shadow, so the dark theme casts the same
                    // black four times as dark as the light theme casts its near-black.
                    shadow: rgba(0, 0, 0, 0.28),
                    shadow_drag: rgba(0, 0, 0, 0.64),
                    shadow_dialog: rgba(0, 0, 0, 0.65),
                }
            }
        }
    }

    /// `--r` as a corner radius.
    pub fn rad(&self) -> CornerRadius {
        CornerRadius::same(R)
    }

    /// `--r-sm` as a corner radius.
    pub fn rad_sm(&self) -> CornerRadius {
        CornerRadius::same(R_SM)
    }

    /// The corner radius of a control `height` tall: `--r-btn`, which is the full pill.
    pub fn rad_btn(&self, height: f32) -> CornerRadius {
        CornerRadius::same(R_BTN.min((height / 2.0) as u8))
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

/// Identity of the font definitions backing cached galley UVs. egui may rebuild its atlas
/// without changing pixel scale; that rebuild clones these family vectors.
pub(super) fn font_epoch(painter: &egui::Painter) -> usize {
    painter.ctx().fonts_mut(|fonts| {
        fonts
            .definitions()
            .families
            .get(&egui::FontFamily::Monospace)
            .map_or(0, |family| family.as_ptr() as usize)
    })
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
    fonts.families.insert(
        fam::mono_medium(),
        chain(&["geist-mono-medium", "geist-mono"]),
    );
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
    if pal.theme == Theme::Light {
        // egui's own light caret is a dark blue — the one blue the widget palette would leave
        // in a Swiss window. The dark theme's own caret is left as the design had it.
        visuals.text_cursor.stroke = Stroke::new(2.0, pal.ink);
    }
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
        style
            .text_styles
            .insert(egui::TextStyle::Body, font(14.0, fam::sans()));
        style
            .text_styles
            .insert(egui::TextStyle::Button, font(13.0, fam::sans_medium()));
        style
            .text_styles
            .insert(egui::TextStyle::Monospace, font(13.0, fam::mono()));
        style
            .text_styles
            .insert(egui::TextStyle::Small, font(11.0, fam::mono()));
        style
            .text_styles
            .insert(egui::TextStyle::Heading, font(24.0, fam::sans_semibold()));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    impl Palette {
        /// Every colour of the theme. The rules of a palette are checked over the whole list,
        /// so a colour added later is checked by them without anyone remembering to.
        fn colors(&self) -> Vec<Color32> {
            vec![
                self.bg,
                self.bg2,
                self.card,
                self.card_glass,
                self.ink,
                self.ink2,
                self.hair,
                self.line,
                self.scrim,
                self.accent,
                self.accent_soft,
                self.glow,
                self.on_accent,
                self.ok,
                self.shadow,
                self.shadow_drag,
                self.shadow_dialog,
            ]
        }
    }

    /// The relative luminance of `c` as WCAG 2 defines it: sRGB channels linearised, then
    /// weighted.
    fn luminance(c: Color32) -> f32 {
        let channel = |v: u8| {
            let s = f32::from(v) / 255.0;
            if s <= 0.039_28 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(c.r()) + 0.7152 * channel(c.g()) + 0.0722 * channel(c.b())
    }

    /// The WCAG 2 contrast ratio of two opaque colours, from 1.0 to 21.0.
    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
        (hi + 0.05) / (lo + 0.05)
    }

    #[test]
    fn the_tokens_are_the_ones_of_mdf_css() {
        let light = Palette::of(Theme::Light);
        assert_eq!(light.bg, Color32::from_rgb(0xFA, 0xFA, 0xF8));
        assert_eq!(light.bg2, Color32::from_rgb(0xF1, 0xF0, 0xEB));
        assert_eq!(light.ink, Color32::from_rgb(0x11, 0x11, 0x11));
        assert_eq!(light.ink2, Color32::from_rgb(0x5B, 0x5A, 0x55));
        let dark = Palette::of(Theme::Dark);
        assert_eq!(dark.bg, Color32::from_rgb(0x0B, 0x0B, 0x0A));
        assert_eq!(dark.accent, Color32::from_rgb(0xFF, 0x5A, 0x2E));
        assert_eq!(dark.ink, Color32::from_rgb(0xEE, 0xEC, 0xE4));
    }

    #[test]
    fn the_light_ink_is_legible_on_the_paper_and_the_surface() {
        let light = Palette::of(Theme::Light);
        // Body text and the dim ink, on the page and on a card laid over it: WCAG AA.
        for (name, ink) in [("ink", light.ink), ("ink2", light.ink2)] {
            for (under, surface) in [("bg", light.bg), ("card", light.card)] {
                let ratio = contrast(ink, surface);
                assert!(
                    ratio >= 4.5,
                    "the light theme's {name} on {under} is {ratio:.2}:1, under WCAG AA's 4.5:1"
                );
            }
        }
    }

    #[test]
    fn no_colour_of_the_light_theme_is_blue() {
        let light = Palette::of(Theme::Light);
        for c in light.colors() {
            let (r, g, b) = (i32::from(c.r()), i32::from(c.g()), i32::from(c.b()));
            assert!(
                !(b > r + 20 && b > g + 20),
                "{} of the light theme is blue-dominant",
                c.to_hex()
            );
        }
    }

    #[test]
    fn the_two_themes_share_one_accent() {
        let (light, dark) = (Palette::of(Theme::Light), Palette::of(Theme::Dark));
        assert_eq!(light.accent, dark.accent);
        // The accent is legible as the ink of a filled button in either theme: the light
        // theme puts the ink on it, the dark theme its near-black.
        assert!(contrast(light.on_accent, light.accent) >= 4.5);
        assert!(contrast(dark.on_accent, dark.accent) >= 4.5);
    }

    #[test]
    fn both_themes_share_one_rounded_style() {
        let (light, dark) = (Palette::of(Theme::Light), Palette::of(Theme::Dark));
        assert_eq!(light.rad(), dark.rad());
        assert_eq!(light.rad_sm(), dark.rad_sm());
        // The pill: a 44 px control is a half-round, in either theme.
        for pal in [light, dark] {
            assert_eq!(pal.rad(), CornerRadius::same(R));
            assert_eq!(pal.rad_sm(), CornerRadius::same(R_SM));
            assert_eq!(pal.rad_btn(44.0), CornerRadius::same(22));
            assert_eq!(pal.rad_btn(38.0), CornerRadius::same(19));
        }
    }
}
