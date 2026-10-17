//! The arrow button of a Settings combo box, drawn from the tokens (#582, dark
//! slice 3). The system draws it light, and an owner-drawn combo hands the app
//! only the field and the list rows, so the button is painted over after the
//! system's paint. Only a window that paints its own face calls this; light and
//! the system's high contrast keep the system's button.
//!
//! Every Settings combo is `CBS_DROPDOWNLIST`. An editable `CBS_DROPDOWN` would
//! also have an edit child that needs its own background.

use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreatePen, CreateSolidBrush, DeleteObject, FillRect, MoveToEx, Polyline, SelectObject, HDC,
    HGDIOBJ, PS_SOLID,
};

use crate::ui::theme::scale_for_dpi;

/// The chevron's half width and height, in dp: an 8 dp by 4 dp mark, as
/// Windows 11's combo box draws.
const CHEVRON_HALF_W_DP: i32 = 4;
const CHEVRON_HALF_H_DP: i32 = 2;

/// The three points of the chevron, centred in `button` (left, bottom-middle,
/// right), for a window at `dpi`.
pub(crate) fn chevron_points(button: &RECT, dpi: u32) -> [(i32, i32); 3] {
    let hw = scale_for_dpi(CHEVRON_HALF_W_DP, dpi).max(2);
    let hh = scale_for_dpi(CHEVRON_HALF_H_DP, dpi).max(1);
    let cx = (button.left + button.right) / 2;
    let cy = (button.top + button.bottom) / 2;
    [(cx - hw, cy - hh), (cx, cy + hh), (cx + hw, cy - hh)]
}

/// `STATE_SYSTEM_PRESSED` in `COMBOBOXINFO::stateButton`: set while the button
/// is held down and while the list is dropped open.
const STATE_SYSTEM_PRESSED: u32 = 0x0000_0008;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArrowState {
    Normal,
    Hot,
    /// Held down, or the list is dropped open.
    Pressed,
}

/// The look of the button: pressed (or dropped) wins over hot, and a disabled
/// combo has neither.
pub(crate) fn arrow_state(
    enabled: bool,
    hot: bool,
    dropped: bool,
    state_button: u32,
) -> ArrowState {
    if !enabled {
        ArrowState::Normal
    } else if dropped || state_button & STATE_SYSTEM_PRESSED != 0 {
        ArrowState::Pressed
    } else if hot {
        ArrowState::Hot
    } else {
        ArrowState::Normal
    }
}

/// The button's `(fill, ink)` in `state`. `highlight` is the system's
/// highlight and its text, which a high contrast window uses for the two active
/// states instead of a tint (no blended pixel is a system colour).
pub(crate) fn arrow_colors(
    state: ArrowState,
    fill: u32,
    ink: u32,
    highlight: Option<(u32, u32)>,
) -> (u32, u32) {
    use crate::ui::card_buttons::{blend, SECONDARY_HOVER_PCT, SECONDARY_PRESSED_PCT};
    match (state, highlight) {
        (ArrowState::Normal, _) => (fill, ink),
        (_, Some(pair)) => pair,
        (ArrowState::Hot, None) => (blend(fill, ink, SECONDARY_HOVER_PCT), ink),
        (ArrowState::Pressed, None) => (blend(fill, ink, SECONDARY_PRESSED_PCT), ink),
    }
}

/// The chevron's stroke in px: 1 dp, never thinner than one pixel.
fn stroke_px(dpi: u32) -> i32 {
    scale_for_dpi(1, dpi).max(1)
}

/// Paints the button of a combo over the system's: `fill` behind, the chevron
/// in `ink`. `field` and `button` are the rectangles COMBOBOXINFO reports, in
/// the DC's coordinates. The system leaves a light gap between the two, so the
/// fill runs from the field's right edge to the button's.
///
/// # Safety
/// `hdc` must be a live device context.
pub(crate) unsafe fn paint_arrow(
    hdc: HDC,
    field: &RECT,
    button: &RECT,
    dpi: u32,
    fill: u32,
    ink: u32,
) {
    if button.right <= button.left || button.bottom <= button.top {
        return;
    }
    let span = RECT {
        left: field.right.min(button.left),
        ..*button
    };
    let brush = CreateSolidBrush(COLORREF(fill));
    FillRect(hdc, &span, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));
    let pen = CreatePen(PS_SOLID, stroke_px(dpi), COLORREF(ink));
    let old = SelectObject(hdc, HGDIOBJ(pen.0));
    let [a, b, c] =
        chevron_points(button, dpi).map(|(x, y)| windows::Win32::Foundation::POINT { x, y });
    let _ = MoveToEx(hdc, a.x, a.y, None);
    let _ = Polyline(hdc, &[a, b, c]);
    SelectObject(hdc, old);
    let _ = DeleteObject(HGDIOBJ(pen.0));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_chevron_is_centred_points_down_and_scales() {
        for dpi in [96u32, 120, 144, 192, 240] {
            let button = RECT {
                left: 100,
                top: 0,
                right: 100 + scale_for_dpi(30, dpi),
                bottom: scale_for_dpi(32, dpi),
            };
            let [a, b, c] = chevron_points(&button, dpi);
            assert!(a.0 < b.0 && b.0 < c.0, "left to right ({dpi} dpi)");
            assert!(b.1 > a.1 && a.1 == c.1, "points down ({dpi} dpi)");
            assert_eq!(b.0, (button.left + button.right) / 2);
            assert!(
                a.0 > button.left && c.0 < button.right,
                "inside ({dpi} dpi)"
            );
            assert!(a.1 > button.top && b.1 < button.bottom);
            assert_eq!(c.0 - b.0, b.0 - a.0, "symmetric");
        }
        let at = |dpi| {
            let r = RECT {
                left: 0,
                top: 0,
                right: 40,
                bottom: 40,
            };
            let [a, _, c] = chevron_points(&r, dpi);
            c.0 - a.0
        };
        assert!(at(192) > at(96), "it grows with the scale");
    }

    #[test]
    fn the_button_reads_pressed_then_hot_and_a_disabled_combo_neither() {
        use ArrowState::*;
        assert_eq!(arrow_state(true, false, false, 0), Normal);
        assert_eq!(arrow_state(true, true, false, 0), Hot);
        assert_eq!(
            arrow_state(true, true, false, STATE_SYSTEM_PRESSED),
            Pressed
        );
        assert_eq!(arrow_state(true, false, true, 0), Pressed);
        assert_eq!(arrow_state(false, true, true, STATE_SYSTEM_PRESSED), Normal);
    }

    #[test]
    fn hot_and_pressed_step_away_from_the_resting_fill() {
        use crate::ui::theme::DARK;
        let (f, i) = (DARK.control_fill, DARK.text);
        let rest = arrow_colors(ArrowState::Normal, f, i, None);
        let hot = arrow_colors(ArrowState::Hot, f, i, None);
        let down = arrow_colors(ArrowState::Pressed, f, i, None);
        assert_eq!(rest, (f, i));
        assert!(hot.0 != f && down.0 != hot.0 && down.0 != f);
        assert_eq!((hot.1, down.1), (i, i));
        // High contrast: the system highlight pair, no tint.
        let pair = (0x00FF_0000, 0x0000_FF00);
        let hc = Some(pair);
        assert_eq!(arrow_colors(ArrowState::Hot, f, i, hc), pair);
        assert_eq!(arrow_colors(ArrowState::Normal, f, i, hc), (f, i));
    }

    #[test]
    fn an_empty_button_is_never_drawn() {
        // No device context is touched for an empty rect.
        let rc = RECT::default();
        unsafe { paint_arrow(HDC::default(), &rc, &rc, 96, 0, 0) };
    }
}
