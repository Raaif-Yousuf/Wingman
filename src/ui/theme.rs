//! The shared design tokens every surface draws from (#362): colours for
//! light, dark and high contrast, type, radius and spacing.
//!
//! The palettes and their contrast are pure, so they can be unit-tested. The
//! only live reads are `detect_theme` (the app-mode registry value and the
//! high contrast flag) and `SysColors::read`, and the `WM_SETTINGCHANGE`
//! helpers every surface uses to know when to call them again. Colours are GDI
//! `COLORREF` values (`0x00BBGGRR`), the same layout `rgb` builds.
//!
//! The card, the region overlay and the palette paint from these tokens in
//! light, dark and high contrast. Settings moves onto them in a later slice.

use std::ffi::c_void;

use windows::Win32::Foundation::LPARAM;
use windows::Win32::Graphics::Gdi::{
    GetSysColor, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT, COLOR_HOTLIGHT,
    COLOR_WINDOW, COLOR_WINDOWTEXT,
};
use windows::Win32::UI::WindowsAndMessaging::SPI_SETHIGHCONTRAST;

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

/// Reads `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`.
///
/// `windows` 0.62 is built in this project *without* the `Win32_System_Registry`
/// feature (see `Cargo.toml`, owned by another agent), so `RegGetValueW` is not
/// available through the `windows` crate here. Rather than touch `Cargo.toml`,
/// this module declares the one registry function it needs itself and links
/// directly against `advapi32.dll` (a standard system import library, not a
/// new crate dependency).
pub(crate) fn detect_theme() -> Theme {
    Theme::resolve(
        registry_apps_use_light_theme(),
        crate::ui::card_buttons::system_high_contrast().is_some(),
    )
}

/// Whether a `WM_SETTINGCHANGE` names the light/dark switch. Windows sends
/// `"ImmersiveColorSet"` in `lparam` when the user changes app mode or the
/// accent color.
fn setting_change_is_theme(name: &str) -> bool {
    name == "ImmersiveColorSet"
}

/// Whether a `WM_SETTINGCHANGE` is one the card's theme depends on: the
/// light/dark switch, or the high contrast flag (`wparam` is
/// `SPI_SETHIGHCONTRAST`; its `lparam` carries no name).
pub(crate) fn setting_change_affects_theme(wparam: usize, name: &str) -> bool {
    wparam == SPI_SETHIGHCONTRAST.0 as usize || setting_change_is_theme(name)
}

/// The text of a `WM_SETTINGCHANGE` `lparam` (a null-terminated UTF-16
/// string, or null for the broadcasts that carry none).
///
/// # Safety
/// `lparam` must be zero or point at a null-terminated UTF-16 string, which
/// is what Windows guarantees for this message.
pub(crate) unsafe fn setting_change_name(lparam: LPARAM) -> String {
    let ptr = lparam.0 as *const u16;
    if ptr.is_null() {
        return String::new();
    }
    let mut len = 0;
    while *ptr.add(len) != 0 {
        len += 1;
    }
    String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
}

const RRF_RT_REG_DWORD: u32 = 0x0000_0010;
const RRF_RT_REG_BINARY: u32 = 0x0000_0008;

fn hkey_current_user() -> *mut c_void {
    // HKEY_CURRENT_USER is defined by the Windows headers as
    // ((HKEY)(ULONG_PTR)((LONG)0x80000001)) — i.e. sign-extended.
    (0x8000_0001u32 as i32 as isize) as *mut c_void
}

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        hkey: *mut c_void,
        lpsubkey: *const u16,
        lpvalue: *const u16,
        dwflags: u32,
        pdwtype: *mut u32,
        pvdata: *mut c_void,
        pcbdata: *mut u32,
    ) -> i32;
}

fn registry_apps_use_light_theme() -> bool {
    let subkey = wide_z(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide_z("AppsUseLightTheme");
    let mut data: u32 = 1; // default to light on any failure
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            hkey_current_user(),
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    if status == 0 {
        data != 0
    } else {
        // #356: a dark desktop would get a light card with no explanation.
        crate::diagnostics::note_warning(&format!(
            "Card: could not read the app theme setting (error {status}), using the light theme"
        ));
        true
    }
}

/// UTF-16, null-terminated, for the registry call below.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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
    /// The fill of a text box, one step off the surface it sits on (Windows 11
    /// `ControlFillColorDefault`). High contrast has no tints: the window
    /// colour, with the border carrying the shape.
    pub(crate) control_fill: u32,
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
    control_fill: rgb(255, 255, 255),
};

pub(crate) const DARK: Tokens = Tokens {
    surface: rgb(32, 32, 32),
    border: rgb(64, 64, 64),
    text: rgb(245, 245, 245),
    text_dim: rgb(190, 190, 190),
    text_faint: rgb(140, 140, 140),
    link: rgb(140, 140, 140),
    danger: rgb(240, 100, 100),
    control_fill: rgb(45, 45, 45),
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
            // Windows promises window text on window is readable. GrayText
            // means "unavailable", so it is not used for live labels, the
            // chevron or the status line: everything is the same ink (high
            // contrast has no tiers of quiet). Disabled buttons take
            // `gray_text` from `SysColors` directly.
            Theme::HighContrast => Tokens {
                surface: sys.window,
                border: sys.window_text,
                text: sys.window_text,
                text_dim: sys.window_text,
                text_faint: sys.window_text,
                link: sys.hotlight,
                danger: sys.window_text,
                control_fill: sys.window,
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
pub(crate) const HC_WHITE: SysColors = SysColors {
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

// ---------------------------------------------------------------------------
// The system accent
// ---------------------------------------------------------------------------

/// The two shades of the user's accent colour that Windows 11 fills accent
/// buttons with: `AccentDark1` in light mode, `AccentLight2` in dark mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AccentShades {
    pub light2: u32,
    pub dark1: u32,
}

/// The default Windows blue's shades, used when the palette cannot be read.
pub(crate) const DEFAULT_ACCENT: AccentShades = AccentShades {
    light2: rgb(0x4C, 0xC2, 0xFF),
    dark1: rgb(0x00, 0x67, 0xC0),
};

/// Parses `HKCU\Software\Microsoft\Windows\CurrentVersion\Explorer\Accent\AccentPalette`:
/// eight RGBA entries, lightest first (Light3, Light2, Light1, Accent, Dark1,
/// Dark2, Dark3, and a neutral). MEASURED 2026-10-16 on the dev machine: entry
/// 3 was the accent blue 0078D4 while `DwmGetColorizationColor` returned
/// graphite 3A3A3A, which is why the card's button was grey. `None` if the
/// blob is too short.
pub(crate) fn parse_accent_palette(bytes: &[u8]) -> Option<AccentShades> {
    let entry = |i: usize| -> Option<u32> {
        let e = bytes.get(i * 4..i * 4 + 3)?;
        Some(rgb(e[0], e[1], e[2]))
    };
    Some(AccentShades {
        light2: entry(1)?,
        dark1: entry(4)?,
    })
}

/// `fill`, moved toward black (on a light surface) or white (on a dark one)
/// in 5% steps until it reaches `min` contrast against `surface`.
pub(crate) fn ensure_contrast(fill: u32, surface: u32, min: f64) -> u32 {
    use crate::ui::card_buttons::blend;
    let toward = if relative_luminance(surface) > 0.5 {
        0x0000_0000
    } else {
        0x00FF_FFFF
    };
    for pct in (0..=100).step_by(5) {
        let c = blend(fill, toward, pct);
        if contrast_ratio(relative_luminance(c), relative_luminance(surface)) >= min {
            return c;
        }
    }
    toward
}

/// The resting fill of an accent (primary) button in `theme` on `surface`:
/// the theme's shade of the user's accent, pushed apart from the surface if
/// the accent is so pale or so dark that the button would not stand out
/// (WCAG 1.4.11: 3:1 for the boundary of a control).
pub(crate) fn accent_fill(theme: Theme, shades: AccentShades, surface: u32) -> u32 {
    let shade = match theme {
        Theme::Dark => shades.light2,
        Theme::Light | Theme::HighContrast => shades.dark1,
    };
    ensure_contrast(shade, surface, 3.0)
}

/// The user's accent shades. Read from the registry rather than
/// `UISettings`: that is WinRT and needs an apartment on this thread, and
/// the UI thread owns the keyboard hook and must not change its COM mode.
/// Read per paint (one small registry read) so an accent change shows at once.
pub(crate) fn read_accent_shades() -> AccentShades {
    let subkey = wide_z(r"Software\Microsoft\Windows\CurrentVersion\Explorer\Accent");
    let value = wide_z("AccentPalette");
    // The real value is 32 bytes; the buffer is larger, and bounded, so a
    // longer value is parsed instead of failing with ERROR_MORE_DATA.
    let mut data = [0u8; 256];
    let mut size = data.len() as u32;
    let status = unsafe {
        RegGetValueW(
            hkey_current_user(),
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_BINARY,
            std::ptr::null_mut(),
            data.as_mut_ptr() as *mut c_void,
            &mut size,
        )
    };
    if status != 0 {
        return DEFAULT_ACCENT;
    }
    parse_accent_palette(&data[..(size as usize).min(data.len())]).unwrap_or(DEFAULT_ACCENT)
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

/// The Windows 11 type ramp, in px at 96 DPI (scaled per window by
/// `type_height_px`). Sentence case, no ALL CAPS.
///
/// Subtitle: titles on larger surfaces, semibold.
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const TYPE_SUBTITLE_PX: i32 = 20;
/// Body: field labels and values, button text. The card headline is this
/// size in semibold ("body strong").
pub(crate) const TYPE_BODY_PX: i32 = 14;
/// Caption: detail text, footer links, the status line and the badge.
pub(crate) const TYPE_CAPTION_PX: i32 = 12;
/// Line height for body text, in px at 96 DPI.
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const TYPE_BODY_LINE_PX: i32 = 20;
/// Line height for caption text, in px at 96 DPI.
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const TYPE_CAPTION_LINE_PX: i32 = 16;
/// The preferred face, installed on Windows 11. Surfaces fall back to the
/// system message font's face when it is missing.
pub(crate) const PREFERRED_FONT_FACE: &str = "Segoe UI Variable Text";
/// The face used when the system message font cannot be read.
pub(crate) const FALLBACK_FONT_FACE: &str = "Segoe UI";

/// A `LOGFONTW::lfHeight` for a ramp size: negative (character height, not
/// cell height), scaled for `dpi` and the user's `text_scale`, never zero.
pub(crate) fn type_height_px(ramp_px: i32, dpi: u32, text_scale: f32) -> i32 {
    let v = (ramp_px as f32 * dpi as f32 / 96.0 * text_scale).round() as i32;
    -v.max(1)
}

/// Spacing on the 4 px grid, in dp (logical pixels at 96 DPI).
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const SPACE_XS_DP: i32 = 4;
pub(crate) const SPACE_SM_DP: i32 = 8;
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const SPACE_MD_DP: i32 = 12;
pub(crate) const SPACE_LG_DP: i32 = 16;
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const SPACE_XL_DP: i32 = 24;

/// Outer margin around a surface, in dp.
pub(crate) const MARGIN_DP: i32 = SPACE_LG_DP;
/// Inner padding of a surface, in dp.
pub(crate) const PADDING_DP: i32 = SPACE_LG_DP;
/// The gap between stacked elements, in dp.
pub(crate) const GAP_DP: i32 = SPACE_SM_DP;
/// Corner radius of owner-drawn controls, in dp.
pub(crate) const CONTROL_RADIUS_DP: i32 = 4;
/// Thickness of the accent line under a focused text box, in dp (the
/// Windows 11 text box underline).
pub(crate) const FOCUS_LINE_DP: i32 = 2;
/// The icon font on Windows 11, and the one it replaced, which Windows 10
/// still has. Use [`icon_font_face`] to pick one.
pub(crate) const FLUENT_ICON_FONT_FACE: &str = "Segoe Fluent Icons";
pub(crate) const MDL2_ICON_FONT_FACE: &str = "Segoe MDL2 Assets";
/// The magnifier glyph, the same code point in both icon fonts.
pub(crate) const GLYPH_SEARCH: char = '\u{E721}';
/// Side of the square an icon glyph is drawn in, in dp.
pub(crate) const ICON_BOX_DP: i32 = 16;
/// Font size of an icon glyph in a text box or list row, in dp.
pub(crate) const ICON_FONT_DP: i32 = 14;

/// The icon font to draw glyphs in: Segoe Fluent Icons when it is installed,
/// otherwise Segoe MDL2 Assets (Windows 10).
pub(crate) fn icon_font_face(fluent_installed: bool) -> &'static str {
    if fluent_installed {
        FLUENT_ICON_FONT_FACE
    } else {
        MDL2_ICON_FONT_FACE
    }
}

/// Corner radius of a top-level window, in dp (what `DWMWCP_ROUND` draws).
#[allow(dead_code)] // ramp token for surfaces not yet moved onto it
pub(crate) const WINDOW_RADIUS_DP: i32 = 8;

// ---------------------------------------------------------------------------
// Menus (the tray menu, #825)
// ---------------------------------------------------------------------------

/// Height of one menu row, in dp. Windows 11's own flyout menus use 32.
pub(crate) const MENU_ITEM_HEIGHT_DP: i32 = 32;
/// Height of a separator row, in dp: a hairline with 4 dp above and below.
pub(crate) const MENU_SEPARATOR_HEIGHT_DP: i32 = 9;
/// Padding above the first row and below the last, and the inset of the
/// hover pill from the menu's sides, in dp.
pub(crate) const MENU_EDGE_PAD_DP: i32 = SPACE_XS_DP;
/// Gap between the hover pill and the row's top and bottom, in dp.
pub(crate) const MENU_PILL_INSET_Y_DP: i32 = 2;
/// Width of the left gutter (icon, radio dot) measured from the pill's left
/// edge to where the text starts, in dp.
pub(crate) const MENU_GUTTER_DP: i32 = 36;
/// Space reserved at the right of a row for the submenu chevron, in dp.
pub(crate) const MENU_CHEVRON_AREA_DP: i32 = 28;
/// Right padding of a row without a chevron, in dp.
pub(crate) const MENU_RIGHT_PAD_DP: i32 = SPACE_LG_DP;
/// Minimum gap between a label and its right-aligned hint, in dp.
pub(crate) const MENU_HINT_GAP_DP: i32 = SPACE_XL_DP;
/// Narrowest and widest a menu is allowed to be, in dp.
pub(crate) const MENU_MIN_WIDTH_DP: i32 = 200;
pub(crate) const MENU_MAX_WIDTH_DP: i32 = 420;
/// Glyph size of the icon column, in px at 96 DPI.
pub(crate) const MENU_ICON_PX: i32 = 16;
/// How far the hover pill is moved from the surface toward the text colour,
/// in percent. Windows 11 uses a 6% white (dark) or black (light) overlay.
const MENU_HOVER_PCT: u32 = 6;
/// How far disabled text is moved from the text colour toward the surface,
/// in percent.
const MENU_DISABLED_PCT: u32 = 55;

/// How far a keycap is moved from the row it sits on toward the text colour,
/// in percent: a quiet neutral chip, as Windows draws a shortcut.
const KEYCAP_FILL_PCT: u32 = 10;

/// The fill of a keycap on a row painted `row_fill`. High contrast has no
/// tints, so it is the row colour and the border carries the shape.
pub(crate) fn keycap_fill(theme: Theme, row_fill: u32, text: u32) -> u32 {
    match theme {
        Theme::HighContrast => row_fill,
        Theme::Light | Theme::Dark => {
            crate::ui::card_buttons::blend(row_fill, text, KEYCAP_FILL_PCT)
        }
    }
}

/// The fill of the hovered or keyboard-selected row. High contrast uses the
/// system highlight, as every native menu does.
pub(crate) fn menu_hover_fill(theme: Theme, tokens: &Tokens, sys: &SysColors) -> u32 {
    match theme {
        Theme::HighContrast => sys.highlight,
        Theme::Light | Theme::Dark => {
            crate::ui::card_buttons::blend(tokens.surface, tokens.text, MENU_HOVER_PCT)
        }
    }
}

/// The text colour of a row, by whether it is hovered and whether it can be
/// used. High contrast uses `HighlightText` on the highlight and `GrayText`
/// for a disabled row.
pub(crate) fn menu_text_color(
    theme: Theme,
    tokens: &Tokens,
    sys: &SysColors,
    hot: bool,
    disabled: bool,
) -> u32 {
    match theme {
        Theme::HighContrast => {
            if hot {
                sys.highlight_text
            } else if disabled {
                sys.gray_text
            } else {
                sys.window_text
            }
        }
        Theme::Light | Theme::Dark => {
            if disabled {
                crate::ui::card_buttons::blend(tokens.text, tokens.surface, MENU_DISABLED_PCT)
            } else {
                tokens.text
            }
        }
    }
}

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
        assert_eq!(LIGHT.control_fill, rgb(255, 255, 255));
        assert_eq!(DARK.control_fill, rgb(45, 45, 45));
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
    fn the_accent_palette_gives_light2_and_dark1() {
        // The dev machine's real AccentPalette: RGBA, Light3 first.
        let blob: [u8; 32] = [
            0x99, 0xEB, 0xFF, 0x00, 0x4C, 0xC2, 0xFF, 0x00, 0x00, 0x91, 0xF8, 0x00, 0x00, 0x78,
            0xD4, 0x00, 0x00, 0x67, 0xC0, 0x00, 0x00, 0x3E, 0x92, 0x00, 0x00, 0x1A, 0x68, 0x00,
            0x88, 0x17, 0x98, 0x00,
        ];
        assert_eq!(parse_accent_palette(&blob), Some(DEFAULT_ACCENT));
        assert_eq!(parse_accent_palette(&blob[..18]), None);
        assert_eq!(parse_accent_palette(&[]), None);
    }

    #[test]
    fn accent_buttons_stand_out_and_stay_readable_for_any_accent() {
        use crate::ui::card_buttons::readable_on;
        // Windows' own accent choices plus the extremes: graphite, black,
        // white and a pale yellow, in both themes.
        let accents = [
            rgb(0x00, 0x78, 0xD4),
            rgb(0xFF, 0xB9, 0x00),
            rgb(0xF7, 0x63, 0x0C),
            rgb(0xE7, 0x48, 0x56),
            rgb(0x00, 0xCC, 0x6A),
            rgb(0x3A, 0x3A, 0x3A),
            rgb(0x00, 0x00, 0x00),
            rgb(0xFF, 0xFF, 0xFF),
            rgb(0xFF, 0xF1, 0x00),
        ];
        let mut checked = 0;
        for (theme, surface) in [(Theme::Light, LIGHT.surface), (Theme::Dark, DARK.surface)] {
            for a in accents {
                let shades = AccentShades {
                    light2: a,
                    dark1: a,
                };
                let fill = accent_fill(theme, shades, surface);
                let vs_surface = contrast_between(fill, surface);
                assert!(vs_surface >= 3.0, "{theme:?} {a:#08x}: {vs_surface:.2}:1");
                let label = contrast_between(fill, readable_on(fill));
                assert!(label >= AA_TEXT, "{theme:?} {a:#08x}: label {label:.2}:1");
                checked += 1;
            }
        }
        assert_eq!(checked, 18, "the test must look at every pair");
    }

    #[test]
    fn the_default_blue_is_used_unchanged_when_it_already_stands_out() {
        assert_eq!(
            accent_fill(Theme::Light, DEFAULT_ACCENT, LIGHT.surface),
            DEFAULT_ACCENT.dark1
        );
        assert_eq!(
            accent_fill(Theme::Dark, DEFAULT_ACCENT, DARK.surface),
            DEFAULT_ACCENT.light2
        );
    }

    #[test]
    fn type_ramp_scales_with_dpi_and_text_scale() {
        assert_eq!(type_height_px(TYPE_BODY_PX, 96, 1.0), -14);
        assert_eq!(type_height_px(TYPE_CAPTION_PX, 96, 1.0), -12);
        assert_eq!(type_height_px(TYPE_SUBTITLE_PX, 96, 1.0), -20);
        assert_eq!(type_height_px(TYPE_BODY_PX, 144, 1.0), -21);
        assert_eq!(type_height_px(TYPE_BODY_PX, 96, 1.5), -21);
        assert_eq!(type_height_px(TYPE_CAPTION_PX, 192, 1.0), -24);
        // Never zero, whatever the inputs.
        assert_eq!(type_height_px(TYPE_CAPTION_PX, 96, 0.0), -1);
    }

    #[test]
    fn type_ramp_orders_and_spacing_sits_on_the_grid() {
        const { assert!(TYPE_SUBTITLE_PX > TYPE_BODY_PX && TYPE_BODY_PX > TYPE_CAPTION_PX) };
        const { assert!(TYPE_BODY_LINE_PX > TYPE_BODY_PX && TYPE_CAPTION_LINE_PX > TYPE_CAPTION_PX) };
        for s in [
            SPACE_XS_DP,
            SPACE_SM_DP,
            SPACE_MD_DP,
            SPACE_LG_DP,
            SPACE_XL_DP,
        ] {
            assert_eq!(s % 4, 0);
        }
        assert_eq!((MARGIN_DP, PADDING_DP, GAP_DP), (16, 16, 8));
        assert_eq!((CONTROL_RADIUS_DP, WINDOW_RADIUS_DP), (4, 8));
    }

    #[test]
    fn the_icon_font_falls_back_to_mdl2_when_fluent_is_missing() {
        assert_eq!(icon_font_face(true), "Segoe Fluent Icons");
        assert_eq!(icon_font_face(false), "Segoe MDL2 Assets");
        assert_eq!(GLYPH_SEARCH as u32, 0xE721);
    }

    #[test]
    fn a_text_box_is_a_step_off_the_surface_except_in_high_contrast() {
        assert_ne!(LIGHT.control_fill, LIGHT.surface);
        assert_ne!(DARK.control_fill, DARK.surface);
        let hc = Tokens::for_theme(Theme::HighContrast, &HC_WHITE);
        assert_eq!(hc.control_fill, HC_WHITE.window);
    }

    #[test]
    fn a_keycap_is_a_neutral_step_off_its_row_and_stays_readable() {
        for (theme, sys) in [(Theme::Light, HC_BLACK), (Theme::Dark, HC_BLACK)] {
            let t = Tokens::for_theme(theme, &sys);
            let fill = keycap_fill(theme, t.surface, t.text);
            assert_ne!(fill, t.surface, "{theme:?}: the cap is a step off the row");
            let ratio = |a, b| contrast_ratio(relative_luminance(a), relative_luminance(b));
            assert!(ratio(t.text_dim, fill) >= 4.5, "{theme:?}: digit on cap");
        }
        // High contrast has no tints: the row colour, the border carries it.
        assert_eq!(
            keycap_fill(Theme::HighContrast, HC_WHITE.window, HC_WHITE.window_text),
            HC_WHITE.window
        );
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
    fn text_pairs(t: &Tokens) -> [(&'static str, u32, u32); 7] {
        [
            ("text on a text box", t.text, t.control_fill),
            ("text_dim on a text box", t.text_dim, t.control_fill),
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
        assert_eq!(checked, 28, "the test must look at every pair");
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
        assert_eq!(t.text_faint, HC_WHITE.window_text);
        assert_ne!(
            t.text_faint, HC_WHITE.gray_text,
            "GrayText means disabled (#816)"
        );
        // Links are not greyed text: Windows keeps GrayText for disabled
        // items and gives links their own colour.
        assert_eq!(t.link, HC_WHITE.hotlight);
        assert_ne!(t.link, t.text_faint);
        // Light and dark ignore the system colours entirely.
        assert_eq!(Tokens::for_theme(Theme::Light, &HC_WHITE), LIGHT);
        assert_eq!(Tokens::for_theme(Theme::Dark, &HC_BLACK), DARK);
    }

    #[test]
    fn only_theme_names_and_the_high_contrast_flag_are_theme_changes() {
        let hc = SPI_SETHIGHCONTRAST.0 as usize;
        assert!(setting_change_affects_theme(0, "ImmersiveColorSet"));
        assert!(setting_change_affects_theme(hc, ""));
        assert!(!setting_change_affects_theme(0, "intl"));
        assert!(!setting_change_affects_theme(0, ""));
    }

    #[test]
    fn only_the_immersive_color_set_setting_is_a_theme_change() {
        assert!(setting_change_is_theme("ImmersiveColorSet"));
        assert!(!setting_change_is_theme("intl"));
        assert!(!setting_change_is_theme(""));
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
