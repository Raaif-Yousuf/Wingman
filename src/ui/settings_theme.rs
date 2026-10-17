//! Which look the Settings window is drawn in, and the pure decisions that
//! follow from it (#582, spec `2026-10-16-settings-dark-and-high-contrast`).
//!
//! Settings is made of stock controls, so only a dark window paints anything of
//! its own. Light and high contrast keep the system's drawing exactly as it
//! was, which is why every decision here is "does the custom path run".

use crate::ui::theme::{Theme, Tokens};

/// Whether Settings fills its own face and answers the colour messages. Only
/// dark does for a real window. A forced high contrast (the gallery, which
/// cannot switch the system theme) does too, so its picture shows the tokens.
pub(crate) fn paints_face(theme: Theme, forced: bool) -> bool {
    theme == Theme::Dark || (forced && theme == Theme::HighContrast)
}

/// The window face: the system's own where the system draws it, the surface
/// token where Settings paints it.
pub(crate) fn face_color(theme: Theme, forced: bool, tokens: &Tokens, system_face: u32) -> u32 {
    if paints_face(theme, forced) {
        tokens.surface
    } else {
        system_face
    }
}

/// The value for `DWMWA_USE_IMMERSIVE_DARK_MODE`: dark for dark, light for
/// light, and `None` (leave it) in high contrast, where the system draws the
/// caption.
pub(crate) fn title_bar_dark(theme: Theme) -> Option<bool> {
    match theme {
        Theme::Dark => Some(true),
        Theme::Light => Some(false),
        Theme::HighContrast => None,
    }
}

/// The visual-style class that darkens a non-client scrollbar. Documented API
/// (`SetWindowTheme`) with an undocumented name; MEASURED 2026-10-16 to darken a
/// `WS_VSCROLL` bar with no ordinal call, and to change nothing on edits, combos
/// or the trackbar, so it is applied to the scrolling body only (spec: Scrollbar).
pub(crate) const DARK_SCROLLBAR_CLASS: &str = "DarkMode_Explorer";

/// Whether the body's scrollbar takes the dark visual style. Only dark does:
/// light and high contrast keep the stock bar (high contrast included when the
/// gallery forces it, since the system draws that bar from its own colours).
pub(crate) fn scrollbar_is_dark(theme: Theme) -> bool {
    theme == Theme::Dark
}

/// Whether `button` is a common controls v6 button. `BCM_GETIDEALSIZE` exists
/// only in v6 (a v5 button answers 0), so this asks the control itself, not
/// the DLL, and is true exactly when `NM_CUSTOMDRAW` will arrive for it (the
/// custom draw of buttons and checkboxes depends on it, #582). The manifest
/// that requests v6 is `assets/app.manifest` (#807).
#[cfg(any(test, debug_assertions))]
pub(crate) fn button_is_common_controls_v6(button: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::Foundation::{LPARAM, SIZE, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
    const BCM_GETIDEALSIZE: u32 = 0x1601;
    let mut size = SIZE::default();
    // SAFETY: the message writes one SIZE through the pointer, which outlives
    // the synchronous call.
    unsafe {
        SendMessageW(
            button,
            BCM_GETIDEALSIZE,
            Some(WPARAM(0)),
            Some(LPARAM(&mut size as *mut SIZE as isize)),
        )
        .0 != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Theme::*, DARK, HC_BLACK, LIGHT};

    /// #582 slice 6: the gate that kept dark off for real users is gone, so a
    /// dark system draws a dark Settings window. A source scan, since the
    /// window reads the theme through Win32. The candidate set must not be
    /// empty, and the carriage return is dropped so CRLF checkouts scan the same text.
    #[test]
    fn the_dark_gate_no_longer_exists() {
        let sources = [
            include_str!("settings.rs"),
            include_str!("settings_theme.rs"),
        ];
        let needle = concat!("SETTINGS_DARK", "_ENABLED");
        let gate_fn = concat!("fn ", "gate(");
        let mut scanned = 0;
        for src in sources {
            let src = src.replace('\r', "");
            assert!(!src.is_empty());
            scanned += 1;
            assert!(!src.contains(needle), "the gate constant is back");
            assert!(!src.contains(gate_fn), "the gate function is back");
        }
        assert_eq!(scanned, 2);
    }

    #[test]
    fn only_dark_and_a_forced_high_contrast_paint_the_face() {
        assert!(paints_face(Dark, false));
        assert!(paints_face(Dark, true));
        assert!(paints_face(HighContrast, true));
        assert!(!paints_face(HighContrast, false), "the system draws it");
        assert!(!paints_face(Light, false));
        assert!(!paints_face(Light, true), "the gallery's light is stock");
    }

    #[test]
    fn the_face_is_the_surface_token_only_where_settings_paints_it() {
        let system = 0x00F0_F0F0;
        assert_eq!(face_color(Dark, false, &DARK, system), DARK.surface);
        assert_eq!(face_color(Light, false, &LIGHT, system), system);
        assert_eq!(face_color(HighContrast, false, &LIGHT, system), system);
        let hc = Tokens::for_theme(HighContrast, &HC_BLACK);
        assert_eq!(face_color(HighContrast, true, &hc, system), HC_BLACK.window);
    }

    #[test]
    fn only_dark_darkens_the_scrollbar() {
        assert!(scrollbar_is_dark(Dark));
        assert!(!scrollbar_is_dark(Light));
        assert!(!scrollbar_is_dark(HighContrast), "the system draws it");
        assert_eq!(DARK_SCROLLBAR_CLASS, "DarkMode_Explorer");
    }

    #[test]
    fn the_title_bar_follows_the_theme_and_is_left_alone_in_high_contrast() {
        assert_eq!(title_bar_dark(Dark), Some(true));
        assert_eq!(title_bar_dark(Light), Some(false));
        assert_eq!(title_bar_dark(HighContrast), None);
    }
}
