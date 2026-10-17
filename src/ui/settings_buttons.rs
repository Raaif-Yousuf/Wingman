//! Push buttons and checkboxes of the Settings window when it paints its own
//! look (#582, slice 2 of `2026-10-16-settings-dark-and-high-contrast`).
//!
//! Both stay real `BUTTON` controls (class, style, tab order, UI Automation
//! role and toggle state are the system's). With common controls v6, which the
//! exe's manifest asks for (#807), a button sends `NM_CUSTOMDRAW` to its
//! parent before it paints; answering `CDRF_SKIPDEFAULT` after drawing replaces
//! the stock paint. MEASURED 2026-10-16 (spec, Slice 0): the notification
//! arrives for push buttons and checkboxes and a custom-drawn checkbox keeps
//! ControlType CheckBox with its Toggle pattern; `BS_OWNERDRAW` would lose both.
//! That is why this is custom draw and not the owner draw the spec first named
//! for push buttons.
//!
//! The decisions ([`state_from_cdis`], [`check_look`], [`check_geometry`],
//! [`check_points`]) are pure and unit-tested. [`handle_custom_draw`] is the
//! thin GDI layer. Every colour comes from the theme tokens, none is written
//! here, and nothing in this file runs for light or for the system's own high
//! contrast: the caller only asks while Settings paints its face.

use crate::ui::card_buttons::{self, HighContrast, Role, State, Surface};
use crate::ui::theme::{
    scale_for_dpi, SysColors, Theme, Tokens, CHECK_BOX_DP, CHECK_TEXT_GAP_DP, CONTROL_RADIUS_DP,
    FOCUS_LINE_DP, SPACE_XS_DP,
};
use windows::Win32::Foundation::{COLORREF, LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    CreatePen, DeleteObject, DrawTextW, FillRect, MoveToEx, Polyline, SelectObject, SetBkMode,
    SetTextColor, DT_CALCRECT, DT_END_ELLIPSIS, DT_HIDEPREFIX, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    HBRUSH, HFONT, HGDIOBJ, PS_SOLID, TRANSPARENT,
};
use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, NMCUSTOMDRAW};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetWindowLongW, GetWindowTextW, SendMessageW, GWL_STYLE, WM_GETFONT,
};

/// `NM_CUSTOMDRAW` (`NM_FIRST - 12`).
pub(crate) const NM_CUSTOMDRAW_CODE: u32 = -12i32 as u32;

const CDDS_PREPAINT: u32 = 0x0000_0001;
const CDRF_DODEFAULT: isize = 0x0000_0000;
const CDRF_SKIPDEFAULT: isize = 0x0000_0004;

const CDIS_SELECTED: u32 = 0x0001;
const CDIS_GRAYED: u32 = 0x0002;
const CDIS_DISABLED: u32 = 0x0004;
const CDIS_FOCUS: u32 = 0x0010;
const CDIS_HOT: u32 = 0x0040;

/// `WM_QUERYUISTATE` and the bits it answers with: Windows hides the focus
/// rectangle and the accelerator underline until the keyboard is used.
const WM_QUERYUISTATE: u32 = 0x0129;
const UISF_HIDEFOCUS: u32 = 0x1;
const UISF_HIDEACCEL: u32 = 0x2;

const BM_GETCHECK: u32 = 0x00F0;
const BST_CHECKED: isize = 1;

const BS_TYPE_MASK: u32 = 0xF;
const BS_PUSHLIKE: u32 = 0x1000;

/// The colours one paint needs: the theme's tokens, the accent, and (forced
/// high contrast only) the system colours.
#[derive(Clone, Copy)]
pub(crate) struct Palette {
    pub tokens: Tokens,
    pub accent: u32,
    pub high_contrast: Option<HighContrast>,
}

impl Palette {
    pub(crate) fn new(theme: Theme, sys: &SysColors, accent: u32) -> Palette {
        Palette {
            tokens: Tokens::for_theme(theme, sys),
            accent,
            high_contrast: (theme == Theme::HighContrast).then_some(HighContrast {
                face: sys.window,
                text: sys.window_text,
                highlight: sys.highlight,
                highlight_text: sys.highlight_text,
                gray_text: sys.gray_text,
            }),
        }
    }

    fn surface(&self) -> Surface {
        Surface {
            bg: self.tokens.surface,
            border: self.tokens.control_border,
            text: self.tokens.text,
            hint: self.tokens.text_faint,
        }
    }
}

/// What a `uItemState` from `NM_CUSTOMDRAW` means for the paint. Disabled wins
/// over pressed, pressed over hot, as in the card's buttons.
pub(crate) fn state_from_cdis(item_state: u32) -> State {
    if item_state & (CDIS_DISABLED | CDIS_GRAYED) != 0 {
        State::Disabled
    } else if item_state & CDIS_SELECTED != 0 {
        State::Pressed
    } else if item_state & CDIS_HOT != 0 {
        State::Hover
    } else {
        State::Normal
    }
}

/// How one checkbox is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CheckLook {
    pub fill: u32,
    pub border: u32,
    /// The tick, drawn only while checked.
    pub glyph: u32,
    pub label: u32,
    pub focus: u32,
}

/// The colours of a checkbox in `state`. Unchecked is an outline on the face
/// (the control border token, 3:1); checked is the accent filled, with the
/// tick in whichever of black and white reads on it. In high contrast the
/// system colours decide (`card_buttons::look` already maps them).
pub(crate) fn check_look(state: State, checked: bool, palette: &Palette) -> CheckLook {
    let role = if checked {
        Role::Primary
    } else {
        Role::Secondary
    };
    let l = card_buttons::look(
        role,
        state,
        &palette.surface(),
        palette.accent,
        palette.high_contrast.as_ref(),
    );
    let t = &palette.tokens;
    let (label, focus) = match (&palette.high_contrast, state) {
        (Some(hc), State::Disabled) => (hc.gray_text, hc.text),
        (Some(hc), _) => (hc.text, hc.text),
        (None, State::Disabled) => (t.text_faint, t.text),
        (None, _) => (t.text, t.text),
    };
    CheckLook {
        fill: l.fill,
        border: l.border,
        glyph: l.text,
        label,
        focus,
    }
}

/// Where the parts of a checkbox go inside its client rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CheckGeometry {
    pub box_rc: RECT,
    pub text_rc: RECT,
    /// The frame drawn round the box and the label while it has the focus.
    pub focus_rc: RECT,
}

/// The layout of a `w` by `h` px checkbox whose label is `text_w` px wide.
/// The box is square, centred on the row and inset by the focus frame's room.
pub(crate) fn check_geometry(w: i32, h: i32, dpi: u32, text_w: i32) -> CheckGeometry {
    let side = scale_for_dpi(CHECK_BOX_DP, dpi).min(h).max(0);
    let left = scale_for_dpi(SPACE_XS_DP, dpi);
    let top = (h - side) / 2;
    let box_rc = RECT {
        left,
        top,
        right: left + side,
        bottom: top + side,
    };
    let text_left = box_rc.right + scale_for_dpi(CHECK_TEXT_GAP_DP, dpi);
    let text_rc = RECT {
        left: text_left,
        top: 0,
        right: w.max(text_left),
        bottom: h,
    };
    let focus_right = (text_left + text_w.max(0) + scale_for_dpi(SPACE_XS_DP, dpi)).min(w);
    let focus_rc = RECT {
        left: 0,
        top: 0,
        right: focus_right.max(box_rc.right),
        bottom: h,
    };
    CheckGeometry {
        box_rc,
        text_rc,
        focus_rc,
    }
}

/// The three corners of the tick, inside `box_rc` (integer percentages of its
/// side, so the shape scales with it).
pub(crate) fn check_points(box_rc: &RECT) -> [(i32, i32); 3] {
    let side = box_rc.right - box_rc.left;
    let at = |px: i32, py: i32| {
        (
            box_rc.left + side * px / 100,
            box_rc.top + (box_rc.bottom - box_rc.top) * py / 100,
        )
    };
    [at(24, 53), at(42, 70), at(76, 31)]
}

/// Which painter a button style gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Push,
    Check,
}

/// The painter for a `BUTTON` of window style `style`, or `None` for anything
/// this module leaves to the system (radio, group box, owner-drawn, push-like
/// toggle, icon and bitmap buttons).
pub(crate) fn kind_of(style: u32) -> Option<Kind> {
    if style & BS_PUSHLIKE != 0 {
        return None;
    }
    match style & BS_TYPE_MASK {
        0 | 1 => Some(Kind::Push),
        2 | 3 | 5 | 6 => Some(Kind::Check),
        _ => None,
    }
}

unsafe fn is_button(hwnd: windows::Win32::Foundation::HWND) -> bool {
    let mut name = [0u16; 8];
    let n = GetClassNameW(hwnd, &mut name) as usize;
    String::from_utf16_lossy(&name[..n.min(name.len())]).eq_ignore_ascii_case("Button")
}

unsafe fn window_text(hwnd: windows::Win32::Foundation::HWND) -> String {
    let mut buf = [0u16; 256];
    let n = GetWindowTextW(hwnd, &mut buf).max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())])
}

/// The answer to an `NM_CUSTOMDRAW` from a button child: paints it from
/// `palette` on the `face` brush and answers `CDRF_SKIPDEFAULT`; anything that
/// is not a push button or checkbox, or not the pre-paint stage, is left to the
/// system (`CDRF_DODEFAULT`).
///
/// # Safety
/// `lparam` must be the `NMCUSTOMDRAW` pointer of a `WM_NOTIFY`.
pub(crate) unsafe fn handle_custom_draw(
    lparam: LPARAM,
    dpi: u32,
    palette: &Palette,
    face: HBRUSH,
) -> isize {
    let nm = &*(lparam.0 as *const NMCUSTOMDRAW);
    if nm.hdr.code != NM_CUSTOMDRAW_CODE || nm.dwDrawStage.0 != CDDS_PREPAINT {
        return CDRF_DODEFAULT;
    }
    let hwnd = nm.hdr.hwndFrom;
    if !is_button(hwnd) {
        return CDRF_DODEFAULT;
    }
    let Some(kind) = kind_of(GetWindowLongW(hwnd, GWL_STYLE) as u32) else {
        return CDRF_DODEFAULT;
    };
    let item_state = nm.uItemState.0;
    let state = state_from_cdis(item_state);
    let ui = SendMessageW(hwnd, WM_QUERYUISTATE, None, None).0 as u32;
    let show_focus = item_state & CDIS_FOCUS != 0 && ui & UISF_HIDEFOCUS == 0;
    let prefix = if ui & UISF_HIDEACCEL != 0 {
        DT_HIDEPREFIX
    } else {
        Default::default()
    };
    let font = HFONT(SendMessageW(hwnd, WM_GETFONT, None, None).0 as *mut _);
    let text = window_text(hwnd);
    FillRect(nm.hdc, &nm.rc, face);
    match kind {
        Kind::Push => draw_push(nm, &text, font, state, show_focus, prefix, dpi, palette),
        Kind::Check => {
            let checked = SendMessageW(hwnd, BM_GETCHECK, None, None).0 == BST_CHECKED;
            draw_check(
                nm, &text, font, state, checked, show_focus, prefix, dpi, palette,
            );
        }
    }
    CDRF_SKIPDEFAULT
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_push(
    nm: &NMCUSTOMDRAW,
    text: &str,
    font: HFONT,
    state: State,
    show_focus: bool,
    prefix: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    dpi: u32,
    palette: &Palette,
) {
    use windows::Win32::UI::Controls::ODS_FLAGS;
    const ODS_FOCUS: u32 = 0x0010;
    const ODS_NOFOCUSRECT: u32 = 0x0200;
    let look = card_buttons::look(
        Role::Secondary,
        state,
        &palette.surface(),
        palette.accent,
        palette.high_contrast.as_ref(),
    );
    let item_state = if show_focus {
        ODS_FOCUS
    } else {
        ODS_NOFOCUSRECT
    };
    let dis = DRAWITEMSTRUCT {
        hDC: nm.hdc,
        rcItem: nm.rc,
        itemState: ODS_FLAGS(item_state),
        ..Default::default()
    };
    // The shared draw paints the shape and the focus ring; the label is drawn
    // here because it must honour the hidden accelerator underline.
    card_buttons::draw(
        &dis,
        "",
        font,
        &look,
        palette.tokens.surface,
        scale_for_dpi(CONTROL_RADIUS_DP, dpi),
        scale_for_dpi(1, dpi),
    );
    let old = SelectObject(nm.hdc, HGDIOBJ(font.0));
    SetBkMode(nm.hdc, TRANSPARENT);
    SetTextColor(nm.hdc, COLORREF(look.text));
    draw_label(
        nm.hdc,
        text,
        nm.rc,
        windows::Win32::Graphics::Gdi::DT_CENTER
            | DT_VCENTER
            | DT_SINGLELINE
            | DT_END_ELLIPSIS
            | prefix,
    );
    SelectObject(nm.hdc, old);
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_check(
    nm: &NMCUSTOMDRAW,
    text: &str,
    font: HFONT,
    state: State,
    checked: bool,
    show_focus: bool,
    prefix: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
    dpi: u32,
    palette: &Palette,
) {
    let hdc = nm.hdc;
    let (w, h) = (nm.rc.right - nm.rc.left, nm.rc.bottom - nm.rc.top);
    let look = check_look(state, checked, palette);
    let smooth = palette.high_contrast.is_none();
    let old = SelectObject(hdc, HGDIOBJ(font.0));
    let text_w = measure_label(hdc, text, prefix);
    let g = check_geometry(w, h, dpi, text_w);
    crate::ui::round_rect::paint(
        hdc,
        g.box_rc,
        scale_for_dpi(CONTROL_RADIUS_DP, dpi),
        Some(look.fill),
        Some((look.border, scale_for_dpi(1, dpi))),
        smooth,
    );
    if checked {
        let pts = check_points(&g.box_rc);
        let points: Vec<windows::Win32::Foundation::POINT> = pts
            .iter()
            .map(|&(x, y)| windows::Win32::Foundation::POINT { x, y })
            .collect();
        let pen = CreatePen(PS_SOLID, scale_for_dpi(2, dpi).max(2), COLORREF(look.glyph));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = MoveToEx(hdc, points[0].x, points[0].y, None);
        let _ = Polyline(hdc, &points);
        SelectObject(hdc, old_pen);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(look.label));
    draw_label(
        hdc,
        text,
        g.text_rc,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_END_ELLIPSIS | prefix,
    );
    SelectObject(hdc, old);
    if show_focus {
        crate::ui::round_rect::paint(
            hdc,
            g.focus_rc,
            scale_for_dpi(CONTROL_RADIUS_DP, dpi),
            None,
            Some((look.focus, scale_for_dpi(FOCUS_LINE_DP, dpi))),
            smooth,
        );
    }
}

/// Draws `text` (never an empty buffer, which `DrawTextW` crashes on).
unsafe fn draw_label(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    text: &str,
    rect: RECT,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    crate::ui::text::draw_text_line(hdc, text, rect, flags);
}

/// The width of `text` in the font selected into `hdc`, in px, honouring the
/// accelerator marker as the draw does.
unsafe fn measure_label(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    text: &str,
    prefix: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rc = RECT::default();
    DrawTextW(
        hdc,
        &mut wide,
        &mut rc,
        DT_CALCRECT | DT_SINGLELINE | prefix,
    );
    rc.right - rc.left
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{contrast_ratio, relative_luminance, DARK, HC_BLACK};

    fn dark() -> Palette {
        Palette::new(Theme::Dark, &HC_BLACK, 0x00D4_7800)
    }

    fn contrast(a: u32, b: u32) -> f64 {
        contrast_ratio(relative_luminance(a), relative_luminance(b))
    }

    #[test]
    fn the_item_state_reads_disabled_then_pressed_then_hot() {
        assert_eq!(state_from_cdis(0), State::Normal);
        assert_eq!(state_from_cdis(CDIS_HOT), State::Hover);
        assert_eq!(state_from_cdis(CDIS_HOT | CDIS_SELECTED), State::Pressed);
        assert_eq!(
            state_from_cdis(CDIS_HOT | CDIS_SELECTED | CDIS_DISABLED),
            State::Disabled
        );
        assert_eq!(state_from_cdis(CDIS_GRAYED), State::Disabled);
        assert_eq!(
            state_from_cdis(CDIS_FOCUS),
            State::Normal,
            "focus is a ring"
        );
    }

    #[test]
    fn an_unchecked_box_is_an_outline_a_checked_one_is_the_accent() {
        let p = dark();
        let off = check_look(State::Normal, false, &p);
        assert_eq!(off.fill, DARK.surface);
        assert_eq!(off.border, DARK.control_border);
        assert_eq!(off.label, DARK.text);
        let on = check_look(State::Normal, true, &p);
        assert_eq!(on.fill, 0x00D4_7800);
        assert_eq!(on.border, on.fill);
        assert!(
            contrast(on.glyph, on.fill) >= 4.5,
            "the tick reads on the fill"
        );
    }

    #[test]
    fn hover_pressed_and_disabled_each_show() {
        let p = dark();
        let fill = |s, checked| check_look(s, checked, &p).fill;
        // Unchecked: the outline fills with a tint under the pointer, more
        // while pressed; disabled keeps the face and greys the label instead.
        let rest = [State::Normal, State::Hover, State::Pressed].map(|s| fill(s, false));
        assert!(rest[0] != rest[1] && rest[1] != rest[2] && rest[0] != rest[2]);
        let disabled = check_look(State::Disabled, false, &p);
        assert_eq!(disabled.label, DARK.text_faint);
        assert_ne!(disabled.label, check_look(State::Normal, false, &p).label);
        // Checked: all four states differ.
        let on =
            [State::Normal, State::Hover, State::Pressed, State::Disabled].map(|s| fill(s, true));
        for i in 0..on.len() {
            for j in i + 1..on.len() {
                assert_ne!(on[i], on[j], "checked states {i} and {j}");
            }
        }
    }

    #[test]
    fn every_label_state_is_readable_on_the_face_in_dark() {
        let p = dark();
        for state in [State::Normal, State::Hover, State::Pressed] {
            let l = check_look(state, false, &p);
            assert!(contrast(l.label, DARK.surface) >= 4.5, "{state:?}");
        }
        let l = check_look(State::Normal, false, &p);
        assert!(contrast(l.border, DARK.surface) >= 3.0, "box edge 3:1");
        assert!(contrast(l.focus, DARK.surface) >= 3.0, "focus frame 3:1");
    }

    #[test]
    fn forced_high_contrast_uses_the_system_colours() {
        let p = Palette::new(Theme::HighContrast, &HC_BLACK, HC_BLACK.highlight);
        assert!(p.high_contrast.is_some());
        let off = check_look(State::Normal, false, &p);
        assert_eq!(off.label, HC_BLACK.window_text);
        assert_eq!(off.focus, HC_BLACK.window_text);
        let on = check_look(State::Normal, true, &p);
        assert_eq!(on.fill, HC_BLACK.highlight);
        assert_eq!(on.glyph, HC_BLACK.highlight_text);
        let dis = check_look(State::Disabled, false, &p);
        assert_eq!(dis.label, HC_BLACK.gray_text);
        assert!(dark().high_contrast.is_none());
    }

    #[test]
    fn the_geometry_scales_and_keeps_the_box_square_inside_the_row() {
        for dpi in [96u32, 120, 144, 192, 240] {
            let (w, h) = (scale_for_dpi(300, dpi), scale_for_dpi(22, dpi));
            let g = check_geometry(w, h, dpi, scale_for_dpi(100, dpi));
            let side = g.box_rc.right - g.box_rc.left;
            assert_eq!(side, scale_for_dpi(CHECK_BOX_DP, dpi), "{dpi} dpi");
            assert_eq!(g.box_rc.bottom - g.box_rc.top, side);
            assert!(g.box_rc.top >= 0 && g.box_rc.bottom <= h);
            assert!(
                (g.box_rc.top - (h - g.box_rc.bottom)).abs() <= 1,
                "centred ({dpi} dpi)"
            );
            assert!(g.text_rc.left > g.box_rc.right, "label clear of the box");
            assert!(g.focus_rc.right <= w && g.focus_rc.right > g.text_rc.left);
        }
        assert_eq!(check_geometry(96, 22, 96, 0).box_rc.right - 4, 16);
    }

    #[test]
    fn a_tiny_or_empty_control_never_gives_a_negative_box() {
        for (w, h) in [(0, 0), (5, 3), (-4, -4)] {
            let g = check_geometry(w, h, 96, 50);
            assert!(g.box_rc.right >= g.box_rc.left);
            assert!(g.text_rc.right >= g.text_rc.left);
        }
    }

    #[test]
    fn the_tick_lies_inside_the_box_and_goes_down_then_up() {
        let rc = RECT {
            left: 4,
            top: 3,
            right: 20,
            bottom: 19,
        };
        let [a, b, c] = check_points(&rc);
        for (x, y) in [a, b, c] {
            assert!(x > rc.left && x < rc.right && y > rc.top && y < rc.bottom);
        }
        assert!(a.0 < b.0 && b.0 < c.0, "left to right");
        assert!(
            b.1 > a.1 && b.1 > c.1,
            "the bottom of the tick is the middle"
        );
    }

    #[test]
    fn only_push_buttons_and_checkboxes_are_ours() {
        assert_eq!(kind_of(0), Some(Kind::Push));
        assert_eq!(kind_of(1), Some(Kind::Push), "default push button");
        assert_eq!(kind_of(2), Some(Kind::Check));
        assert_eq!(kind_of(3), Some(Kind::Check), "auto checkbox");
        assert_eq!(kind_of(4), None, "radio");
        assert_eq!(kind_of(7), None, "group box");
        assert_eq!(kind_of(0xB), None, "owner draw");
        assert_eq!(kind_of(3 | BS_PUSHLIKE), None, "a toggle drawn as a button");
        assert_eq!(kind_of(0x0003 | 0x0001_0000), Some(Kind::Check), "tab stop");
    }

    /// The paint code holds no colour of its own: every colour is a token or a
    /// system colour handed in (CRLF normalised, non-empty candidate set).
    #[test]
    fn the_paint_code_has_no_literal_colours() {
        let src = include_str!("settings_buttons.rs").replace('\r', "");
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
