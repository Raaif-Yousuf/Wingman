//! The arrow button of a Settings combo box, drawn from the tokens (#582, dark
//! slice 3). The system draws it light, and an owner-drawn combo hands the app
//! only the field and the list rows, so the button is painted over after the
//! system's paint. Only a window that paints its own face calls this; light and
//! the system's high contrast keep the system's button.

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
    let points: Vec<windows::Win32::Foundation::POINT> = chevron_points(button, dpi)
        .iter()
        .map(|&(x, y)| windows::Win32::Foundation::POINT { x, y })
        .collect();
    let _ = MoveToEx(hdc, points[0].x, points[0].y, None);
    let _ = Polyline(hdc, &points);
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
    fn an_empty_button_is_never_drawn() {
        // No device context is touched for an empty rect.
        let rc = RECT::default();
        unsafe { paint_arrow(HDC::default(), &rc, &rc, 96, 0, 0) };
    }
}
