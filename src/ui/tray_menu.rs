//! How the tray menu looks (#825): owner-drawn rows, so the menu follows the
//! app's dark mode, shows the chosen option, right-aligns hints and has
//! working letter keys.
//!
//! A stock `TrackPopupMenu` menu stays light on a dark desktop unless the app
//! calls the undocumented `uxtheme` ordinals, which this project rules out.
//! The documented route is `MF_OWNERDRAW`: the menu still owns layout,
//! keyboard handling and submenus, and answers `WM_MEASUREITEM` and
//! `WM_DRAWITEM` through the tray's owner window. Colours, sizes and the type
//! ramp come from `theme.rs`.
//!
//! Each row carries an index into a thread-local table of [`ItemRecord`]s in
//! its item data (the label, hint and icon glyph), because an owner-drawn row
//! has no text of its own. Everything here runs on the UI thread, which is
//! where the menu is built and tracked.
//!
//! The pure parts (mnemonic assignment, sizes) are unit-tested; the painting
//! is checked by the gallery's `tray-menu` pictures.

use std::cell::{Cell, RefCell};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW,
    EnumFontFamiliesExW, FillRect, FillRgn, GetDC, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, DEFAULT_CHARSET, DT_CALCRECT, DT_END_ELLIPSIS, DT_HIDEPREFIX, DT_LEFT,
    DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_VCENTER, HBRUSH, HDC, HFONT, LOGFONTW, TEXTMETRICW,
    TRANSPARENT,
};
use windows::Win32::UI::Controls::{
    DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_GRAYED, ODS_NOACCEL, ODS_SELECTED,
    ODT_MENU,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    GetMenuItemCount, GetMenuItemInfoW, SetMenuInfo, MENUINFO, MENUITEMINFOW, MFS_HILITE,
    MIIM_DATA, MIIM_STATE, MIM_APPLYTOSUBMENUS, MIM_BACKGROUND, WM_DRAWITEM, WM_MEASUREITEM,
    WM_MENUCHAR,
};

use crate::ui::theme::{
    self, type_height_px, SysColors, Theme, Tokens, CONTROL_RADIUS_DP, FALLBACK_FONT_FACE,
    MENU_CHEVRON_AREA_DP, MENU_EDGE_PAD_DP, MENU_GUTTER_DP, MENU_HINT_GAP_DP, MENU_ICON_PX,
    MENU_ITEM_HEIGHT_DP, MENU_MAX_WIDTH_DP, MENU_MIN_WIDTH_DP, MENU_PILL_INSET_Y_DP,
    MENU_RIGHT_PAD_DP, MENU_SEPARATOR_HEIGHT_DP, PREFERRED_FONT_FACE, TYPE_BODY_PX,
};

/// What a row is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Item,
    Submenu,
    Separator,
}

/// What the menu needs to paint one row.
#[derive(Debug, Clone)]
pub(crate) struct ItemRecord {
    /// The label, with `&` before its mnemonic letter and `&&` for a real
    /// ampersand.
    pub label: String,
    /// Right-aligned quiet text: a shortcut, or the current state of a
    /// submenu ("Local" beside "Mode").
    pub hint: String,
    /// A Segoe Fluent Icons code point for the gutter.
    pub glyph: Option<char>,
    pub kind: Kind,
    /// Extra space above this row (it is the first of its menu).
    pub pad_top: bool,
    /// Extra space below this row (it is the last of its menu).
    pub pad_bottom: bool,
    /// The row's accessible name, NUL-terminated UTF-16, set by
    /// [`add_record`]. The menu keeps a pointer to it, so it must not change
    /// while the menu lives.
    name: Vec<u16>,
}

impl ItemRecord {
    pub(crate) fn new(label: &str, kind: Kind) -> Self {
        Self {
            label: label.to_string(),
            hint: String::new(),
            glyph: None,
            kind,
            pad_top: false,
            pad_bottom: false,
            name: Vec::new(),
        }
    }
}

thread_local! {
    /// The rows of the menus built on this thread. Indexed by item data.
    static RECORDS: RefCell<Vec<ItemRecord>> = const { RefCell::new(Vec::new()) };
    /// What the menu being shown is drawn with. `None` outside a track.
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
    static TRACK_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

/// Forgets every row. Called when a new menu starts to be built; the old menu
/// is gone by then.
pub(crate) fn begin_build() {
    RECORDS.with(|r| r.borrow_mut().clear());
}

/// Stores a row and returns the item data that finds it again (never 0, so
/// a menu item that is not ours cannot be mistaken for one), and a pointer to
/// the row's accessible name ("Mode, Local": the label without its `&` marks,
/// then the hint; a command's hint is its shortcut and follows a tab). The name lives as long as the table does, that is until
/// the next menu is built.
pub(crate) fn add_record(mut record: ItemRecord) -> (usize, *const u16) {
    let mut name = strip_mnemonics(&record.label);
    if !record.hint.is_empty() {
        // After a tab, Windows reads text as the item's shortcut key, which
        // is what the hint of a command is. The hint of a submenu is its
        // current value, which reads as part of the name.
        name.push_str(if record.kind == Kind::Submenu {
            ", "
        } else {
            "\t"
        });
        name.push_str(&record.hint);
    }
    record.name = name.encode_utf16().chain(std::iter::once(0)).collect();
    RECORDS.with(|r| {
        let mut r = r.borrow_mut();
        r.push(record);
        let ptr = r.last().map_or(std::ptr::null(), |rec| rec.name.as_ptr());
        (r.len(), ptr)
    })
}

/// A copy of the row an item's data names, if any.
pub(crate) fn record(data: usize) -> Option<ItemRecord> {
    RECORDS.with(|r| r.borrow().get(data.wrapping_sub(1)).cloned())
}

/// The label of the row an item's data names, without its `&` marks.
#[cfg(test)]
pub(crate) fn plain_label(data: usize) -> Option<String> {
    record(data).map(|r| strip_mnemonics(&r.label))
}

fn with_record_mut(data: usize, f: impl FnOnce(&mut ItemRecord)) {
    RECORDS.with(|r| {
        if let Some(rec) = r.borrow_mut().get_mut(data.wrapping_sub(1)) {
            f(rec);
        }
    });
}

// ---------------------------------------------------------------------------
// Mnemonics (pure)
// ---------------------------------------------------------------------------

/// `label` without mnemonic marks: `&x` becomes `x`, `&&` becomes `&`.
pub(crate) fn strip_mnemonics(label: &str) -> String {
    let mut out = String::new();
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.peek() {
                Some('&') => {
                    out.push('&');
                    chars.next();
                }
                Some(_) => {}
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// The mnemonic letter of `label`, lower-cased, if it has one.
pub(crate) fn mnemonic_of(label: &str) -> Option<char> {
    let mut chars = label.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            match chars.peek() {
                Some('&') => {
                    chars.next();
                }
                Some(m) => return m.to_lowercase().next(),
                None => return None,
            }
        }
    }
    None
}

/// Gives every label in one menu a mnemonic that no other label in it has.
/// A label that already carries one keeps it; the rest take the first free
/// letter or digit, preferring the first letter of a word. A label with no
/// free character gets none. Only ASCII letters and digits are used, so the
/// key a user types is the key they see underlined.
pub(crate) fn assign_mnemonics(labels: &[String]) -> Vec<String> {
    let mut used: Vec<char> = labels.iter().filter_map(|l| mnemonic_of(l)).collect();
    labels
        .iter()
        .map(|label| {
            if mnemonic_of(label).is_some() {
                return label.clone();
            }
            // Candidate positions (char index), word starts first.
            let chars: Vec<char> = label.chars().collect();
            let mut word_starts = Vec::new();
            let mut anywhere = Vec::new();
            let mut at_start = true;
            let mut i = 0;
            while i < chars.len() {
                let c = chars[i];
                if c == '&' && chars.get(i + 1) == Some(&'&') {
                    i += 2;
                    at_start = false;
                    continue;
                }
                if c.is_ascii_alphanumeric() {
                    if at_start {
                        word_starts.push(i);
                    }
                    anywhere.push(i);
                }
                at_start = !c.is_ascii_alphanumeric();
                i += 1;
            }
            for pos in word_starts.into_iter().chain(anywhere) {
                let lower = chars[pos].to_ascii_lowercase();
                if !used.contains(&lower) {
                    used.push(lower);
                    let mut out: String = chars[..pos].iter().collect();
                    out.push('&');
                    out.extend(chars[pos..].iter());
                    return out;
                }
            }
            label.clone()
        })
        .collect()
}

/// What a typed letter does in one menu (`WM_MENUCHAR`'s answer).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MenuChar {
    /// No row has this mnemonic: the menu beeps.
    Ignore,
    /// Run (or open) this row.
    Execute(usize),
    /// Several rows share the letter: move to this one, so another press
    /// moves to the next.
    Select(usize),
}

/// Resolves a typed character against the mnemonics of one menu's rows,
/// `selected` being the row that is highlighted now.
pub(crate) fn resolve_mnemonic(
    mnemonics: &[Option<char>],
    typed: char,
    selected: Option<usize>,
) -> MenuChar {
    let typed = typed.to_lowercase().next().unwrap_or(typed);
    let hits: Vec<usize> = mnemonics
        .iter()
        .enumerate()
        .filter(|(_, m)| **m == Some(typed))
        .map(|(i, _)| i)
        .collect();
    match hits.as_slice() {
        [] => MenuChar::Ignore,
        [only] => MenuChar::Execute(*only),
        many => {
            let next = selected
                .and_then(|s| many.iter().find(|&&i| i > s))
                .copied()
                .unwrap_or(many[0]);
            MenuChar::Select(next)
        }
    }
}

// ---------------------------------------------------------------------------
// Sizes (pure)
// ---------------------------------------------------------------------------

/// `dp` device-independent pixels at `dpi`, rounded.
pub(crate) fn px(dp: i32, dpi: u32) -> i32 {
    (dp * dpi as i32 + 48) / 96
}

/// The width of a row, from the measured widths of its label and hint, in px.
pub(crate) fn row_width(label_w: i32, hint_w: i32, submenu: bool, dpi: u32) -> i32 {
    let hint = if hint_w > 0 {
        px(MENU_HINT_GAP_DP, dpi) + hint_w
    } else {
        0
    };
    let right = if submenu {
        px(MENU_CHEVRON_AREA_DP, dpi)
    } else {
        px(MENU_RIGHT_PAD_DP, dpi)
    };
    let w = px(MENU_EDGE_PAD_DP, dpi) * 2 + px(MENU_GUTTER_DP, dpi) + label_w + hint + right;
    w.clamp(px(MENU_MIN_WIDTH_DP, dpi), px(MENU_MAX_WIDTH_DP, dpi))
}

/// The height of a row in px.
pub(crate) fn row_height(rec: &ItemRecord, dpi: u32) -> i32 {
    let base = match rec.kind {
        Kind::Separator => px(MENU_SEPARATOR_HEIGHT_DP, dpi),
        Kind::Item | Kind::Submenu => px(MENU_ITEM_HEIGHT_DP, dpi),
    };
    let pad = px(MENU_EDGE_PAD_DP, dpi);
    base + if rec.pad_top { pad } else { 0 } + if rec.pad_bottom { pad } else { 0 }
}

// ---------------------------------------------------------------------------
// Drawing context
// ---------------------------------------------------------------------------

struct Ctx {
    theme: Theme,
    tokens: Tokens,
    sys: SysColors,
    dpi: u32,
    font: HFONT,
    icon_font: HFONT,
    /// A Segoe Fluent Icons or Segoe MDL2 Assets face is installed.
    has_icon_font: bool,
    bg: HBRUSH,
}

impl Drop for Ctx {
    fn drop(&mut self) {
        unsafe {
            let _ = DeleteObject(self.font.into());
            let _ = DeleteObject(self.icon_font.into());
            let _ = DeleteObject(self.bg.into());
        }
    }
}

/// Keeps the drawing context alive while a menu is on screen. Dropping it
/// frees the fonts and the background brush.
pub(crate) struct TrackScope(());

impl Drop for TrackScope {
    fn drop(&mut self) {
        CTX.with(|c| *c.borrow_mut() = None);
        TRACK_ACTIVE.with(|c| c.set(false));
    }
}

/// The effective DPI of the monitor `pt` is on, 96 when it cannot be read.
pub(crate) fn dpi_at(pt: POINT) -> u32 {
    use windows::Win32::Graphics::Gdi::{MonitorFromPoint, MONITOR_DEFAULTTONEAREST};
    let mon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let (mut x, mut y) = (0u32, 0u32);
    match unsafe { GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x > 0 => x,
        _ => 96,
    }
}

fn face_installed(face: &str) -> bool {
    unsafe extern "system" fn found(
        _lf: *const LOGFONTW,
        _tm: *const TEXTMETRICW,
        _font_type: u32,
        lparam: LPARAM,
    ) -> i32 {
        *(lparam.0 as *mut bool) = true;
        0
    }
    let mut lf = LOGFONTW {
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    set_face(&mut lf, face);
    let mut hit = false;
    unsafe {
        let hdc = GetDC(None);
        EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(found),
            LPARAM(&mut hit as *mut bool as isize),
            0,
        );
        ReleaseDC(None, hdc);
    }
    hit
}

fn set_face(lf: &mut LOGFONTW, face: &str) {
    lf.lfFaceName = [0; 32];
    for (i, u) in face.encode_utf16().take(31).enumerate() {
        lf.lfFaceName[i] = u;
    }
}

/// Which face the glyph column uses, if any: Windows 11's Segoe Fluent Icons,
/// else Windows 10's Segoe MDL2 Assets.
fn icon_face() -> Option<&'static str> {
    use std::sync::OnceLock;
    static FACE: OnceLock<Option<&'static str>> = OnceLock::new();
    *FACE.get_or_init(|| {
        ["Segoe Fluent Icons", "Segoe MDL2 Assets"]
            .into_iter()
            .find(|f| face_installed(f))
    })
}

fn text_face() -> &'static str {
    use std::sync::OnceLock;
    static FACE: OnceLock<&'static str> = OnceLock::new();
    FACE.get_or_init(|| {
        if face_installed(PREFERRED_FONT_FACE) {
            PREFERRED_FONT_FACE
        } else {
            FALLBACK_FONT_FACE
        }
    })
}

/// Starts drawing for the menu about to be shown: the theme, the monitor's
/// DPI, the fonts and the background brush. The menu's own background is set
/// to the surface colour by [`apply_background`].
pub(crate) fn begin_track(theme: Theme, dpi: u32, sys: SysColors) -> TrackScope {
    let tokens = Tokens::for_theme(theme, &sys);
    let mut lf = LOGFONTW {
        lfHeight: type_height_px(TYPE_BODY_PX, dpi, 1.0),
        lfWeight: 400,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    set_face(&mut lf, text_face());
    let font = unsafe { CreateFontIndirectW(&lf) };
    let mut ilf = LOGFONTW {
        lfHeight: type_height_px(MENU_ICON_PX, dpi, 1.0),
        lfWeight: 400,
        lfCharSet: DEFAULT_CHARSET,
        ..Default::default()
    };
    let icon_face = icon_face();
    set_face(&mut ilf, icon_face.unwrap_or(FALLBACK_FONT_FACE));
    let icon_font = unsafe { CreateFontIndirectW(&ilf) };
    let bg = unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(tokens.surface)) };
    CTX.with(|c| {
        *c.borrow_mut() = Some(Ctx {
            theme,
            tokens,
            sys,
            dpi,
            font,
            icon_font,
            has_icon_font: icon_face.is_some(),
            bg,
        });
    });
    TRACK_ACTIVE.with(|c| c.set(true));
    TrackScope(())
}

/// Paints `hmenu` and its submenus with the surface colour behind and around
/// the rows. A no-op outside a track.
pub(crate) fn apply_background(hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU) {
    let brush = CTX.with(|c| c.borrow().as_ref().map(|c| c.bg));
    let Some(brush) = brush else { return };
    let info = MENUINFO {
        cbSize: std::mem::size_of::<MENUINFO>() as u32,
        fMask: MIM_BACKGROUND | MIM_APPLYTOSUBMENUS,
        hbrBack: brush,
        ..Default::default()
    };
    let _ = unsafe { SetMenuInfo(hmenu, &info) };
}

// ---------------------------------------------------------------------------
// Finishing a built menu
// ---------------------------------------------------------------------------

/// Gives every menu level its mnemonics and marks the first and last row of
/// each so they get the padding Windows 11 menus have. Call once after a menu
/// is fully built.
pub(crate) fn finalize(hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU) {
    use windows::Win32::UI::WindowsAndMessaging::GetSubMenu;
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    if count <= 0 {
        return;
    }
    let mut rows: Vec<usize> = Vec::new();
    let mut selectable: Vec<usize> = Vec::new();
    for pos in 0..count as u32 {
        if let Some((data, disabled)) = item_data(hmenu, pos) {
            rows.push(data);
            // Separators and disabled rows have no letter to give: a letter
            // typed for a disabled row would do nothing.
            if !disabled && record(data).is_some_and(|r| r.kind != Kind::Separator) {
                selectable.push(data);
            }
        }
        let sub = unsafe { GetSubMenu(hmenu, pos as i32) };
        if !sub.0.is_null() {
            finalize(sub);
        }
    }
    let labels: Vec<String> = selectable
        .iter()
        .filter_map(|d| record(*d).map(|r| r.label))
        .collect();
    for (data, label) in selectable.iter().zip(assign_mnemonics(&labels)) {
        with_record_mut(*data, |r| r.label = label);
    }
    if let Some(first) = rows.first() {
        with_record_mut(*first, |r| r.pad_top = true);
    }
    if let Some(last) = rows.last() {
        with_record_mut(*last, |r| r.pad_bottom = true);
    }
}

/// The item data of the row at `pos` and whether it is disabled, if it has a
/// record.
fn item_data(
    hmenu: windows::Win32::UI::WindowsAndMessaging::HMENU,
    pos: u32,
) -> Option<(usize, bool)> {
    let mut info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_DATA | MIIM_STATE,
        ..Default::default()
    };
    unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut info) }.ok()?;
    let data = info.dwItemData;
    let disabled = info.fState.0 & windows::Win32::UI::WindowsAndMessaging::MFS_DISABLED.0 != 0;
    record(data).map(|_| (data, disabled))
}

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// Answers the menu messages that reach the tray's owner window. `None` means
/// the message is not for the tray menu and the caller carries on. Reads only
/// thread-local state, so it is safe to call before the app is borrowed.
pub(crate) fn handle_message(msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
    if !TRACK_ACTIVE.with(|c| c.get()) {
        return None;
    }
    match msg {
        WM_MEASUREITEM => {
            let mis = unsafe { (lparam.0 as *mut MEASUREITEMSTRUCT).as_mut() }?;
            if mis.CtlType != ODT_MENU {
                return None;
            }
            measure(mis);
            Some(LRESULT(1))
        }
        WM_DRAWITEM => {
            let dis = unsafe { (lparam.0 as *const DRAWITEMSTRUCT).as_ref() }?;
            if dis.CtlType != ODT_MENU {
                return None;
            }
            draw(dis);
            Some(LRESULT(1))
        }
        WM_MENUCHAR => Some(menu_char(wparam, lparam)),
        _ => None,
    }
}

fn measure(mis: &mut MEASUREITEMSTRUCT) {
    CTX.with(|c| {
        let c = c.borrow();
        let Some(ctx) = c.as_ref() else { return };
        let Some(rec) = record(mis.itemData) else {
            return;
        };
        mis.itemHeight = row_height(&rec, ctx.dpi).max(1) as u32;
        if rec.kind == Kind::Separator {
            mis.itemWidth = 1;
            return;
        }
        let (label_w, hint_w) = unsafe {
            let hdc = GetDC(None);
            let old = SelectObject(hdc, ctx.font.into());
            let l = text_width(hdc, &rec.label, false);
            let h = text_width(hdc, &rec.hint, true);
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            (l, h)
        };
        mis.itemWidth =
            row_width(label_w, hint_w, rec.kind == Kind::Submenu, ctx.dpi).max(1) as u32;
    });
}

unsafe fn text_width(hdc: HDC, text: &str, no_prefix: bool) -> i32 {
    if text.is_empty() {
        return 0;
    }
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    let mut rc = RECT::default();
    let flags = DT_CALCRECT
        | DT_SINGLELINE
        | DT_LEFT
        | if no_prefix {
            DT_NOPREFIX
        } else {
            Default::default()
        };
    DrawTextW(hdc, &mut wide, &mut rc, flags);
    rc.right - rc.left
}

fn draw(dis: &DRAWITEMSTRUCT) {
    CTX.with(|c| {
        let c = c.borrow();
        let Some(ctx) = c.as_ref() else { return };
        let Some(rec) = record(dis.itemData) else {
            return;
        };
        unsafe { paint_row(ctx, dis, &rec) };
    });
}

unsafe fn paint_row(ctx: &Ctx, dis: &DRAWITEMSTRUCT, rec: &ItemRecord) {
    use windows::Win32::Foundation::COLORREF;
    let hdc = dis.hDC;
    let dpi = ctx.dpi;
    let row = dis.rcItem;
    FillRect(hdc, &row, ctx.bg);

    let edge = px(MENU_EDGE_PAD_DP, dpi);
    // The strip the row itself occupies, without the menu's top and bottom padding.
    let top = row.top + if rec.pad_top { edge } else { 0 };
    let bottom = row.bottom - if rec.pad_bottom { edge } else { 0 };

    if rec.kind == Kind::Separator {
        let mid = (top + bottom) / 2;
        let line = RECT {
            left: row.left + edge,
            right: row.right - edge,
            top: mid,
            bottom: mid + 1.max(px(1, dpi)),
        };
        let brush = CreateSolidBrush(COLORREF(ctx.tokens.border));
        FillRect(hdc, &line, brush);
        let _ = DeleteObject(brush.into());
        return;
    }

    let disabled = dis.itemState.0 & (ODS_DISABLED.0 | ODS_GRAYED.0) != 0;
    let hot = dis.itemState.0 & ODS_SELECTED.0 != 0;
    let checked = dis.itemState.0 & windows::Win32::UI::Controls::ODS_CHECKED.0 != 0;
    let hide_prefix = dis.itemState.0 & ODS_NOACCEL.0 != 0;

    let pill = RECT {
        left: row.left + edge,
        right: row.right - edge,
        top: top + px(MENU_PILL_INSET_Y_DP, dpi),
        bottom: bottom - px(MENU_PILL_INSET_Y_DP, dpi),
    };
    if hot {
        let r = px(CONTROL_RADIUS_DP, dpi) * 2;
        let rgn = CreateRoundRectRgn(pill.left, pill.top, pill.right + 1, pill.bottom + 1, r, r);
        let fill = CreateSolidBrush(COLORREF(theme::menu_hover_fill(
            ctx.theme,
            &ctx.tokens,
            &ctx.sys,
        )));
        let _ = FillRgn(hdc, rgn, fill);
        let _ = DeleteObject(fill.into());
        let _ = DeleteObject(rgn.into());
    }

    let ink = theme::menu_text_color(ctx.theme, &ctx.tokens, &ctx.sys, hot, disabled);
    let quiet = match ctx.theme {
        Theme::HighContrast => ink,
        _ => {
            if disabled {
                ink
            } else {
                ctx.tokens.text_dim
            }
        }
    };
    SetBkMode(hdc, TRANSPARENT);

    // The gutter: the chosen-option dot, or the row's icon.
    let gutter = RECT {
        left: pill.left,
        right: pill.left + px(MENU_GUTTER_DP, dpi),
        top: pill.top,
        bottom: pill.bottom,
    };
    if checked {
        let old = SelectObject(hdc, ctx.font.into());
        SetTextColor(hdc, COLORREF(ink));
        draw_text(
            hdc,
            "\u{25CF}",
            gutter,
            DT_SINGLELINE | DT_VCENTER | windows::Win32::Graphics::Gdi::DT_CENTER | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    } else if let (Some(glyph), true) = (rec.glyph, ctx.has_icon_font) {
        let old = SelectObject(hdc, ctx.icon_font.into());
        SetTextColor(hdc, COLORREF(ink));
        draw_text(
            hdc,
            &glyph.to_string(),
            gutter,
            DT_SINGLELINE | DT_VCENTER | windows::Win32::Graphics::Gdi::DT_CENTER | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }

    // Right side: chevron, then the hint, then the label.
    let mut right = pill.right
        - if rec.kind == Kind::Submenu {
            px(MENU_CHEVRON_AREA_DP, dpi)
        } else {
            px(MENU_RIGHT_PAD_DP, dpi)
        };
    if rec.kind == Kind::Submenu {
        let area = RECT {
            left: pill.right - px(MENU_CHEVRON_AREA_DP, dpi),
            right: pill.right - px(MENU_RIGHT_PAD_DP, dpi) / 2,
            top: pill.top,
            bottom: pill.bottom,
        };
        let (font, glyph) = if ctx.has_icon_font {
            (ctx.icon_font, "\u{E76C}")
        } else {
            (ctx.font, ">")
        };
        let old = SelectObject(hdc, font.into());
        SetTextColor(hdc, COLORREF(ink));
        draw_text(
            hdc,
            glyph,
            area,
            DT_SINGLELINE | DT_VCENTER | windows::Win32::Graphics::Gdi::DT_CENTER | DT_NOPREFIX,
        );
        SelectObject(hdc, old);
    }
    let old = SelectObject(hdc, ctx.font.into());
    if !rec.hint.is_empty() {
        let hint_rc = RECT {
            left: pill.left + px(MENU_GUTTER_DP, dpi),
            right,
            top: pill.top,
            bottom: pill.bottom,
        };
        SetTextColor(hdc, COLORREF(quiet));
        draw_text(
            hdc,
            &rec.hint,
            hint_rc,
            DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX,
        );
        let hint_w = text_width(hdc, &rec.hint, true);
        right -= hint_w + px(MENU_HINT_GAP_DP, dpi);
    }
    let text_rc = RECT {
        left: pill.left + px(MENU_GUTTER_DP, dpi),
        right,
        top: pill.top,
        bottom: pill.bottom,
    };
    SetTextColor(hdc, COLORREF(ink));
    let mut flags = DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_END_ELLIPSIS;
    if hide_prefix {
        flags |= DT_HIDEPREFIX;
    }
    draw_text(hdc, &rec.label, text_rc, flags);
    SelectObject(hdc, old);
}

unsafe fn draw_text(
    hdc: HDC,
    text: &str,
    mut rc: RECT,
    flags: windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT,
) {
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    DrawTextW(hdc, &mut wide, &mut rc, flags);
}

/// `WM_MENUCHAR`: a letter was typed in an owner-drawn menu, which cannot
/// find its mnemonics itself.
fn menu_char(wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    use windows::Win32::UI::WindowsAndMessaging::HMENU;
    const MNC_IGNORE: isize = 0;
    const MNC_EXECUTE: isize = 2;
    const MNC_SELECT: isize = 3;
    let hmenu = HMENU(lparam.0 as *mut _);
    let typed = char::from_u32((wparam.0 & 0xFFFF) as u32).unwrap_or('\0');
    let count = unsafe { GetMenuItemCount(Some(hmenu)) };
    if count <= 0 {
        return LRESULT(MNC_IGNORE << 16);
    }
    let mut mnemonics = Vec::new();
    let mut selected = None;
    for pos in 0..count as u32 {
        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_DATA | MIIM_STATE,
            ..Default::default()
        };
        if unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut info) }.is_err() {
            mnemonics.push(None);
            continue;
        }
        if info.fState.0 & MFS_HILITE.0 != 0 {
            selected = Some(pos as usize);
        }
        // A disabled row cannot be run by its letter.
        let usable = info.fState.0 & (windows::Win32::UI::WindowsAndMessaging::MFS_DISABLED.0) == 0;
        mnemonics.push(
            record(info.dwItemData)
                .filter(|_| usable)
                .and_then(|r| mnemonic_of(&r.label)),
        );
    }
    match resolve_mnemonic(&mnemonics, typed, selected) {
        MenuChar::Ignore => LRESULT(MNC_IGNORE << 16),
        MenuChar::Execute(i) => LRESULT((MNC_EXECUTE << 16) | i as isize),
        MenuChar::Select(i) => LRESULT((MNC_SELECT << 16) | i as isize),
    }
}

#[allow(dead_code)]
fn _unused(_: PCWSTR, _: HWND) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn labels(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn marks_are_stripped_and_found() {
        assert_eq!(strip_mnemonics("&Settings..."), "Settings...");
        assert_eq!(strip_mnemonics("Tom && Jerry"), "Tom & Jerry");
        assert_eq!(strip_mnemonics("Ask &now"), "Ask now");
        assert_eq!(strip_mnemonics("Trailing &"), "Trailing ");
        assert_eq!(mnemonic_of("&Settings"), Some('s'));
        assert_eq!(mnemonic_of("Ask &Now"), Some('n'));
        assert_eq!(mnemonic_of("Tom && Jerry"), None);
        assert_eq!(mnemonic_of("Tom && &Jerry"), Some('j'));
        assert_eq!(mnemonic_of("No mark"), None);
    }

    #[test]
    fn every_label_in_a_menu_gets_a_different_letter() {
        let out = assign_mnemonics(&labels(&["Cloud", "Local", "Auto", "Offline"]));
        assert_eq!(out, ["&Cloud", "&Local", "&Auto", "&Offline"]);
        let letters: Vec<char> = out.iter().filter_map(|l| mnemonic_of(l)).collect();
        assert_eq!(letters.len(), 4);
    }

    #[test]
    fn a_taken_first_letter_falls_back_to_the_next_word_then_any_letter() {
        let out = assign_mnemonics(&labels(&["Ask now", "Actions", "Copy last answer"]));
        // "Ask now": A. "Actions": no free word start, so the first free
        // letter, c. "Copy last answer": C is taken by that, so L.
        assert_eq!(out, ["&Ask now", "A&ctions", "Copy &last answer"]);
    }

    #[test]
    fn an_explicit_mark_is_kept_and_reserved_before_others_are_assigned() {
        let out = assign_mnemonics(&labels(&["Pause", "Pro&viders", "&Mode"]));
        // Providers keeps v; Pause takes p; Mode keeps m.
        assert_eq!(out, ["&Pause", "Pro&viders", "&Mode"]);
    }

    #[test]
    fn a_literal_ampersand_is_never_a_mnemonic() {
        let out = assign_mnemonics(&labels(&["&&Tom"]));
        assert_eq!(out, ["&&&Tom"]);
        assert_eq!(strip_mnemonics(&out[0]), "&Tom");
        // Nothing but punctuation: no mnemonic, label unchanged.
        assert_eq!(assign_mnemonics(&labels(&["..."])), ["..."]);
    }

    #[test]
    fn only_ascii_letters_and_digits_are_used() {
        let out = assign_mnemonics(&labels(&["\u{00E9}cran", "gpt-4.1"]));
        assert_eq!(out[1], "&gpt-4.1");
        // "écran": the é is skipped, c is the first usable character.
        assert_eq!(out[0], "\u{00E9}&cran");
    }

    #[test]
    fn a_unique_letter_runs_the_row_and_a_shared_one_cycles() {
        let m = [Some('a'), Some('s'), None, Some('s')];
        assert_eq!(resolve_mnemonic(&m, 'A', None), MenuChar::Execute(0));
        assert_eq!(resolve_mnemonic(&m, 'x', None), MenuChar::Ignore);
        assert_eq!(resolve_mnemonic(&m, 's', None), MenuChar::Select(1));
        assert_eq!(resolve_mnemonic(&m, 's', Some(1)), MenuChar::Select(3));
        assert_eq!(resolve_mnemonic(&m, 's', Some(3)), MenuChar::Select(1));
    }

    #[test]
    fn sizes_scale_with_the_dpi() {
        assert_eq!(px(32, 96), 32);
        assert_eq!(px(32, 144), 48);
        assert_eq!(px(32, 192), 64);
        let plain = ItemRecord::new("x", Kind::Item);
        assert_eq!(row_height(&plain, 96), 32);
        assert_eq!(row_height(&plain, 192), 64);
        let sep = ItemRecord::new("", Kind::Separator);
        assert_eq!(row_height(&sep, 96), 9);
        let mut first = plain.clone();
        first.pad_top = true;
        assert_eq!(row_height(&first, 96), 36);
        first.pad_bottom = true;
        assert_eq!(row_height(&first, 96), 40);
    }

    #[test]
    fn a_row_is_as_wide_as_its_content_within_the_limits() {
        // Short label: the minimum.
        assert_eq!(row_width(40, 0, false, 96), 200);
        // A hint and a chevron widen the row.
        let with_hint = row_width(150, 60, false, 96);
        let with_chevron = row_width(150, 0, true, 96);
        assert!(with_hint > row_width(150, 0, false, 96));
        assert!(with_chevron > row_width(150, 0, false, 96));
        // Very long: capped.
        assert_eq!(row_width(5000, 0, false, 96), 420);
        assert_eq!(row_width(5000, 0, false, 192), 840);
    }

    /// The owner window's procedure must give the menu its messages before
    /// the Settings reentrancy policy sees them, or a menu opened while that
    /// policy is on would measure every row as nothing (wired to nothing).
    #[test]
    fn the_owner_window_forwards_menu_messages_first() {
        let src = include_str!("../app.rs").replace('\r', "");
        let start = src
            .find("extern \"system\" fn wnd_proc(")
            .expect("app.rs has no wnd_proc");
        let body = &src[start..];
        let forward = body
            .find("tray_menu::handle_message(")
            .expect("wnd_proc never calls tray_menu::handle_message");
        let policy = body
            .find("if SETTINGS_OPEN.with(")
            .expect("wnd_proc lost its reentrancy gate");
        assert!(forward < policy, "the menu forward must come first");
    }

    /// An owner-drawn row has no text of its own, so a screen reader reads the
    /// name the row was given: the label without `&`, a shortcut after a tab
    /// and a submenu's current value after a comma (UI Automation showed
    /// every name empty before this, #825).
    #[test]
    fn rows_carry_an_accessible_name() {
        let name_of = |rec: ItemRecord| {
            let (_, ptr) = add_record(rec);
            let mut len = 0;
            // SAFETY: add_record returns a pointer to a NUL-terminated
            // buffer the table owns until the next begin_build.
            unsafe {
                while *ptr.add(len) != 0 {
                    len += 1;
                }
                String::from_utf16_lossy(std::slice::from_raw_parts(ptr, len))
            }
        };
        begin_build();
        let mut ask = ItemRecord::new("Ask &now", Kind::Item);
        ask.hint = "Win+Shift+F23".to_string();
        assert_eq!(name_of(ask), "Ask now\tWin+Shift+F23");
        let mut mode = ItemRecord::new("&Mode", Kind::Submenu);
        mode.hint = "Local".to_string();
        assert_eq!(name_of(mode), "Mode, Local");
        assert_eq!(
            name_of(ItemRecord::new("Tom && Jerry", Kind::Item)),
            "Tom & Jerry"
        );
    }

    #[test]
    fn records_round_trip_and_a_new_build_forgets_the_old_ones() {
        begin_build();
        let (a, _) = add_record(ItemRecord::new("&Alpha", Kind::Item));
        let (b, _) = add_record(ItemRecord::new("Beta", Kind::Submenu));
        assert_eq!(plain_label(a).as_deref(), Some("Alpha"));
        assert_eq!(record(b).map(|r| r.kind), Some(Kind::Submenu));
        begin_build();
        assert!(record(a).is_none());
    }

    #[test]
    fn hover_and_text_colours_are_readable_in_every_theme() {
        use crate::ui::theme::{contrast_ratio, relative_luminance, DARK, HC_BLACK, LIGHT};
        for (theme, tokens, sys) in [
            (Theme::Light, LIGHT, HC_BLACK),
            (Theme::Dark, DARK, HC_BLACK),
            (
                Theme::HighContrast,
                Tokens::for_theme(Theme::HighContrast, &HC_BLACK),
                HC_BLACK,
            ),
        ] {
            let ratio =
                |fg: u32, bg: u32| contrast_ratio(relative_luminance(fg), relative_luminance(bg));
            let rest = theme::menu_text_color(theme, &tokens, &sys, false, false);
            assert!(ratio(rest, tokens.surface) >= 4.5, "{theme:?} resting");
            let hot_bg = theme::menu_hover_fill(theme, &tokens, &sys);
            let hot = theme::menu_text_color(theme, &tokens, &sys, true, false);
            assert!(ratio(hot, hot_bg) >= 4.5, "{theme:?} hot");
            assert_ne!(hot_bg, tokens.surface, "{theme:?}: hover must show");
            let off = theme::menu_text_color(theme, &tokens, &sys, false, true);
            assert_ne!(off, rest, "{theme:?}: disabled must look different");
        }
    }
}
