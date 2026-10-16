//! The shared design tokens every surface draws from (#362): colours for
//! light, dark and high contrast, type, radius and spacing.
//!
//! Pure on purpose: nothing here talks to the registry or a window, so the
//! palettes and their contrast can be unit-tested. Colours are GDI
//! `COLORREF` values (`0x00BBGGRR`), the same layout `rgb` builds.
//!
//! The card paints from these tokens in light, dark and high contrast. The
//! palette and region overlay move onto them in later slices.

use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT,
    COLOR_WINDOW, COLOR_WINDOWTEXT,
};

/// Packs 8-bit channels into a GDI `COLORREF` (`0x00BBGGRR`).
pub(crate) const fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

// ---------------------------------------------------------------------------
// Which theme
// ---------------------------------------------------------------------------

/// The look a surface is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Theme {
    Light,
    Dark,
    HighContrast,
}

impl Theme {
    /// High contrast wins over the light/dark app mode, the way Windows
    /// itself treats it.
    pub(crate) fn resolve(apps_use_light_theme: bool, high_contrast: bool) -> Theme {
        if high_contrast {
            Theme::HighContrast
        } else if apps_use_light_theme {
            Theme::Light
        } else {
            Theme::Dark
        }
    }
}

// ---------------------------------------------------------------------------
// Colour tokens
// ---------------------------------------------------------------------------

/// The colours of one theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tokens {
    /// The surface everything is drawn on.
    pub(crate) surface: u32,
    /// The hairline around a surface. Decorative, so it is not held to a
    /// text contrast ratio.
    pub(crate) border: u32,
    /// Primary text.
    pub(crate) text: u32,
    /// Secondary text, one step quieter.
    pub(crate) text_dim: u32,
    /// The quietest text: hints and footers.
    pub(crate) text_faint: u32,
    /// Clickable text (the card's footer links). Not greyed in high
    /// contrast, where grey text means disabled.
    pub(crate) link: u32,
    /// Error text.
    pub(crate) danger: u32,
}

/// Neither pure black nor pure white, per the card spec.
pub(crate) const LIGHT: Tokens = Tokens {
    surface: rgb(250, 250, 250),
    border: rgb(214, 214, 214),
    text: rgb(23, 23, 23),
    text_dim: rgb(90, 90, 90),
    text_faint: rgb(112, 112, 112),
    link: rgb(112, 112, 112),
    danger: rgb(200, 50, 50),
};

pub(crate) const DARK: Tokens = Tokens {
    surface: rgb(32, 32, 32),
    border: rgb(64, 64, 64),
    text: rgb(245, 245, 245),
    text_dim: rgb(190, 190, 190),
    text_faint: rgb(140, 140, 140),
    link: rgb(140, 140, 140),
    danger: rgb(240, 100, 100),
};

/// The Windows system colours that matter in high contrast, read again on
/// every theme change by [`SysColors::read`] and passed in so the mapping
/// stays pure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SysColors {
    pub(crate) window: u32,
    pub(crate) window_text: u32,
    pub(crate) gray_text: u32,
    pub(crate) hotlight: u32,
    pub(crate) highlight: u32,
    pub(crate) highlight_text: u32,
}

impl SysColors {
    /// The current system colours (`GetSysColor`).
    pub(crate) fn read() -> SysColors {
        // SAFETY: GetSysColor takes a constant index and has no pointers.
        unsafe {
            SysColors {
                window: GetSysColor(COLOR_WINDOW),
                window_text: GetSysColor(COLOR_WINDOWTEXT),
                gray_text: GetSysColor(COLOR_GRAYTEXT),
                hotlight: GetSysColor(COLOR_HOTLIGHT),
                highlight: GetSysColor(COLOR_HIGHLIGHT),
                highlight_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
            }
        }
    }
}

impl Tokens {
    /// The tokens for `theme`. Only high contrast looks at `sys`.
    pub(crate) fn for_theme(theme: Theme, sys: &SysColors) -> Tokens {
        match theme {
            Theme::Light => LIGHT,
            Theme::Dark => DARK,
            // Windows promises window text on window is readable, and
            // greyed text is its own pairing. Everything else is the same
            // ink: high contrast has no tiers of quiet.
            Theme::HighContrast => Tokens {
                surface: sys.window,
                border: sys.window_text,
                text: sys.window_text,
                text_dim: sys.window_text,
                text_faint: sys.gray_text,
                link: sys.hotlight,
                danger: sys.window_text,
            },
        }
    }
}

/// Windows' high-contrast black and high-contrast white themes. The tests
/// hold them to the contrast bar; the UI gallery's `--theme high-contrast`
/// draws with the black one, so a screenshot needs no change to the
/// machine's real settings.
#[cfg(any(test, debug_assertions))]
pub(crate) const HC_BLACK: SysColors = SysColors {
    window: 0x0000_0000,
    window_text: 0x00FF_FFFF,
    gray_text: rgb(0x3F, 0xF2, 0x3F),
    hotlight: rgb(0xFF, 0xFF, 0x00),
    highlight: 0x00FF_FF00,
    highlight_text: 0x0000_0000,
};
#[cfg(test)]
const HC_WHITE: SysColors = SysColors {
    window: 0x00FF_FFFF,
    window_text: 0x0000_0000,
    gray_text: rgb(0, 0x60, 0),
    hotlight: rgb(0, 0, 0x80),
    highlight: rgb(0x37, 0x00, 0x6E),
    highlight_text: 0x00FF_FFFF,
};

/// Difficulty badge fill: green, amber, red stops at ranks 1, 5 and 10.
/// Three stops (not a straight green to red blend) so the middle reads as
/// amber rather than a muddy brown.
pub(crate) const BADGE_LOW: (u8, u8, u8) = (0x2e, 0xa0, 0x43);
pub(crate) const BADGE_MID: (u8, u8, u8) = (0xf2, 0xa9, 0x00);
pub(crate) const BADGE_HIGH: (u8, u8, u8) = (0xd6, 0x2c, 0x2c);
/// Deliberately outside the 1-10 gradient: Ultra is its own thing.
pub(crate) const BADGE_ULTRA: (u8, u8, u8) = (0x8e, 0x24, 0xaa);
/// Badge label ink on a light fill, and on a dark fill. Not pure black and
/// white, so contrast checks match what is painted.
pub(crate) const BADGE_INK_DARK: u32 = rgb(0x20, 0x20, 0x20);
pub(crate) const BADGE_INK_LIGHT: u32 = rgb(0xff, 0xff, 0xff);

// ---------------------------------------------------------------------------
// Contrast
// ---------------------------------------------------------------------------

/// One sRGB channel (0..=255), gamma-expanded to linear light per the WCAG 2
/// relative luminance formula (https://www.w3.org/TR/WCAG21/#dfn-relative-luminance).
fn srgb_channel_to_linear(channel: u8) -> f64 {
    let c = channel as f64 / 255.0;
    if c <= 0.03928 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

/// WCAG 2 relative luminance of a packed `0x00BBGGRR` colour.
pub(crate) fn relative_luminance(color: u32) -> f64 {
    let r = (color & 0xFF) as u8;
    let g = ((color >> 8) & 0xFF) as u8;
    let b = ((color >> 16) & 0xFF) as u8;
    0.2126 * srgb_channel_to_linear(r)
        + 0.7152 * srgb_channel_to_linear(g)
        + 0.0722 * srgb_channel_to_linear(b)
}

/// WCAG 2 contrast ratio between two relative luminances
/// (https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio): the lighter over the
/// darker, each padded by 0.05 so black on black still gives a defined 1:1.
pub(crate) fn contrast_ratio(l1: f64, l2: f64) -> f64 {
    let (lighter, darker) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
    (lighter + 0.05) / (darker + 0.05)
}

/// The contrast ratio between two packed colours.
#[cfg(test)]
fn contrast_between(a: u32, b: u32) -> f64 {
    contrast_ratio(relative_luminance(a), relative_luminance(b))
}

// ---------------------------------------------------------------------------
// Type, radius and spacing
// ---------------------------------------------------------------------------

/// Headline size relative to the system message font. Deliberately below
/// 1.0: the headline should read smaller than a stock notification.
pub(crate) const HEADLINE_FONT_SCALE: f32 = 0.85;
/// Just under the headline, so the two stay distinguishable by size as a
/// second cue after colour. Neither is bold.
pub(crate) const DETAIL_FONT_SCALE: f32 = 0.8;
/// The smallest text: at-a-glance annotations such as the badge label.
pub(crate) const SMALL_FONT_SCALE: f32 = 0.7;
/// The face used when the system message font cannot be read.
pub(crate) const FALLBACK_FONT_FACE: &str = "Segoe UI";

/// Outer margin around a surface, in dp (logical pixels at 96 DPI).
pub(crate) const MARGIN_DP: i32 = 14;
/// Inner padding of a surface, in dp.
pub(crate) const PADDING_DP: i32 = 14;
/// The gap between stacked elements, in dp.
pub(crate) const GAP_DP: i32 = 6;
/// Corner radius of owner-drawn controls, in dp.
pub(crate) const CONTROL_RADIUS_DP: i32 = 6;

#[cfg(test)]
mod tests {
    use super::*;

    /// Ratios from WCAG 2: body text needs 4.5, large text and UI parts 3.
    const AA_TEXT: f64 = 4.5;
    const AA_LARGE: f64 = 3.0;

    #[test]
    fn light_and_dark_values_are_the_ones_the_card_always_painted() {
        // Pinned: changing any of these is a visible change. The hint and
        // error colours were darkened or lightened for 4.5:1 in #578.
        assert_eq!(LIGHT.surface, rgb(250, 250, 250));
        assert_eq!(LIGHT.border, rgb(214, 214, 214));
        assert_eq!(LIGHT.text, rgb(23, 23, 23));
        assert_eq!(LIGHT.text_dim, rgb(90, 90, 90));
        assert_eq!(LIGHT.text_faint, rgb(112, 112, 112));
        assert_eq!(LIGHT.danger, rgb(200, 50, 50));
        assert_eq!(LIGHT.link, LIGHT.text_faint);
        assert_eq!(DARK.link, DARK.text_faint);
        assert_eq!(DARK.surface, rgb(32, 32, 32));
        assert_eq!(DARK.border, rgb(64, 64, 64));
        assert_eq!(DARK.text, rgb(245, 245, 245));
        assert_eq!(DARK.text_dim, rgb(190, 190, 190));
        assert_eq!(DARK.text_faint, rgb(140, 140, 140));
        assert_eq!(DARK.danger, rgb(240, 100, 100));
    }

    #[test]
    fn rgb_packs_as_bbggrr() {
        assert_eq!(rgb(0x11, 0x22, 0x33), 0x0033_2211);
    }

    #[test]
    fn contrast_helpers_match_the_wcag_extremes() {
        assert!((contrast_between(0, 0x00FF_FFFF) - 21.0).abs() < 1e-9);
        assert!((contrast_between(0x0012_3456, 0x0012_3456) - 1.0).abs() < 1e-9);
    }

    /// Every (foreground, background) text pair a theme defines.
    fn text_pairs(t: &Tokens) -> [(&'static str, u32, u32); 5] {
        [
            ("text", t.text, t.surface),
            ("text_dim", t.text_dim, t.surface),
            ("text_faint", t.text_faint, t.surface),
            ("link", t.link, t.surface),
            ("danger", t.danger, t.surface),
        ]
    }

    #[test]
    fn every_text_pair_in_every_theme_meets_aa() {
        let themes = [
            (Theme::Light, LIGHT),
            (Theme::Dark, DARK),
            (
                Theme::HighContrast,
                Tokens::for_theme(Theme::HighContrast, &HC_WHITE),
            ),
            (
                Theme::HighContrast,
                Tokens::for_theme(Theme::HighContrast, &HC_BLACK),
            ),
        ];
        let mut checked = 0;
        for (theme, tokens) in themes {
            for (name, fg, bg) in text_pairs(&tokens) {
                let ratio = contrast_between(fg, bg);
                assert!(
                    ratio >= AA_TEXT,
                    "{theme:?} {name}: {ratio:.2}:1 is under {AA_TEXT}:1"
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 20, "the test must look at every pair");
    }

    #[test]
    fn badge_ink_is_readable_on_every_badge_fill() {
        // The two end stops and the amber middle are the extremes the
        // gradient blends between; Ultra is its own fill.
        for (r, g, b) in [BADGE_LOW, BADGE_MID, BADGE_HIGH, BADGE_ULTRA] {
            let fill = rgb(r, g, b);
            let best =
                contrast_between(fill, BADGE_INK_DARK).max(contrast_between(fill, BADGE_INK_LIGHT));
            assert!(best >= AA_LARGE, "{fill:#08x} reaches only {best:.2}:1");
        }
    }

    #[test]
    fn high_contrast_takes_its_colours_from_the_system() {
        let t = Tokens::for_theme(Theme::HighContrast, &HC_WHITE);
        assert_eq!(t.surface, HC_WHITE.window);
        assert_eq!(t.text, HC_WHITE.window_text);
        assert_eq!(t.text_dim, HC_WHITE.window_text);
        assert_eq!(t.text_faint, HC_WHITE.gray_text);
        // Links are not greyed text: Windows keeps GrayText for disabled
        // items and gives links their own colour.
        assert_eq!(t.link, HC_WHITE.hotlight);
        assert_ne!(t.link, t.text_faint);
        // Light and dark ignore the system colours entirely.
        assert_eq!(Tokens::for_theme(Theme::Light, &HC_WHITE), LIGHT);
        assert_eq!(Tokens::for_theme(Theme::Dark, &HC_BLACK), DARK);
    }

    #[test]
    fn high_contrast_wins_over_the_app_mode() {
        assert_eq!(Theme::resolve(true, false), Theme::Light);
        assert_eq!(Theme::resolve(false, false), Theme::Dark);
        assert_eq!(Theme::resolve(true, true), Theme::HighContrast);
        assert_eq!(Theme::resolve(false, true), Theme::HighContrast);
    }

    #[test]
    fn the_system_colour_read_returns_a_real_palette() {
        let sys = SysColors::read();
        // Whatever theme is active, text on the window must be readable.
        assert!(contrast_between(sys.window_text, sys.window) >= AA_TEXT);
    }
}
