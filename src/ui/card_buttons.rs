//! Owner-drawn push buttons for the card (#351), built once so the options
//! card (#307) can reuse them.
//!
//! The card is custom-painted with rounded corners and a themed palette, but
//! stock `BUTTON` controls ignore both. This module decides what an
//! owner-drawn button looks like ([`look`], pure and unit-tested) and paints
//! it into the `DC` a `WM_DRAWITEM` hands over ([`draw`]).
//!
//! Colors are GDI `COLORREF` values (`0x00BBGGRR`), the same as the card's
//! own palette.

use crate::ui::theme::{accent_fill, read_accent_shades, Theme};
use windows::Win32::Foundation::{COLORREF, RECT};
use windows::Win32::Graphics::Gdi::{
    CreateSolidBrush, DeleteObject, FillRect, GetSysColor, SelectObject, SetBkMode, SetTextColor,
    COLOR_BTNFACE, COLOR_BTNTEXT, COLOR_GRAYTEXT, COLOR_HIGHLIGHT, COLOR_HIGHLIGHTTEXT,
    DRAW_TEXT_FORMAT, DT_CENTER, DT_END_ELLIPSIS, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HFONT,
    HGDIOBJ, TRANSPARENT,
};
use windows::Win32::UI::Accessibility::{HCF_HIGHCONTRASTON, HIGHCONTRASTW};
use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
use windows::Win32::UI::WindowsAndMessaging::{
    SystemParametersInfoW, SPI_GETHIGHCONTRAST, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
};

/// `BS_OWNERDRAW` (winuser.h). The low four bits of a button's style hold its
/// type, so this is compared against `style & 0xF`.
pub(crate) const BS_OWNERDRAW: u32 = 0x0000_000B;
const BS_TYPE_MASK: u32 = 0x0000_000F;

const ODS_SELECTED: u32 = 0x0001;
const ODS_DISABLED: u32 = 0x0004;
const ODS_FOCUS: u32 = 0x0010;
const ODS_NOFOCUSRECT: u32 = 0x0200;

/// Whether a button's window style marks it owner-drawn.
pub(crate) fn is_owner_drawn(style: u32) -> bool {
    style & BS_TYPE_MASK == BS_OWNERDRAW
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    /// The action the user is deciding on: filled with the accent color.
    Primary,
    /// Everything else: an outline on the card's own background.
    Secondary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Normal,
    Hover,
    Pressed,
    Disabled,
}

/// The card colors a button sits on.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Surface {
    pub bg: u32,
    pub border: u32,
    pub text: u32,
    pub hint: u32,
}

/// The system colors Windows asks apps to use in high contrast mode.
#[derive(Debug, Clone, Copy)]
pub(crate) struct HighContrast {
    pub face: u32,
    pub text: u32,
    pub highlight: u32,
    pub highlight_text: u32,
    pub gray_text: u32,
}

/// What to paint for one button in one state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Look {
    pub fill: u32,
    pub border: u32,
    pub text: u32,
    /// The ring drawn inside the button while it has keyboard focus.
    pub focus: u32,
    /// Anti-aliased corners. Off in high contrast: a blended pixel is not a
    /// system colour.
    pub smooth: bool,
}

fn channels(c: u32) -> (f32, f32, f32) {
    (
        (c & 0xFF) as f32,
        ((c >> 8) & 0xFF) as f32,
        ((c >> 16) & 0xFF) as f32,
    )
}

fn pack(r: f32, g: f32, b: f32) -> u32 {
    (r.round().clamp(0.0, 255.0) as u32)
        | ((g.round().clamp(0.0, 255.0) as u32) << 8)
        | ((b.round().clamp(0.0, 255.0) as u32) << 16)
}

/// `from` moved `pct` percent of the way to `to`.
pub(crate) fn blend(from: u32, to: u32, pct: u32) -> u32 {
    let t = pct.min(100) as f32 / 100.0;
    let (fr, fg, fb) = channels(from);
    let (tr, tg, tb) = channels(to);
    pack(fr + (tr - fr) * t, fg + (tg - fg) * t, fb + (tb - fb) * t)
}

/// Black or white text for `fill`, whichever has the higher WCAG contrast
/// ratio against it.
pub(crate) fn readable_on(fill: u32) -> u32 {
    use crate::ui::theme::{contrast_ratio, relative_luminance};
    let l = relative_luminance(fill);
    let against_black = contrast_ratio(l, 0.0);
    let against_white = contrast_ratio(l, 1.0);
    if against_black >= against_white {
        0x0000_0000
    } else {
        0x00FF_FFFF
    }
}

/// Black on a light surface, white on a dark one: the colour that raises a
/// fill's contrast against `surface`.
fn away_from(surface: u32) -> u32 {
    if crate::ui::theme::relative_luminance(surface) > 0.5 {
        0x0000_0000
    } else {
        0x00FF_FFFF
    }
}

/// How an `itemState` (plus whether the mouse is over the button) reads.
pub(crate) fn state_from(item_state: u32, hovered: bool) -> State {
    if item_state & ODS_DISABLED != 0 {
        State::Disabled
    } else if item_state & ODS_SELECTED != 0 {
        State::Pressed
    } else if hovered {
        State::Hover
    } else {
        State::Normal
    }
}

/// How far a secondary button's fill moves toward its text colour when hot and
/// when pressed, in percent. Settings' combo arrow button shares them.
pub(crate) const SECONDARY_HOVER_PCT: u32 = 8;
pub(crate) const SECONDARY_PRESSED_PCT: u32 = 16;

/// The pure decision: colors for a role in a state on a surface, honoring
/// the system accent, and the system colors outright in high contrast mode.
pub(crate) fn look(
    role: Role,
    state: State,
    surface: &Surface,
    accent: u32,
    high_contrast: Option<&HighContrast>,
) -> Look {
    if let Some(hc) = high_contrast {
        return high_contrast_look(role, state, hc);
    }
    match role {
        Role::Primary => {
            let fill = match state {
                State::Normal => accent,
                // Hover and pressed move away from the card colour, so they
                // keep at least the resting fill's contrast against it (#857).
                State::Hover => blend(accent, away_from(surface.bg), 10),
                State::Pressed => blend(accent, away_from(surface.bg), 22),
                State::Disabled => blend(surface.bg, surface.hint, 30),
            };
            // The disabled label is about 3.2:1 on its fill: WCAG 1.4.3 exempts
            // text on inactive controls from the 4.5:1 bar.
            let text = if state == State::Disabled {
                surface.hint
            } else {
                readable_on(fill)
            };
            Look {
                fill,
                border: fill,
                text,
                focus: readable_on(fill),
                smooth: true,
            }
        }
        Role::Secondary => {
            let fill = match state {
                State::Normal | State::Disabled => surface.bg,
                State::Hover => blend(surface.bg, surface.text, SECONDARY_HOVER_PCT),
                State::Pressed => blend(surface.bg, surface.text, SECONDARY_PRESSED_PCT),
            };
            let text = if state == State::Disabled {
                surface.hint
            } else {
                surface.text
            };
            Look {
                fill,
                border: surface.border,
                text,
                focus: surface.text,
                smooth: true,
            }
        }
    }
}

fn high_contrast_look(role: Role, state: State, hc: &HighContrast) -> Look {
    let highlighted = match (role, state) {
        (_, State::Disabled) => false,
        (Role::Primary, _) => true,
        (Role::Secondary, State::Hover | State::Pressed) => true,
        (Role::Secondary, State::Normal) => false,
    };
    let (fill, text) = if highlighted {
        (hc.highlight, hc.highlight_text)
    } else {
        (hc.face, hc.text)
    };
    Look {
        fill,
        border: if state == State::Disabled {
            hc.gray_text
        } else {
            text
        },
        text: if state == State::Disabled {
            hc.gray_text
        } else {
            text
        },
        focus: text,
        smooth: false,
    }
}

/// The fill of the primary button for `theme` on a card of colour
/// `surface_bg`: the user's accent shade for that theme, kept at 3:1 against
/// the card (see `theme::accent_fill`). Not `DwmGetColorizationColor`: that is
/// the window colourisation (often graphite), not the accent buttons use.
pub(crate) fn system_accent(theme: Theme, surface_bg: u32) -> u32 {
    accent_fill(theme, read_accent_shades(), surface_bg)
}

/// The system colors when high contrast mode is on, else `None`.
pub(crate) fn system_high_contrast() -> Option<HighContrast> {
    let mut hc = HIGHCONTRASTW {
        cbSize: std::mem::size_of::<HIGHCONTRASTW>() as u32,
        ..Default::default()
    };
    let ok = unsafe {
        SystemParametersInfoW(
            SPI_GETHIGHCONTRAST,
            hc.cbSize,
            Some(&mut hc as *mut _ as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    };
    if ok.is_err() || hc.dwFlags.0 & HCF_HIGHCONTRASTON.0 == 0 {
        return None;
    }
    unsafe {
        Some(HighContrast {
            face: GetSysColor(COLOR_BTNFACE),
            text: GetSysColor(COLOR_BTNTEXT),
            highlight: GetSysColor(COLOR_HIGHLIGHT),
            highlight_text: GetSysColor(COLOR_HIGHLIGHTTEXT),
            gray_text: GetSysColor(COLOR_GRAYTEXT),
        })
    }
}

/// Paints one owner-drawn button into the `DC` of a `WM_DRAWITEM`.
/// `corner` and `ring` are in pixels (the caller scales them for DPI). The
/// corners are anti-aliased unless `look.smooth` is false (high contrast, so
/// every pixel stays a system colour).
///
/// # Safety
/// `dis` must be the `DRAWITEMSTRUCT` Windows passed with the message, so
/// `hDC` is a live device context.
pub(crate) unsafe fn draw(
    dis: &DRAWITEMSTRUCT,
    text: &str,
    font: HFONT,
    look: &Look,
    surface_bg: u32,
    corner: i32,
    ring: i32,
) {
    let hdc = dis.hDC;
    let rc: RECT = dis.rcItem;

    // The card's own background first, so the rounded corners blend in.
    let bg = CreateSolidBrush(COLORREF(surface_bg));
    FillRect(hdc, &rc, bg);
    let _ = DeleteObject(HGDIOBJ(bg.0));

    // Smoothed corners: `RoundRect` has no anti-aliasing (#989).
    crate::ui::round_rect::paint(
        hdc,
        rc,
        corner,
        Some(look.fill),
        Some((look.border, 1)),
        look.smooth,
    );

    let old_font = SelectObject(hdc, HGDIOBJ(font.0));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(look.text));
    // Through the shared guard: `DrawTextW` with an empty buffer crashes the
    // process (see `ui::text`), and the options card (#307) draws its own
    // label and passes no text here.
    crate::ui::text::draw_text_line(hdc, text, rc, label_flags());

    if dis.itemState.0 & ODS_FOCUS != 0 && dis.itemState.0 & ODS_NOFOCUSRECT == 0 {
        let inset = ring + 2;
        let ring_rect = RECT {
            left: rc.left + inset,
            top: rc.top + inset,
            right: rc.right - inset,
            bottom: rc.bottom - inset,
        };
        crate::ui::round_rect::paint(
            hdc,
            ring_rect,
            corner.max(2) / 2,
            None,
            Some((look.focus, focus_ring_thickness(ring))),
            look.smooth,
        );
    }

    SelectObject(hdc, old_font);
}

/// The focus ring is 2 px at 100% like Windows 11's, twice the border width at
/// any scale (#817). Never thinner than the border, so a zero never vanishes.
fn focus_ring_thickness(ring: i32) -> i32 {
    (ring * 2).max(2)
}

/// Maps a `DRAWITEMSTRUCT` to the state to draw, given the hover flag.
/// How a button label is drawn: centred on one line, with an ellipsis when it
/// is wider than the button (a long label used to clip on both sides, #782).
fn label_flags() -> DRAW_TEXT_FORMAT {
    DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS
}

pub(crate) fn state_of(dis: &DRAWITEMSTRUCT, hovered: bool) -> State {
    state_from(dis.itemState.0, hovered)
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_focus_ring_is_twice_the_border_width() {
        assert_eq!(super::focus_ring_thickness(1), 2);
        assert_eq!(super::focus_ring_thickness(2), 4);
        assert_eq!(super::focus_ring_thickness(0), 2);
    }

    use super::*;

    #[test]
    fn label_flags_ellipsize_a_long_label() {
        assert!(label_flags().0 & DT_END_ELLIPSIS.0 != 0);
        assert!(label_flags().0 & DT_SINGLELINE.0 != 0);
    }

    const BG: u32 = 0x00FA_FAFA;
    const SURFACE: Surface = Surface {
        bg: BG,
        border: 0x00D6_D6D6,
        text: 0x0017_1717,
        hint: 0x007A_7A7A,
    };
    const ACCENT: u32 = 0x00D4_7800;

    #[test]
    fn primary_is_filled_with_the_accent() {
        let l = look(Role::Primary, State::Normal, &SURFACE, ACCENT, None);
        assert_eq!(l.fill, ACCENT);
        assert_eq!(l.border, ACCENT);
    }

    #[test]
    fn secondary_is_an_outline_on_the_card_background() {
        let l = look(Role::Secondary, State::Normal, &SURFACE, ACCENT, None);
        assert_eq!(l.fill, BG);
        assert_eq!(l.border, SURFACE.border);
        assert_eq!(l.text, SURFACE.text);
    }

    #[test]
    fn hover_and_pressed_change_the_fill() {
        for role in [Role::Primary, Role::Secondary] {
            let normal = look(role, State::Normal, &SURFACE, ACCENT, None).fill;
            let hover = look(role, State::Hover, &SURFACE, ACCENT, None).fill;
            let pressed = look(role, State::Pressed, &SURFACE, ACCENT, None).fill;
            assert_ne!(normal, hover, "{role:?} hover must show");
            assert_ne!(hover, pressed, "{role:?} pressed must differ from hover");
            assert_ne!(normal, pressed);
        }
    }

    #[test]
    fn every_primary_state_keeps_3_to_1_against_the_card() {
        use crate::ui::theme::{
            accent_fill, contrast_ratio, relative_luminance, AccentShades, Theme, DARK, LIGHT,
        };
        let accents = [
            rgb(0x00, 0x78, 0xD4),
            rgb(0xFF, 0xB9, 0x00),
            rgb(0x3A, 0x3A, 0x3A),
            rgb(0xFF, 0xFF, 0xFF),
            rgb(0x00, 0x00, 0x00),
        ];
        let mut checked = 0;
        for (theme, bg) in [(Theme::Light, LIGHT.surface), (Theme::Dark, DARK.surface)] {
            let surface = Surface { bg, ..SURFACE };
            for a in accents {
                let rest = accent_fill(
                    theme,
                    AccentShades {
                        light2: a,
                        dark1: a,
                    },
                    bg,
                );
                for state in [State::Normal, State::Hover, State::Pressed] {
                    let fill = look(Role::Primary, state, &surface, rest, None).fill;
                    let c = contrast_ratio(relative_luminance(fill), relative_luminance(bg));
                    assert!(c >= 3.0, "{theme:?} {a:#08x} {state:?}: {c:.2}:1");
                    checked += 1;
                }
            }
        }
        assert_eq!(checked, 30);
    }

    #[test]
    fn disabled_text_is_the_hint_color() {
        for role in [Role::Primary, Role::Secondary] {
            let l = look(role, State::Disabled, &SURFACE, ACCENT, None);
            assert_eq!(l.text, SURFACE.hint);
        }
    }

    #[test]
    fn text_on_the_accent_is_readable_for_light_and_dark_accents() {
        assert_eq!(readable_on(0x00FF_FFFF), 0x0000_0000, "black on white");
        assert_eq!(readable_on(0x0000_0000), 0x00FF_FFFF, "white on black");
        // A pale yellow accent needs dark text; a deep blue needs white.
        assert_eq!(readable_on(rgb(255, 240, 120)), 0x0000_0000);
        assert_eq!(readable_on(rgb(0, 40, 120)), 0x00FF_FFFF);
        // Mid accents (the default blue, an orange) get whichever of black
        // and white has the higher contrast, never the weaker one.
        use crate::ui::theme::{contrast_ratio, relative_luminance};
        for accent in [ACCENT, rgb(0xF7, 0x63, 0x0C)] {
            let l = relative_luminance(accent);
            let chosen = relative_luminance(readable_on(accent));
            let other = relative_luminance(0x00FF_FFFF - readable_on(accent));
            assert!(contrast_ratio(l, chosen) >= contrast_ratio(l, other));
        }
        assert_eq!(readable_on(rgb(0xF7, 0x63, 0x0C)), 0x0000_0000);
    }

    fn rgb(r: u32, g: u32, b: u32) -> u32 {
        r | (g << 8) | (b << 16)
    }

    #[test]
    fn state_follows_the_draw_item_flags() {
        assert_eq!(state_from(0, false), State::Normal);
        assert_eq!(state_from(0, true), State::Hover);
        assert_eq!(state_from(ODS_SELECTED, true), State::Pressed);
        assert_eq!(
            state_from(ODS_DISABLED | ODS_SELECTED, true),
            State::Disabled
        );
    }

    #[test]
    fn high_contrast_uses_the_system_colors_and_ignores_the_accent() {
        let hc = HighContrast {
            face: 1,
            text: 2,
            highlight: 3,
            highlight_text: 4,
            gray_text: 5,
        };
        let primary = look(Role::Primary, State::Normal, &SURFACE, ACCENT, Some(&hc));
        assert_eq!((primary.fill, primary.text), (3, 4));
        let secondary = look(Role::Secondary, State::Normal, &SURFACE, ACCENT, Some(&hc));
        assert_eq!(
            (secondary.fill, secondary.text, secondary.border),
            (1, 2, 2)
        );
        let hover = look(Role::Secondary, State::Hover, &SURFACE, ACCENT, Some(&hc));
        assert_eq!((hover.fill, hover.text), (3, 4));
        let off = look(Role::Primary, State::Disabled, &SURFACE, ACCENT, Some(&hc));
        assert_eq!((off.text, off.border), (5, 5));
        assert!(!primary.smooth, "high contrast takes plain corners");
        let normal = look(Role::Primary, State::Normal, &SURFACE, ACCENT, None);
        assert!(normal.smooth);
    }

    /// #989: in high contrast the corner pixels are exactly the card colour,
    /// the fill or the border, never a blend that is no system colour.
    #[test]
    fn high_contrast_corners_hold_only_system_colours() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
        };
        let hc = HighContrast {
            face: 0x0000_0000,
            text: 0x00FF_FFFF,
            highlight: 0x00FF_FF00,
            highlight_text: 0x0000_0000,
            gray_text: 0x0080_8080,
        };
        let rc = RECT {
            left: 0,
            top: 0,
            right: 60,
            bottom: 24,
        };
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, 60, 24);
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let dis = DRAWITEMSTRUCT {
                hDC: dc,
                rcItem: rc,
                ..Default::default()
            };
            let look = look(Role::Primary, State::Normal, &SURFACE, ACCENT, Some(&hc));
            draw(&dis, "", HFONT(std::ptr::null_mut()), &look, BG, 8, 1);
            for y in 0..8 {
                for x in 0..8 {
                    let p = GetPixel(dc, x, y).0;
                    assert!(
                        p == BG || p == look.fill || p == look.border,
                        "({x},{y}) = {p:06X}"
                    );
                }
            }
            assert_eq!(GetPixel(dc, 0, 0).0, BG, "the very corner is the card");
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
        }
    }

    /// A button drawn with no text must not crash: `DrawTextW` with an empty
    /// buffer is an access violation (see `ui::text`), and the options card
    /// (#307) paints its own label over a text-less button.
    #[test]
    fn a_button_with_no_text_draws_without_crashing() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, ReleaseDC,
        };
        let rc = RECT {
            left: 0,
            top: 0,
            right: 60,
            bottom: 24,
        };
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, 60, 24);
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let dis = DRAWITEMSTRUCT {
                hDC: dc,
                rcItem: rc,
                ..Default::default()
            };
            let look = look(Role::Secondary, State::Normal, &SURFACE, ACCENT, None);
            draw(&dis, "", HFONT(std::ptr::null_mut()), &look, BG, 4, 1);
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
        }
    }

    #[test]
    fn owner_drawn_style_is_recognised_by_its_type_bits() {
        assert!(is_owner_drawn(0x5001_000B));
        assert!(!is_owner_drawn(0x5001_0001));
        assert!(!is_owner_drawn(0x5001_0000));
    }
}
