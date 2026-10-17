//! The Settings trackbar when Settings paints its own look (#582, slice 4 of
//! `2026-10-16-settings-dark-and-high-contrast`).
//!
//! It stays the system's `msctls_trackbar32` (arrow keys, page keys, the value
//! and its UI Automation range pattern are the control's). A trackbar sends
//! `NM_CUSTOMDRAW` to its parent on every common controls version: a pre-paint
//! stage, then one item stage each for the tick marks, the thumb and the
//! channel. Answering `CDRF_SKIPDEFAULT` after drawing replaces that part.
//!
//! The decisions ([`look`], [`channel_rects`], [`thumb_rects`]) are pure and
//! unit-tested; [`handle_custom_draw`] is the thin GDI layer. Every colour is
//! a token or a system colour handed in. The two end marks the system always
//! draws are left out (they would keep their stock colour).
//! Nothing here runs for light or for the system's own high contrast: the
//! caller only asks while Settings paints its face.

use crate::ui::card_buttons::HighContrast;
use crate::ui::settings_buttons::{Palette, NM_CUSTOMDRAW_CODE};
use crate::ui::theme::{scale_for_dpi, TRACK_CHANNEL_DP, TRACK_THUMB_DP};
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::HDC;
use windows::Win32::UI::Controls::NMCUSTOMDRAW;
use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, SendMessageW};

const CDDS_PREPAINT: u32 = 0x0000_0001;
const CDDS_ITEMPREPAINT: u32 = 0x0001_0001;
const CDRF_DODEFAULT: isize = 0x0000_0000;
const CDRF_SKIPDEFAULT: isize = 0x0000_0004;
const CDRF_NOTIFYITEMDRAW: isize = 0x0000_0020;

const CDIS_SELECTED: u32 = 0x0001;
const CDIS_DISABLED: u32 = 0x0004;
const CDIS_HOT: u32 = 0x0040;

/// `dwItemSpec` of the item stages: tick marks, thumb, channel.
const TBCD_TICS: usize = 1;
const TBCD_THUMB: usize = 2;
const TBCD_CHANNEL: usize = 3;

/// `TBM_GETTHUMBRECT`: the thumb's rectangle in the trackbar's client space.
const TBM_GETTHUMBRECT: u32 = 0x0419;

/// What the pointer or the keyboard is doing to the thumb.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThumbState {
    Normal,
    Hot,
    Pressed,
    Disabled,
}

/// Disabled wins over pressed (a drag), pressed over hot.
pub(crate) fn state_from_cdis(item_state: u32, enabled: bool) -> ThumbState {
    if !enabled || item_state & CDIS_DISABLED != 0 {
        ThumbState::Disabled
    } else if item_state & CDIS_SELECTED != 0 {
        ThumbState::Pressed
    } else if item_state & CDIS_HOT != 0 {
        ThumbState::Hot
    } else {
        ThumbState::Normal
    }
}

/// The colours of the trackbar in one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TrackLook {
    /// The rail the thumb has not passed.
    pub rail: u32,
    /// The rail from the left end to the thumb.
    pub filled: u32,
    /// The thumb's disc.
    pub thumb: u32,
    /// The dot in the thumb's middle, `None` while disabled.
    pub dot: Option<u32>,
}

/// The dot's diameter as a percentage of the thumb's: it swells under the
/// pointer and shrinks while the thumb is held, as in Windows 11.
pub(crate) fn dot_percent(state: ThumbState) -> i32 {
    match state {
        ThumbState::Normal => 55,
        ThumbState::Hot => 70,
        ThumbState::Pressed => 40,
        ThumbState::Disabled => 0,
    }
}

/// The colours in `state`. Dark: the rail is the control border token (3:1
/// against the face), the filled part and the dot are the accent, the thumb is
/// the text token. Disabled drops the accent. High contrast uses the system
/// colours only.
pub(crate) fn look(state: ThumbState, palette: &Palette) -> TrackLook {
    let disabled = state == ThumbState::Disabled;
    if let Some(HighContrast {
        text,
        highlight,
        gray_text,
        ..
    }) = palette.high_contrast
    {
        return if disabled {
            TrackLook {
                rail: gray_text,
                filled: gray_text,
                thumb: gray_text,
                dot: None,
            }
        } else {
            TrackLook {
                rail: text,
                filled: highlight,
                thumb: text,
                dot: Some(highlight),
            }
        };
    }
    let t = &palette.tokens;
    if disabled {
        TrackLook {
            rail: t.border,
            filled: t.border,
            thumb: t.text_faint,
            dot: None,
        }
    } else {
        TrackLook {
            rail: t.control_border,
            filled: palette.accent,
            thumb: t.text,
            dot: Some(palette.accent),
        }
    }
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

/// The whole rail and the part of it left of `thumb_x` (the thumb's centre), as
/// two rounded bars centred on the channel rectangle `rc`.
pub(crate) fn channel_rects(rc: &RECT, dpi: u32, thumb_x: i32) -> (RECT, RECT) {
    let thick = scale_for_dpi(TRACK_CHANNEL_DP, dpi).min((rc.bottom - rc.top).max(1));
    let top = (rc.top + rc.bottom - thick) / 2;
    let full = rect(rc.left, top, rc.right.max(rc.left), top + thick);
    let end = thumb_x.max(rc.left).min(rc.right.max(rc.left));
    (full, rect(rc.left, top, end, top + thick))
}

/// The thumb's disc and the dot inside it, centred on the thumb rectangle `rc`.
/// The disc never leaves `rc` (the control repaints only that rectangle when
/// the thumb moves), so it is the smaller of the rectangle's sides and the
/// token diameter.
pub(crate) fn thumb_rects(rc: &RECT, dpi: u32, state: ThumbState) -> (RECT, RECT) {
    let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
    let d = scale_for_dpi(TRACK_THUMB_DP, dpi).min(w).min(h).max(0);
    let (cx, cy) = ((rc.left + rc.right) / 2, (rc.top + rc.bottom) / 2);
    let disc = rect(cx - d / 2, cy - d / 2, cx - d / 2 + d, cy - d / 2 + d);
    let dd = d * dot_percent(state) / 100;
    let dot = rect(cx - dd / 2, cy - dd / 2, cx - dd / 2 + dd, cy - dd / 2 + dd);
    (disc, dot)
}

unsafe fn is_trackbar(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut name = [0u16; 24];
    let n = GetClassNameW(hwnd, &mut name) as usize;
    String::from_utf16_lossy(&name[..n.min(name.len())]).eq_ignore_ascii_case("msctls_trackbar32")
}

/// The answer to an `NM_CUSTOMDRAW` from a trackbar, or `None` when it is not
/// one (the caller then offers it to the other painters). Paints the channel
/// and the thumb from `palette` and answers `CDRF_SKIPDEFAULT` for them; the
/// pre-paint stage asks for the item stages, and the end marks are
/// skipped.
///
/// # Safety
/// `lparam` must be the `NMCUSTOMDRAW` pointer of a `WM_NOTIFY`.
pub(crate) unsafe fn handle_custom_draw(
    lparam: LPARAM,
    dpi: u32,
    palette: &Palette,
) -> Option<isize> {
    let nm = &*(lparam.0 as *const NMCUSTOMDRAW);
    if nm.hdr.code != NM_CUSTOMDRAW_CODE || !is_trackbar(nm.hdr.hwndFrom) {
        return None;
    }
    let stage = nm.dwDrawStage.0;
    if stage == CDDS_PREPAINT {
        return Some(CDRF_NOTIFYITEMDRAW);
    }
    if stage != CDDS_ITEMPREPAINT {
        return Some(CDRF_DODEFAULT);
    }
    let hwnd = nm.hdr.hwndFrom;
    let state = state_from_cdis(nm.uItemState.0, IsWindowEnabled(hwnd).as_bool());
    let colors = look(state, palette);
    let smooth = palette.high_contrast.is_none();
    match nm.dwItemSpec {
        TBCD_CHANNEL => {
            let mut thumb = RECT::default();
            SendMessageW(
                hwnd,
                TBM_GETTHUMBRECT,
                None,
                Some(LPARAM(&mut thumb as *mut RECT as isize)),
            );
            let (full, filled) = channel_rects(&nm.rc, dpi, (thumb.left + thumb.right) / 2);
            fill_pill(nm.hdc, full, colors.rail, smooth);
            fill_pill(nm.hdc, filled, colors.filled, smooth);
            Some(CDRF_SKIPDEFAULT)
        }
        TBCD_THUMB => {
            let (disc, dot) = thumb_rects(&nm.rc, dpi, state);
            fill_pill(nm.hdc, disc, colors.thumb, smooth);
            if let Some(dot_color) = colors.dot {
                fill_pill(nm.hdc, dot, dot_color, smooth);
            }
            Some(CDRF_SKIPDEFAULT)
        }
        // The two end marks the system always draws have no value to read and
        // would keep their stock colour (near black in forced high contrast), so
        // this look has none, as the Windows 11 slider.
        TBCD_TICS => Some(CDRF_SKIPDEFAULT),
        _ => Some(CDRF_DODEFAULT),
    }
}

/// A filled bar whose ends are half circles (a disc when it is square).
unsafe fn fill_pill(hdc: HDC, rc: RECT, color: u32, smooth: bool) {
    let radius = (rc.right - rc.left).min(rc.bottom - rc.top) / 2;
    crate::ui::round_rect::paint(hdc, rc, radius, Some(color), None, smooth);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{contrast_ratio, relative_luminance, Theme, DARK, HC_BLACK};

    fn dark() -> Palette {
        Palette::new(Theme::Dark, &HC_BLACK, 0x00D4_7800)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        contrast_ratio(relative_luminance(a), relative_luminance(b))
    }

    #[test]
    fn the_item_state_reads_disabled_then_pressed_then_hot() {
        assert_eq!(state_from_cdis(0, true), ThumbState::Normal);
        assert_eq!(state_from_cdis(CDIS_HOT, true), ThumbState::Hot);
        assert_eq!(
            state_from_cdis(CDIS_HOT | CDIS_SELECTED, true),
            ThumbState::Pressed
        );
        assert_eq!(state_from_cdis(CDIS_SELECTED, false), ThumbState::Disabled);
        assert_eq!(state_from_cdis(CDIS_DISABLED, true), ThumbState::Disabled);
    }

    #[test]
    fn the_dark_look_comes_from_the_tokens_and_reads_on_the_face() {
        let l = look(ThumbState::Normal, &dark());
        assert_eq!(l.rail, DARK.control_border);
        assert_eq!(l.filled, 0x00D4_7800);
        assert_eq!(l.thumb, DARK.text);
        assert!(contrast(l.rail, DARK.surface) >= 3.0, "rail 3:1");
        assert!(contrast(l.thumb, DARK.surface) >= 3.0, "thumb 3:1");
    }

    #[test]
    fn disabled_drops_the_accent_and_the_dot() {
        let l = look(ThumbState::Disabled, &dark());
        assert_ne!(l.filled, 0x00D4_7800);
        assert_eq!(l.dot, None);
        assert_ne!(l, look(ThumbState::Normal, &dark()));
    }

    #[test]
    fn the_dot_swells_when_hot_and_shrinks_when_pressed() {
        assert!(dot_percent(ThumbState::Hot) > dot_percent(ThumbState::Normal));
        assert!(dot_percent(ThumbState::Normal) > dot_percent(ThumbState::Pressed));
        assert_eq!(dot_percent(ThumbState::Disabled), 0);
    }

    #[test]
    fn forced_high_contrast_uses_the_system_colours() {
        let p = Palette::new(Theme::HighContrast, &HC_BLACK, HC_BLACK.highlight);
        let l = look(ThumbState::Normal, &p);
        assert_eq!(l.rail, HC_BLACK.window_text);
        assert_eq!(l.filled, HC_BLACK.highlight);
        assert_eq!(l.thumb, HC_BLACK.window_text);
        assert_eq!(look(ThumbState::Disabled, &p).thumb, HC_BLACK.gray_text);
    }

    #[test]
    fn the_rail_is_centred_and_filled_up_to_the_thumb() {
        for dpi in [96u32, 144, 192, 240] {
            let rc = rect(10, 4, 210, 4 + scale_for_dpi(8, dpi));
            let (full, filled) = channel_rects(&rc, dpi, 90);
            assert_eq!(full.bottom - full.top, scale_for_dpi(TRACK_CHANNEL_DP, dpi));
            assert!((full.top - rc.top - (rc.bottom - full.bottom)).abs() <= 1);
            assert_eq!((full.left, full.right), (10, 210));
            assert_eq!((filled.left, filled.right), (10, 90));
        }
    }

    #[test]
    fn the_filled_part_never_leaves_the_rail() {
        let rc = rect(10, 0, 110, 8);
        assert_eq!(channel_rects(&rc, 96, -50).1.right, 10);
        assert_eq!(channel_rects(&rc, 96, 900).1.right, 110);
        let flat = rect(5, 3, 5, 3);
        let (full, filled) = channel_rects(&flat, 96, 5);
        assert!(full.right >= full.left && filled.right >= filled.left);
    }

    #[test]
    fn the_thumb_stays_inside_its_rectangle_and_is_round() {
        for dpi in [96u32, 144, 192, 240] {
            for state in [ThumbState::Normal, ThumbState::Hot, ThumbState::Pressed] {
                let rc = rect(
                    40,
                    2,
                    40 + scale_for_dpi(11, dpi),
                    2 + scale_for_dpi(21, dpi),
                );
                let (disc, dot) = thumb_rects(&rc, dpi, state);
                assert_eq!(disc.right - disc.left, disc.bottom - disc.top, "round");
                assert!(disc.left >= rc.left && disc.right <= rc.right);
                assert!(disc.top >= rc.top && disc.bottom <= rc.bottom);
                assert!(dot.left >= disc.left && dot.right <= disc.right);
                assert!(dot.top >= disc.top && dot.bottom <= disc.bottom);
            }
        }
        let (disc, dot) = thumb_rects(&rect(0, 0, 0, 0), 96, ThumbState::Normal);
        assert!(disc.right >= disc.left && dot.right >= dot.left);
    }

    #[test]
    fn the_paint_code_has_no_literal_colours() {
        let src = include_str!("settings_trackbar.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]").next().expect("code part");
        assert!(code.lines().count() > 100, "the scan must see the file");
        for (n, line) in code.lines().enumerate() {
            let t = line.trim_start();
            if t.starts_with("//") {
                continue;
            }
            assert!(!line.contains("rgb("), "line {}: rgb(", n + 1);
            assert!(
                !line.contains("COLORREF(0") && !line.contains("GetStockObject"),
                "line {}: a literal colour or stock object",
                n + 1
            );
        }
    }
}
