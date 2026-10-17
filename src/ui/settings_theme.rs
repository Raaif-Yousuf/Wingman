//! Which look the Settings window is drawn in, and the pure decisions that
//! follow from it (#582, spec `2026-10-16-settings-dark-and-high-contrast`).
//!
//! Settings is made of stock controls, so only a dark window paints anything of
//! its own. Light and high contrast keep the system's drawing exactly as it
//! was, which is why every decision here is "does the custom path run".

use crate::ui::theme::{Theme, Tokens};

/// Dark mode is built slice by slice; until the last slice it is off for real
/// users, so nobody sees a half-dark window. The UI gallery draws it anyway
/// (it forces a theme). The spec's last slice deletes this constant.
pub(crate) const SETTINGS_DARK_ENABLED: bool = false;

/// The theme a real window follows: the detected one, with dark turned back
/// into light while `dark_enabled` is false. High contrast is never touched.
pub(crate) fn gate(detected: Theme, dark_enabled: bool) -> Theme {
    match detected {
        Theme::Dark if !dark_enabled => Theme::Light,
        other => other,
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{Theme::*, DARK, HC_BLACK, LIGHT};

    #[test]
    fn dark_is_off_for_real_users_until_the_last_slice() {
        const { assert!(!SETTINGS_DARK_ENABLED) };
        assert_eq!(gate(Dark, false), Light);
        assert_eq!(gate(Dark, true), Dark);
        assert_eq!(gate(Light, false), Light);
        assert_eq!(gate(Light, true), Light);
        assert_eq!(gate(HighContrast, false), HighContrast);
        assert_eq!(gate(HighContrast, true), HighContrast);
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
    fn the_title_bar_follows_the_theme_and_is_left_alone_in_high_contrast() {
        assert_eq!(title_bar_dark(Dark), Some(true));
        assert_eq!(title_bar_dark(Light), Some(false));
        assert_eq!(title_bar_dark(HighContrast), None);
    }
}
