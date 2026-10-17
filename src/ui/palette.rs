//! Win32 window for the Quick Ask palette (#25): a pre-created, hidden,
//! topmost, DPI-aware popup with a child `EDIT` control for the query and a
//! DirectWrite/Direct2D-painted row list below it. Every decision (fuzzy
//! scoring, grouping, key handling, dispatch) lives in
//! [`crate::ui::palette_model`], which this module only renders and forwards
//! Win32 messages into -- see that module's doc comment and
//! `docs/superpowers/specs/2026-09-17-palette-design.md`.
//!
//! # Rendering (issue #216)
//!
//! The row list and footer render through
//! `ID2D1HwndRenderTarget` + `IDWriteTextFormat` (see [`PaletteRenderer`]
//! below), not plain GDI `DrawTextW` -- #25's original body named
//! DirectWrite as the expected renderer; the first cut used GDI to keep the
//! pre-created window's first paint on the sub-100ms path with no new
//! dependency (see the design spec's now-superseded "Rendering" section),
//! and #216 is this follow-up.
//!
//! `ID2D1RenderTarget::DrawText` with a cached `IDWriteTextFormat` is used
//! instead of building a per-row `IDWriteTextLayout` + `DrawTextLayout`:
//! `DrawTextLayout`'s `origin` parameter is a `windows_numerics::Vector2`,
//! a type the `windows` crate does not re-export, so naming it would need a
//! new direct Cargo dependency (`windows-numerics`) beyond this task's
//! `Cargo.toml` scope (the `windows` crate's own feature list only, since
//! `Cargo.toml` is contended across the overnight run). `DrawText` still
//! goes through the same DirectWrite text renderer and `IDWriteTextFormat`
//! (font, single-line, vertical-centering); it just skips materializing an
//! intermediate layout object this module never otherwise needs (no
//! hit-testing, no multi-format runs).
//!
//! The D2D factory, DirectWrite factory and the `IDWriteTextFormat` are
//! created once, in [`Palette::create`], right after the `HWND` exists --
//! never per paint (that is what would blow the sub-100ms show-latency
//! budget: see the MEASURED block below). The `ID2D1HwndRenderTarget` is
//! created lazily on first use from the same call (so it always exists
//! before the first real paint) and is resized in place
//! (`ID2D1HwndRenderTarget::Resize`) on every window-size or DPI change
//! rather than recreated. If Direct2D/DirectWrite factory creation fails
//! at window-creation time (e.g. no Direct2D support at all -- rare, but
//! rule 7 says every failure ends in a card, never a silent blank palette),
//! or `EndDraw` ever returns `D2DERR_RECREATE_TARGET` (device loss: the
//! GPU driver reset or the adapter went away), [`PaletteInner::on_paint`]
//! falls back to the exact GDI `DrawTextW` path this module used before
//! #216 rather than paint nothing -- device loss additionally drops the
//! render target so the next paint recreates it from scratch.
//!
//! MEASURED 2026-09-19, this machine, `dev` profile, machine otherwise idle,
//! `cargo test ui::palette::tests::measure_show_latency -- --ignored
//! --nocapture`, 20 shows of a real palette window: **avg 12.94 ms, max
//! 44.11 ms**, the max being the first-show outlier (44.11 ms; every
//! subsequent sample is 9.2 to 12.9 ms). The GDI baseline this replaced was
//! avg 11.84 ms / max 34.53 ms (MEASURED 2026-09-17, same harness), so
//! DirectWrite costs roughly 1 ms on average here and stays far inside
//! #25's under-100 ms Done-when. Not re-measured in release; the debug
//! number already clears the budget by a factor of seven, and `opt-level =
//! "z"` plus LTO only moves it down.
//!
//! The fallback above is the reason
//! [`tests::show_leaves_a_live_direct2d_renderer_not_a_silent_gdi_fallback`]
//! exists: a DirectWrite path that never initializes at all paints
//! identically, passes every other test, and keeps this latency number in
//! budget, because GDI was fast too. That test asserts the renderer, its
//! render target and its text format are really there after a real show.
//!
//! Per-monitor-v2 DPI: `WM_DPICHANGED` updates `self.dpi`, re-lays-out the
//! query edit control, resizes the window to the system's suggested rect
//! (the standard per-monitor-v2 contract), and calls
//! `ID2D1RenderTarget::SetDpi` on the render target -- the target's DPI is
//! never read once and reused; see `handle_message`'s `WM_DPICHANGED` arm.
//! Content itself is laid out in logical (96-DPI) DIPs, exactly the row/
//! padding/font constants already used for the window's own logical size
//! math -- `SetDpi` plus `Resize`'d target then does the DPI scaling to
//! physical pixels, so no D2D draw call scales anything itself.
//!
//! # Window lifecycle
//!
//! Created once at startup ([`Palette::new`]), never destroyed until process
//! exit (rule 5: zero work while hidden -- showing again is a repaint, not a
//! window creation). [`Palette::show`] centers on the active monitor,
//! resets the query, and makes the window visible; [`Palette::hide`] is
//! `ShowWindow(SW_HIDE)`, never `DestroyWindow`.
//!
//! # Text input and key handling
//!
//! The query `EDIT` control is a child window, so plain `WM_KEYDOWN` never
//! reaches the palette's own `HWND` while it has focus (Win32 delivers
//! keyboard input to whichever window has focus, not to the parent). This
//! crate already solved exactly this problem for the confirm card's preview
//! state (`ui::card`'s `preview_control_subclass`): the query edit control
//! is subclassed the same way (`SetWindowSubclass`/`DefSubclassProc`),
//! mapping a recognized key
//! ([`crate::ui::palette_model::palette_key_from_vk`]) to a `WM_COMMAND`
//! posted back to the palette's own `HWND`, and forwarding `WM_KILLFOCUS`
//! the same way so losing focus hides the palette regardless of which
//! control had it.

use std::ffi::c_void;
use std::sync::{Once, OnceLock};

use crate::ui::palette_paint::{
    build_ops, ellipsizes, focus_band_top, palette_colors, query_edit_rect, row_at, snap_rect,
    snap_stroke, window_height, Face, Op, PaintInput, PaletteColors, SnappedRect, WINDOW_WIDTH,
};
use crate::ui::theme::{
    detect_theme, icon_font_face, setting_change_affects_theme, setting_change_name, SysColors,
    Theme, Tokens, FLUENT_ICON_FONT_FACE, ICON_FONT_DP, TYPE_CAPTION_PX,
};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, D2DERR_RECREATE_TARGET, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, ID2D1RenderTarget,
    ID2D1SolidColorBrush, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE, D2D1_COMBINE_MODE_EXCLUDE,
    D2D1_DRAW_TEXT_OPTIONS_NONE, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
    D2D1_RENDER_TARGET_TYPE_DEFAULT, D2D1_ROUNDED_RECT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT,
    DWRITE_TEXT_ALIGNMENT_CENTER, DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_TRIMMING,
    DWRITE_TRIMMING_GRANULARITY_CHARACTER, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontIndirectW, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetDC,
    GetTextMetricsW, InvalidateRect, ReleaseDC, SelectObject, SetBkColor, SetBkMode, SetTextColor,
    UpdateWindow, CLEARTYPE_QUALITY, DEFAULT_CHARSET, DRAW_TEXT_FORMAT, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, LOGFONTW,
    PAINTSTRUCT, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, LoadCursorW, PostMessageW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HMENU, HWND_TOPMOST, IDC_ARROW,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE,
    WM_APP, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN,
    WM_KILLFOCUS, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_PRINTCLIENT, WM_SETFONT, WM_SETTINGCHANGE, WM_SYSCOLORCHANGE,
    WM_THEMECHANGED, WM_USER, WNDCLASSEXW, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_TABSTOP, WS_VISIBLE,
};

/// `WM_MOUSELEAVE`'s stable, documented value (winuser.h) -- not re-exported
/// by the `windows` crate under `Win32::UI::WindowsAndMessaging` (unlike
/// `WM_MOUSEMOVE`), so spelled out here the same way this file already
/// spells out `EN_CHANGE`.
const WM_MOUSELEAVE: u32 = 0x02A3;

const WC_EDIT: &str = "EDIT";
const ES_AUTOHSCROLL: u32 = 0x0080;

/// Child `EDIT` control id, used to recognize its `EN_CHANGE` notification
/// in `WM_COMMAND`.
const QUERY_EDIT_ID: i32 = 101;
/// `EN_CHANGE`'s stable, documented value (winuser.h) -- not re-exported by
/// `windows` under `Win32::UI::WindowsAndMessaging` for this crate's edit
/// controls, so it is spelled out here the same way `hotkey.rs` spells out
/// its own well-known Win32 constants.
const EN_CHANGE: u32 = 0x0300;

/// Synthetic `WM_COMMAND` ids the query edit control's subclass posts back
/// to the palette window for a recognized key or a focus loss -- never real
/// child-control ids (no control uses these), so there is no risk of
/// colliding with a genuine `EN_CHANGE` notification (which always carries
/// the edit `HWND` in `lParam`; these always carry `0`).
const ID_PALETTE_UP: i32 = 9101;
const ID_PALETTE_DOWN: i32 = 9102;
const ID_PALETTE_ENTER: i32 = 9103;
const ID_PALETTE_ESCAPE: i32 = 9104;
const ID_PALETTE_LOST_FOCUS: i32 = 9105;
/// #217: PageUp/PageDown, forwarded the same way Up/Down already are.
const ID_PALETTE_PAGE_UP: i32 = 9106;
const ID_PALETTE_PAGE_DOWN: i32 = 9107;

const PALETTE_SUBCLASS_ID: usize = 1;

/// Posted to the palette's owner window when Enter selects a runnable
/// action. `lparam` is `Box::into_raw(Box::new(PaletteRun))` (the selected
/// action id and the typed line, #92) -- the receiver takes ownership and must reconstruct the
/// `Box` to free it, the same boxed-payload idiom [`crate::hotkey::WM_APP_LEARNED`]
/// uses. Adding another `WM_APP_*` constant anywhere in the crate also means
/// adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue #163).
pub const WM_APP_PALETTE_RUN: u32 = WM_APP + 11;

// The layout lives in `palette_paint`: every size there is in dp and scaled by
// `scale()` (`px`) at paint and layout time.
/// The placeholder in an empty query box (#881). No em dash (rule 11).
const QUERY_CUE: &str = "Type to filter";
/// Private message the query edit's subclass sends its parent to have the
/// placeholder drawn into the DC it just painted (`wParam` is the HDC, `lParam`
/// the edit). `WM_USER` range: private to this window class.
const WM_PALETTE_DRAW_CUE: u32 = WM_USER + 1;
/// `EM_GETMARGINS`: the low word of the result is the left margin.
const EM_GETMARGINS: u32 = 0x00D4;
/// #217: rows beyond `crate::ui::palette_model::MAX_VISIBLE_ROWS` are
/// reachable by scrolling (keyboard: Up/Down/PageUp/PageDown drag the
/// viewport via `PaletteState::offset`; mouse: `WM_MOUSEWHEEL` below) rather
/// than being truncated -- that constant is the single source of truth for
/// both the window's fixed height and the palette-side viewport math in
/// `palette_model.rs`.
fn scale(v: i32, dpi: u32) -> i32 {
    crate::ui::palette_paint::px(v, dpi)
}

/// The height in pixels of one line of text in `font`, or the body line
/// height at `dpi` when it cannot be measured.
fn font_text_height(font: HFONT, dpi: u32) -> i32 {
    let fallback = scale(crate::ui::theme::TYPE_BODY_LINE_PX, dpi);
    if font.0.is_null() {
        return fallback;
    }
    unsafe {
        let dc = GetDC(None);
        let old = SelectObject(dc, HGDIOBJ(font.0));
        let mut tm = TEXTMETRICW::default();
        let ok = GetTextMetricsW(dc, &mut tm).as_bool();
        SelectObject(dc, old);
        ReleaseDC(None, dc);
        if ok && tm.tmHeight > 0 {
            tm.tmHeight
        } else {
            fallback
        }
    }
}

/// Extracts the signed `y` client coordinate from a mouse message's
/// `lParam` (`GET_Y_LPARAM`, winuser.h) -- not re-exported by the `windows`
/// crate for these messages, so spelled out here the same way this file
/// already spells out `EN_CHANGE`.
fn mouse_y(lparam: LPARAM) -> i32 {
    let raw = lparam.0 as i32 as u32;
    ((raw >> 16) & 0xFFFF) as u16 as i16 as i32
}

/// One owned Quick Ask palette window. Thin handle around a heap-allocated
/// [`PaletteInner`] -- same indirection reasoning as [`crate::ui::card::Card`]:
/// the window's `WNDPROC` stashes a raw pointer in `GWLP_USERDATA` at
/// creation time, which must stay valid for the window's whole life even if
/// this handle moves (e.g. into `App`'s own struct).
pub struct Palette {
    inner: Box<PaletteInner>,
}

impl Palette {
    pub fn new(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the palette window class");
        }
        Palette::create(instance, CLASS_NAME)
    }

    /// Same as [`Palette::new`], but registers (once) and uses a class name
    /// distinct from the production one (rule 9: tests never touch
    /// production names).
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new_for_test(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_test_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the test palette window class");
        }
        Palette::create(instance, TEST_CLASS_NAME)
    }

    fn create(instance: HINSTANCE, class_name: &str) -> anyhow::Result<Self> {
        let theme = detect_theme();
        let sys = SysColors::read();
        let inner = Box::new(PaletteInner {
            hwnd: HWND(std::ptr::null_mut()),
            edit_hwnd: HWND(std::ptr::null_mut()),
            owner: None,
            dpi: 96,
            // Made once the window (and so its DPI) exists.
            font: HFONT(std::ptr::null_mut()),
            d2d: None,
            catalogue: Vec::new(),
            model_configured: true,
            footer_text: String::new(),
            state: crate::ui::palette_model::PaletteState::new(Vec::new()),
            visible: false,
            hover: None,
            ignore_mouse: false,
            tracking_leave: false,
            suppress_en_change: false,
            theme,
            sys,
            colors: palette_colors(theme, &sys, live_accent(theme, &sys)),
            theme_forced: false,
            keep_open: false,
            edit_brush: HBRUSH(std::ptr::null_mut()),
        });
        let raw = Box::into_raw(inner);

        let class_name_w = wide_z(class_name);
        let title = wide_z("Wingman Quick Ask");
        let create_result = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST,
                PCWSTR(class_name_w.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                WINDOW_WIDTH,
                window_height(96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        };

        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(e) => {
                unsafe {
                    drop(Box::from_raw(raw));
                }
                return Err(anyhow::anyhow!(
                    "Wingman: CreateWindowExW (palette) failed: {e}"
                ));
            }
        };

        let inner_ref = unsafe { &mut *raw };
        inner_ref.hwnd = hwnd;
        inner_ref.dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        inner_ref.font = gdi_font(inner_ref.dpi, FW_NORMAL.0 as i32);

        // Best-effort rounded corners (Windows 11), as the card does (#361).
        // A square window on an older system is an acceptable degrade.
        unsafe {
            let pref = DWMWCP_ROUND;
            if let Err(e) = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            ) {
                crate::diagnostics::note_warning(&format!(
                    "Palette: rounded corners unavailable ({}), using square corners",
                    e.code()
                ));
            }
        }

        // #216: created once, here, right after the HWND exists -- never
        // per paint (see the module doc comment's "Rendering" section for
        // why that matters for the show-latency budget). `ensure_target`
        // sizes the render target to the window's actual size at THIS
        // moment (the un-DPI-corrected, logical-96 size the window was just
        // created with above; `reposition_centered` resizes it again, to
        // the real per-monitor size, on the first real `Show`).
        let mut d2d = PaletteRenderer::new();
        if let Some(renderer) = &mut d2d {
            renderer.ensure_target(
                hwnd,
                inner_ref.dpi,
                WINDOW_WIDTH.max(1) as u32,
                window_height(96).max(1) as u32,
            );
        }
        inner_ref.d2d = d2d;

        let edit_hwnd = create_query_edit(hwnd, instance, inner_ref.font, inner_ref.dpi);
        inner_ref.edit_hwnd = edit_hwnd;
        if !edit_hwnd.0.is_null() {
            unsafe {
                let _ = SetWindowSubclass(
                    edit_hwnd,
                    Some(palette_edit_subclass),
                    PALETTE_SUBCLASS_ID,
                    hwnd.0 as usize,
                );
            }
        }

        Ok(Palette {
            inner: unsafe { Box::from_raw(raw) },
        })
    }

    /// Exposed for tests (which need the raw `HWND` to assert against and
    /// to confirm the window exists); production callers never need it --
    /// mirrors [`crate::ui::region::Overlay::hwnd`].
    #[allow(dead_code)]
    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd
    }

    /// Tells the palette which window to `PostMessageW`
    /// [`WM_APP_PALETTE_RUN`] to when Enter selects a runnable action. Call
    /// once, right after [`Palette::new`] -- mirrors
    /// [`crate::ui::card::Card::set_owner`].
    pub fn set_owner(&mut self, hwnd: HWND) {
        self.inner.owner = Some(hwnd);
    }

    pub fn is_visible(&self) -> bool {
        self.inner.visible
    }

    /// Shows the palette centered on the active monitor with a fresh
    /// catalogue, empty query, and grouped (empty-query) rows. Does no
    /// gathering itself -- `catalogue` is built by the caller
    /// (`App::toggle_palette`) from `actions::load_actions()` plus the two
    /// utility entries.
    pub fn show(
        &mut self,
        catalogue: Vec<crate::ui::palette_model::PaletteAction>,
        model_configured: bool,
        footer_text: String,
    ) {
        self.inner.show(catalogue, model_configured, footer_text);
    }

    pub fn hide(&mut self) {
        self.inner.hide();
    }

    /// The palette's own window proc dispatches internally; this is the
    /// seam tests use to feed synthetic messages directly. Unused by this
    /// module's own tests today (they drive the palette through real posted
    /// messages instead, to exercise `wndproc`'s `GWLP_USERDATA` routing and
    /// the subclass's `WM_COMMAND` forwarding too -- see
    /// `real_win32_down_then_enter_dispatches_the_second_action`'s doc
    /// comment), kept as the seam a future test can reach for without going
    /// through a real message pump. Mirrors
    /// [`crate::ui::region::Overlay::handle_message`].
    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn handle_message(
        &mut self,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> Option<LRESULT> {
        self.inner.handle_message(msg, wparam, lparam)
    }

    #[cfg(test)]
    pub(crate) fn edit_hwnd(&self) -> HWND {
        self.inner.edit_hwnd
    }

    #[cfg(test)]
    pub(crate) fn selected_action_id(&self) -> Option<String> {
        self.inner.state.selected_action_id().map(|s| s.to_string())
    }

    /// #357: like [`Palette::selected_action_id`] but for an arbitrary real
    /// row index rather than `state.selected` -- lets a test read off the
    /// action id a hovered/clicked row names without duplicating
    /// `Row::Action`'s field-matching itself.
    #[cfg(test)]
    pub(crate) fn selected_action_id_at(&self, idx: usize) -> Option<String> {
        match self.inner.state.rows.get(idx) {
            Some(crate::ui::palette_model::Row::Action { id, .. }) => Some(id.clone()),
            _ => None,
        }
    }

    /// The UI gallery's `--theme`: draws in `theme` whatever the system is
    /// set to and ignores later theme messages. High contrast uses Windows'
    /// high contrast black colours. Never touches the registry.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn force_theme_for_gallery(&mut self, theme: Theme) {
        self.inner.theme = theme;
        self.inner.sys = crate::ui::theme::HC_BLACK;
        self.inner.theme_forced = true;
        self.inner.apply_colors();
    }

    /// The UI gallery's `--dpi` (#1007): lays the palette out at `dpi` whatever
    /// monitor it is on, as Settings does. Window, query box font and Direct2D
    /// target all follow, the same updates `WM_DPICHANGED` makes.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn force_dpi_for_gallery(&mut self, dpi: u32) {
        self.inner.dpi = dpi.max(1);
        self.inner.rebuild_font();
        if let Some(renderer) = &self.inner.d2d {
            renderer.set_dpi(self.inner.dpi);
        }
        self.inner.reposition_centered();
    }

    /// The UI gallery (#987): paints the window with the real Direct2D path
    /// (the same paint list, text formats and `paint_ops_d2d` the live window
    /// uses) into `hdc`, because a screen capture and `PrintWindow` cannot read
    /// the window's own Direct2D surface. The target is a DC render target at
    /// the window's DPI. The query box is a child window Direct2D never
    /// draws, so it is printed on top with `WM_PRINT`.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn paint_d2d_into_for_gallery(&mut self, hdc: HDC) -> anyhow::Result<()> {
        use windows::Win32::Graphics::Direct2D::{
            ID2D1DCRenderTarget, D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
        };
        let inner = &mut *self.inner;
        let (w, h) = (
            scale(WINDOW_WIDTH, inner.dpi).max(1),
            window_height(inner.dpi).max(1),
        );
        let hwnd = inner.hwnd;
        let dpi = inner.dpi;
        let renderer = inner
            .d2d
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no Direct2D on this window"))?;
        anyhow::ensure!(
            renderer.ensure_target(hwnd, dpi, w as u32, h as u32),
            "no Direct2D target or text format"
        );
        let body = renderer.text_format.clone().expect("ensure_target checked");
        let caption = renderer
            .caption_format
            .clone()
            .unwrap_or_else(|| body.clone());
        let icon = renderer.icon_format.clone().unwrap_or_else(|| body.clone());
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode:
                    windows::Win32::Graphics::Direct2D::Common::D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: dpi as f32,
            dpiY: dpi as f32,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: Default::default(),
        };
        let target: ID2D1DCRenderTarget = unsafe { renderer.factory.CreateDCRenderTarget(&props)? };
        let rect = RECT {
            left: 0,
            top: 0,
            right: w,
            bottom: h,
        };
        unsafe { target.BindDC(hdc, &rect)? };
        let ops = build_ops(
            96,
            &PaintInput {
                rows: &inner.state.rows,
                offset: inner.state.offset,
                selected: inner.state.selected,
                hover: inner.hover,
                footer: &inner.footer_text,
                colors: &inner.colors,
            },
        );
        unsafe { target.BeginDraw() };
        paint_ops_d2d(&target, [&body, &caption, &icon], dpi, &ops);
        unsafe { target.EndDraw(None, None)? };
        // The query box: print it at its own spot in the picture.
        let edit = query_edit_rect(dpi, font_text_height(inner.font, dpi));
        unsafe {
            use windows::Win32::Graphics::Gdi::SetViewportOrgEx;
            use windows::Win32::UI::WindowsAndMessaging::WM_PRINT;
            let _ = SetViewportOrgEx(hdc, edit.left, edit.top, None);
            // PRF_CLIENT | PRF_ERASEBKGND
            SendMessageW(
                inner.edit_hwnd,
                WM_PRINT,
                Some(WPARAM(hdc.0 as usize)),
                Some(LPARAM(0x4 | 0x8)),
            );
            let _ = SetViewportOrgEx(hdc, 0, 0, None);
        }
        Ok(())
    }

    /// The UI gallery (#919): the palette hides itself when its query box
    /// loses focus, so any other window taking the foreground during a run
    /// hid it before the capture. With this set it stays up.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn keep_open_for_gallery(&mut self) {
        self.inner.keep_open = true;
    }

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn set_query_for_test(&mut self, query: &str) {
        self.inner.set_query(query);
    }

    /// The UI gallery's scroll: moves the viewport by `delta_rows`, clamped,
    /// like the mouse wheel.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn scroll_for_test(&mut self, delta_rows: i32) {
        self.inner.state.offset = crate::ui::palette_model::scroll_by(
            self.inner.state.offset,
            delta_rows,
            self.inner.state.rows.len(),
            crate::ui::palette_model::MAX_VISIBLE_ROWS,
        );
    }

    /// #949: from now on the real pointer cannot hover a row (see
    /// `PaletteInner::ignore_mouse`). The gallery and the pixel tests call this
    /// so two runs of the same state draw the same picture.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn ignore_mouse_for_test(&mut self) {
        self.inner.ignore_mouse = true;
        self.inner.hover = None;
    }

    /// The UI gallery's hovered row, as if the mouse were over it.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn set_hover_for_test(&mut self, row: Option<usize>) {
        self.inner.hover = row;
        self.inner.invalidate();
    }

    #[cfg(test)]
    pub(crate) fn state_offset(&self) -> usize {
        self.inner.state.offset
    }
}

impl Drop for Palette {
    fn drop(&mut self) {
        unsafe {
            if !self.inner.hwnd.0.is_null() {
                let _ = DestroyWindow(self.inner.hwnd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Window class registration (once per process)
// ---------------------------------------------------------------------------

const CLASS_NAME: &str = "Wingman.Palette.Window.9c4e2b17";
/// Rule 9: tests never touch production names.
#[cfg(any(test, debug_assertions))]
const TEST_CLASS_NAME: &str = "Wingman.Palette.Window.9c4e2b17.Test";

static CLASS_INIT: Once = Once::new();
static CLASS_OK: OnceLock<bool> = OnceLock::new();
#[cfg(any(test, debug_assertions))]
static TEST_CLASS_INIT: Once = Once::new();
#[cfg(any(test, debug_assertions))]
static TEST_CLASS_OK: OnceLock<bool> = OnceLock::new();

fn ensure_class_registered(instance: HINSTANCE) -> bool {
    CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance, CLASS_NAME) };
        let _ = CLASS_OK.set(ok);
    });
    CLASS_OK.get().copied().unwrap_or(false)
}

#[cfg(any(test, debug_assertions))]
fn ensure_test_class_registered(instance: HINSTANCE) -> bool {
    TEST_CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance, TEST_CLASS_NAME) };
        let _ = TEST_CLASS_OK.set(ok);
    });
    TEST_CLASS_OK.get().copied().unwrap_or(false)
}

unsafe fn register_class(instance: HINSTANCE, class_name_str: &str) -> bool {
    let class_name = wide_z(class_name_str);
    let cursor = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: Default::default(),
        hCursor: cursor,
        hbrBackground: Default::default(),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc) != 0
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        if !cs.lpCreateParams.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut PaletteInner;
    if !ptr.is_null() {
        let inner = &mut *ptr;
        if let Some(result) = inner.handle_message(msg, wparam, lparam) {
            return result;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------------------
// Query edit control + its key/focus subclass
// ---------------------------------------------------------------------------

fn create_query_edit(parent: HWND, instance: HINSTANCE, font: HFONT, dpi: u32) -> HWND {
    let class_w = wide_z(WC_EDIT);
    let text_w = wide_z("");
    let rect = query_edit_rect(dpi, font_text_height(font, dpi));
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL,
            ),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(parent),
            Some(HMENU(QUERY_EDIT_ID as isize as *mut c_void)),
            Some(instance),
            None,
        )
    };
    let Ok(hwnd) = hwnd else {
        return HWND(std::ptr::null_mut());
    };
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    hwnd
}

/// Subclass installed on the query edit control so Up/Down/Enter/Esc and
/// focus loss work regardless of Win32's normal "keyboard input goes to the
/// focused child, not its parent" routing -- see the module doc comment's
/// "Text input and key handling" section, and `ui::card`'s
/// `preview_control_subclass` for the pattern this mirrors.
unsafe extern "system" fn palette_edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let parent = HWND(ref_data as *mut c_void);

    if msg == WM_KEYDOWN {
        if let Some(key) = crate::ui::palette_model::palette_key_from_vk(wparam.0 as u16) {
            let command_id = match key {
                crate::ui::palette_model::PaletteKey::Up => ID_PALETTE_UP,
                crate::ui::palette_model::PaletteKey::Down => ID_PALETTE_DOWN,
                crate::ui::palette_model::PaletteKey::PageUp => ID_PALETTE_PAGE_UP,
                crate::ui::palette_model::PaletteKey::PageDown => ID_PALETTE_PAGE_DOWN,
                crate::ui::palette_model::PaletteKey::Enter => ID_PALETTE_ENTER,
                crate::ui::palette_model::PaletteKey::Escape => ID_PALETTE_ESCAPE,
            };
            let _ = PostMessageW(
                Some(parent),
                WM_COMMAND,
                WPARAM(command_id as usize),
                LPARAM(0),
            );
            // Swallow: Up/Down/PageUp/PageDown/Enter/Esc must never edit the
            // query text or otherwise be handled by the edit control's own
            // default proc.
            return LRESULT(0);
        }
    }
    // The placeholder (#881): the control paints itself, then the palette
    // draws its cue over the empty box.
    if msg == WM_PAINT || msg == WM_PRINTCLIENT {
        let result = DefSubclassProc(hwnd, msg, wparam, lparam);
        if GetWindowTextLengthW(hwnd) == 0 {
            let own_dc = (msg == WM_PAINT).then(|| GetDC(Some(hwnd)));
            let hdc = own_dc.unwrap_or(HDC(wparam.0 as *mut c_void));
            SendMessageW(
                parent,
                WM_PALETTE_DRAW_CUE,
                Some(WPARAM(hdc.0 as usize)),
                Some(LPARAM(hwnd.0 as isize)),
            );
            if own_dc.is_some() {
                ReleaseDC(Some(hwnd), hdc);
            }
        }
        return result;
    }
    if msg == WM_KILLFOCUS {
        let _ = PostMessageW(
            Some(parent),
            WM_COMMAND,
            WPARAM(ID_PALETTE_LOST_FOCUS as usize),
            LPARAM(0),
        );
        // Fall through: the control still needs to actually lose focus.
    }
    if msg == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(palette_edit_subclass), PALETTE_SUBCLASS_ID);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------------------
// D2D/DirectWrite rendering (#216) -- see the module doc comment's
// "Rendering" section for the design and the GDI-fallback/device-loss story.
// ---------------------------------------------------------------------------

/// Font family and size for the palette's DirectWrite text. Logical (DIP)
/// size, not scaled by DPI here -- `ID2D1RenderTarget::SetDpi` plus a
/// correctly `Resize`'d target does that scaling; see the module doc
/// comment. Roughly matches `DEFAULT_GUI_FONT`'s visual size at 96 DPI.
const D2D_FONT_FAMILY: &str = "Segoe UI";

/// How closely Direct2D follows the curve of the focus line (the API default).
const FOCUS_FLATTENING_TOLERANCE: f32 = 0.25;
const D2D_FONT_SIZE_DIP: f32 = 14.0;
/// `CreateTextFormat`'s locale -- matches `ocr.rs`'s recognizer language
/// choice elsewhere in this crate rather than leaving it to the current
/// thread's locale, which is not guaranteed to be English on every machine
/// this runs on.
const D2D_LOCALE: &str = "en-US";

/// Owns the Direct2D/DirectWrite resources for one palette window: the two
/// factories (created once, in [`Palette::create`], and never per paint --
/// see the module doc comment) and the `ID2D1HwndRenderTarget`/
/// `IDWriteTextFormat`, both created lazily (on first use, or again after a
/// device-loss drop) rather than up front, since the very first creation
/// needs a real `HWND` and a real pixel size to size the target to.
struct PaletteRenderer {
    factory: ID2D1Factory,
    dwrite_factory: IDWriteFactory,
    target: Option<ID2D1HwndRenderTarget>,
    text_format: Option<IDWriteTextFormat>,
    /// The footer's caption size, and the icon font. Without either, the
    /// body format draws in its place.
    caption_format: Option<IDWriteTextFormat>,
    icon_format: Option<IDWriteTextFormat>,
}

impl PaletteRenderer {
    /// `None` on any failure to create either factory -- rare (no Direct2D
    /// support at all), but rule 7 says every failure ends in a card, never
    /// a panic: the caller falls back to the GDI path for the lifetime of
    /// this window rather than unwrap either factory.
    fn new() -> Option<Self> {
        let factory: ID2D1Factory =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None) }.ok()?;
        let dwrite_factory: IDWriteFactory =
            unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED) }.ok()?;
        Some(Self {
            factory,
            dwrite_factory,
            target: None,
            text_format: None,
            caption_format: None,
            icon_format: None,
        })
    }

    /// Creates the render target and text format if they do not already
    /// exist (first call, or a call after [`PaletteRenderer::drop_target`]
    /// dropped a lost device) -- a no-op otherwise. Returns whether both now
    /// exist, which is what [`PaletteInner::try_paint_d2d`] uses to decide
    /// whether to paint via D2D at all this time.
    fn ensure_target(&mut self, hwnd: HWND, dpi: u32, width: u32, height: u32) -> bool {
        if self.target.is_none() {
            let rt_props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_IGNORE,
                },
                dpiX: dpi as f32,
                dpiY: dpi as f32,
                usage: Default::default(),
                minLevel: Default::default(),
            };
            let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd,
                pixelSize: D2D_SIZE_U {
                    width: width.max(1),
                    height: height.max(1),
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            self.target = unsafe {
                self.factory
                    .CreateHwndRenderTarget(&rt_props as *const _, &hwnd_props as *const _)
            }
            .ok();
        }
        if self.target.is_some() && self.text_format.is_none() {
            self.text_format = self.build_text_format(
                D2D_FONT_FAMILY,
                D2D_FONT_SIZE_DIP,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                ellipsizes(Face::Body),
            );
            self.caption_format = self.build_text_format(
                D2D_FONT_FAMILY,
                TYPE_CAPTION_PX as f32,
                DWRITE_TEXT_ALIGNMENT_LEADING,
                ellipsizes(Face::Caption),
            );
            self.icon_format = self.build_text_format(
                icon_face(),
                ICON_FONT_DP as f32,
                DWRITE_TEXT_ALIGNMENT_CENTER,
                ellipsizes(Face::Icon),
            );
        }
        self.target.is_some() && self.text_format.is_some()
    }

    fn build_text_format(
        &self,
        face: &str,
        size_dip: f32,
        align: DWRITE_TEXT_ALIGNMENT,
        ellipsis: bool,
    ) -> Option<IDWriteTextFormat> {
        let family = wide_z(face);
        let locale = wide_z(D2D_LOCALE);
        let format = unsafe {
            self.dwrite_factory.CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size_dip,
                PCWSTR(locale.as_ptr()),
            )
        }
        .ok()?;
        unsafe {
            // DT_LEFT | DT_VCENTER | DT_SINGLELINE's DirectWrite equivalent:
            // leading (left) horizontal alignment, centered vertically
            // within the layout box, no wrapping (every row is one line).
            let _ = format.SetTextAlignment(align);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        }
        if ellipsis {
            self.set_ellipsis_trimming(&format);
        }
        Some(format)
    }

    /// Makes `format` end a line that is wider than its box in an ellipsis
    /// (#1000), the Direct2D side of GDI's `DT_END_ELLIPSIS`. The trimming sign
    /// is created here, once for the format, never per frame. Any failure leaves
    /// the format as it was: the text is then drawn without trimming (clipped
    /// at the box) rather than not at all.
    fn set_ellipsis_trimming(&self, format: &IDWriteTextFormat) {
        let Ok(sign) = (unsafe { self.dwrite_factory.CreateEllipsisTrimmingSign(format) }) else {
            return;
        };
        let trimming = DWRITE_TRIMMING {
            granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
            delimiter: 0,
            delimiterCount: 0,
        };
        let _ = unsafe { format.SetTrimming(&trimming as *const _, &sign) };
    }

    /// Resizes the render target's pixel buffer to match a real window-size
    /// change (a fresh `Show`, or `WM_DPICHANGED`'s suggested rect) -- called
    /// instead of recreating the target, per the module doc comment. Device
    /// loss can surface here too (`Resize` can return
    /// `D2DERR_RECREATE_TARGET` the same as `EndDraw`), handled the same
    /// way: drop the target so the next paint recreates it from scratch.
    fn resize(&mut self, width: u32, height: u32) {
        let Some(target) = &self.target else {
            return;
        };
        let size = D2D_SIZE_U {
            width: width.max(1),
            height: height.max(1),
        };
        if unsafe { target.Resize(&size as *const _) }.is_err() {
            self.target = None;
        }
    }

    /// `WM_DPICHANGED` calls this so the render target's own DPI always
    /// tracks the window's current monitor rather than the DPI it was
    /// created with (see the module doc comment's "Per-monitor-v2 DPI"
    /// paragraph).
    fn set_dpi(&self, dpi: u32) {
        if let Some(target) = &self.target {
            unsafe {
                target.SetDpi(dpi as f32, dpi as f32);
            }
        }
    }

    /// Drops a lost device's render target after `EndDraw` reports
    /// `D2DERR_RECREATE_TARGET` -- the next paint's `ensure_target` call
    /// recreates it (and the text format, which belongs to the old
    /// `IDWriteFactory`'s target-independent state but is cheap enough to
    /// just rebuild alongside it rather than special-case keeping it).
    fn drop_target(&mut self) {
        self.target = None;
        self.text_format = None;
        self.caption_format = None;
        self.icon_format = None;
    }
}

/// Converts this crate's existing `COLORREF` palette (0x00bbggrr, the GDI
/// convention every color constant in this module already uses) to D2D's
/// `D2D1_COLOR_F` -- one conversion point so the GDI-era constants stay the
/// single source of truth for this module's colors rather than forking into
/// a second, D2D-only set that could drift from them.
fn colorref_to_d2d(c: COLORREF) -> D2D1_COLOR_F {
    let v = c.0;
    D2D1_COLOR_F {
        r: (v & 0xFF) as f32 / 255.0,
        g: ((v >> 8) & 0xFF) as f32 / 255.0,
        b: ((v >> 16) & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

/// The user's accent for the selection bar in `theme`, kept at 3:1 against the
/// theme's surface (the same shade and fit the primary button uses). Read live,
/// so call it when the theme or the accent changes, never per paint.
fn live_accent(theme: Theme, sys: &SysColors) -> u32 {
    crate::ui::card_buttons::system_accent(theme, Tokens::for_theme(theme, sys).surface)
}
/// The palette's GDI font: Segoe UI, `D2D_FONT_SIZE_DIP` tall at `dpi`, in
/// `weight`. The caller frees it with `DeleteObject`.
fn gdi_font(dpi: u32, weight: i32) -> HFONT {
    gdi_font_face(D2D_FONT_FAMILY, D2D_FONT_SIZE_DIP as i32, dpi, weight)
}

/// A GDI font in `face`, `size_dp` tall at `dpi`. The caller frees it with
/// `DeleteObject`.
fn gdi_font_face(face: &str, size_dp: i32, dpi: u32, weight: i32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -scale(size_dp, dpi),
        lfWeight: weight,
        lfCharSet: DEFAULT_CHARSET,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    for (slot, unit) in lf.lfFaceName.iter_mut().zip(face.encode_utf16()) {
        *slot = unit;
    }
    unsafe { CreateFontIndirectW(&lf) }
}

/// Whether Segoe Fluent Icons is installed (Windows 11), asked of DirectWrite
/// once. A failure to ask counts as not installed, which picks the older icon
/// font: the glyph is drawn either way.
fn fluent_icons_installed() -> bool {
    use windows::Win32::Graphics::DirectWrite::IDWriteFontCollection;
    let Ok(factory) =
        (unsafe { DWriteCreateFactory::<IDWriteFactory>(DWRITE_FACTORY_TYPE_SHARED) })
    else {
        return false;
    };
    let mut collection: Option<IDWriteFontCollection> = None;
    if unsafe { factory.GetSystemFontCollection(&mut collection, false) }.is_err() {
        return false;
    }
    let Some(collection) = collection else {
        return false;
    };
    let name = wide_z(FLUENT_ICON_FONT_FACE);
    let (mut index, mut exists) = (0u32, windows::core::BOOL(0));
    unsafe { collection.FindFamilyName(PCWSTR(name.as_ptr()), &mut index, &mut exists) }.is_ok()
        && exists.as_bool()
}

/// The font the palette (and the card's footer and error mark) draw icon
/// glyphs in.
pub(crate) fn icon_face() -> &'static str {
    static FLUENT: OnceLock<bool> = OnceLock::new();
    icon_font_face(*FLUENT.get_or_init(fluent_icons_installed))
}

/// D2D/DirectWrite equivalent of `draw_text_line` (imported from
/// `crate::ui::text` for the GDI fallback path below): draws one line of
/// `text` in `rect` (logical/DIP coordinates -- see the module doc comment)
/// with `color`, or does nothing at all when `text` is empty. Unlike the GDI
/// version, the empty-string check here is a plain optimization (skip a
/// wasted brush + draw call), not a crash guard -- `IDWriteFactory`/
/// `ID2D1RenderTarget` do not share `DrawTextW`'s zero-length-buffer bug.
fn draw_text_line_d2d(
    target: &ID2D1RenderTarget,
    format: &IDWriteTextFormat,
    text: &str,
    rect: D2D_RECT_F,
    color: COLORREF,
) {
    if text.is_empty() {
        return;
    }
    let buf = utf16(text);
    let Ok(brush) =
        (unsafe { target.CreateSolidColorBrush(&colorref_to_d2d(color) as *const _, None) })
    else {
        return;
    };
    unsafe {
        target.DrawText(
            &buf,
            format,
            &rect as *const _,
            &brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
}

/// A rectangle in dp (or pixels) as Direct2D wants it.
fn rect_f(r: &RECT) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left as f32,
        top: r.top as f32,
        right: r.right as f32,
        bottom: r.bottom as f32,
    }
}

/// A snapped rectangle as Direct2D wants it.
fn snapped_rect_f(r: SnappedRect) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// Draws a paint list with Direct2D. `formats` are the body, caption and icon
/// text formats. A brush that cannot be made leaves that one shape out, so a
/// single failure never blanks the window.
fn paint_ops_d2d(
    target: &ID2D1RenderTarget,
    formats: [&IDWriteTextFormat; 3],
    dpi: u32,
    ops: &[Op],
) {
    for op in ops {
        let color = match op {
            Op::Clear { color }
            | Op::FillRound { color, .. }
            | Op::StrokeRound { color, .. }
            | Op::FocusLine { color, .. }
            | Op::Text { color, .. } => *color,
        };
        match op {
            Op::Clear { .. } => {
                unsafe {
                    target.Clear(Some(&colorref_to_d2d(COLORREF(color)) as *const _));
                }
                continue;
            }
            Op::Text {
                rect, text, face, ..
            } => {
                let format = match face {
                    Face::Body => formats[0],
                    Face::Caption => formats[1],
                    Face::Icon => formats[2],
                };
                draw_text_line_d2d(target, format, text, rect_f(rect), COLORREF(color));
                continue;
            }
            _ => {}
        }
        let Ok(brush) =
            (unsafe { target.CreateSolidColorBrush(&colorref_to_d2d(COLORREF(color)), None) })
        else {
            continue;
        };
        match op {
            Op::Clear { .. } | Op::Text { .. } => {}
            Op::FillRound { rect, radius, .. } => {
                let shape = D2D1_ROUNDED_RECT {
                    rect: snapped_rect_f(snap_rect(*rect, dpi)),
                    radiusX: *radius as f32,
                    radiusY: *radius as f32,
                };
                unsafe { target.FillRoundedRectangle(&shape as *const _, &brush) };
            }
            Op::StrokeRound {
                rect,
                radius,
                width,
                ..
            } => {
                // A stroke is centred on its path: snap_stroke puts the path half
                // a stroke in from edges that are whole device pixels, so a 1 dp
                // border is one crisp pixel line at 150 and 250 percent (#1001).
                let s = snap_stroke(*rect, *radius, *width, dpi);
                let shape = D2D1_ROUNDED_RECT {
                    rect: snapped_rect_f(s.rect),
                    radiusX: s.radius,
                    radiusY: s.radius,
                };
                unsafe { target.DrawRoundedRectangle(&shape as *const _, &brush, s.width, None) };
            }
            Op::FocusLine {
                rect,
                radius,
                width,
                ..
            } => fill_focus_line_d2d(target, &brush, dpi, *rect, *radius, *width),
        }
    }
}

/// The focus line (#1039): the query box's shape less the same shape moved up
/// by the line's width, cut to the band at the bottom. It follows the corner
/// arcs and thins to nothing where they meet the sides. A failure to build
/// the geometry leaves the line out and nothing else.
fn fill_focus_line_d2d(
    target: &ID2D1RenderTarget,
    brush: &ID2D1SolidColorBrush,
    dpi: u32,
    rect: RECT,
    radius: i32,
    width: i32,
) {
    let outer = snapped_rect_f(snap_rect(rect, dpi));
    let w = snap_stroke(rect, radius, width, dpi).width;
    let r = radius as f32;
    let shape = |dy: f32| D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            top: outer.top - dy,
            bottom: outer.bottom - dy,
            ..outer
        },
        radiusX: r,
        radiusY: r,
    };
    // As `focus_band_top`, in dp.
    let band = D2D_RECT_F {
        top: outer.bottom - r - w,
        ..outer
    };
    unsafe {
        let Ok(factory) = target.GetFactory() else {
            return;
        };
        let (Ok(a), Ok(b), Ok(path)) = (
            factory.CreateRoundedRectangleGeometry(&shape(0.0)),
            factory.CreateRoundedRectangleGeometry(&shape(w)),
            factory.CreatePathGeometry(),
        ) else {
            return;
        };
        let Ok(sink) = path.Open() else {
            return;
        };
        let combined = a.CombineWithGeometry(
            &b,
            D2D1_COMBINE_MODE_EXCLUDE,
            None,
            FOCUS_FLATTENING_TOLERANCE,
            &sink,
        );
        let closed = sink.Close();
        if combined.is_err() || closed.is_err() {
            return;
        }
        target.PushAxisAlignedClip(&band, D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        target.FillGeometry(&path, brush, None);
        target.PopAxisAlignedClip();
    }
}

/// Draws a paint list with GDI into `hdc`, whose client area is `client`:
/// the `WM_PRINTCLIENT` picture, and the fallback when Direct2D is missing.
/// Corners are GDI's `RoundRect`, as everywhere else in the app.
fn paint_ops_gdi(hdc: HDC, dpi: u32, client: RECT, ops: &[Op]) {
    use windows::Win32::Graphics::Gdi::{
        CombineRgn, CreatePen, CreateRectRgn, CreateRoundRectRgn, FillRgn, GetStockObject,
        RoundRect, DT_CENTER, NULL_BRUSH, PS_INSIDEFRAME, RGN_AND, RGN_DIFF, RGN_ERROR,
    };
    unsafe {
        SetBkMode(hdc, TRANSPARENT);
        let body = gdi_font(dpi, FW_NORMAL.0 as i32);
        let caption = gdi_font_face(D2D_FONT_FAMILY, TYPE_CAPTION_PX, dpi, FW_NORMAL.0 as i32);
        let icon = gdi_font_face(icon_face(), ICON_FONT_DP, dpi, FW_NORMAL.0 as i32);
        let old_font = SelectObject(hdc, HGDIOBJ(body.0));
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
        for op in ops {
            match op {
                Op::Clear { color } => {
                    let brush = CreateSolidBrush(COLORREF(*color));
                    FillRect(hdc, &client, brush);
                    let _ = DeleteObject(brush.into());
                }
                Op::FillRound {
                    rect,
                    radius,
                    color,
                } => {
                    // The pen is the fill's own colour, drawn inside the
                    // rectangle, so the shape is exactly `rect`.
                    let brush = CreateSolidBrush(COLORREF(*color));
                    let pen = CreatePen(PS_INSIDEFRAME, 1, COLORREF(*color));
                    let (ob, op_) = (
                        SelectObject(hdc, HGDIOBJ(brush.0)),
                        SelectObject(hdc, HGDIOBJ(pen.0)),
                    );
                    let _ = RoundRect(
                        hdc,
                        rect.left,
                        rect.top,
                        rect.right,
                        rect.bottom,
                        radius * 2,
                        radius * 2,
                    );
                    SelectObject(hdc, ob);
                    SelectObject(hdc, op_);
                    let _ = DeleteObject(pen.into());
                    let _ = DeleteObject(brush.into());
                }
                Op::StrokeRound {
                    rect,
                    radius,
                    width,
                    color,
                } => {
                    let pen = CreatePen(PS_INSIDEFRAME, *width, COLORREF(*color));
                    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
                    let _ = RoundRect(
                        hdc,
                        rect.left,
                        rect.top,
                        rect.right,
                        rect.bottom,
                        radius * 2,
                        radius * 2,
                    );
                    SelectObject(hdc, old_pen);
                    let _ = DeleteObject(pen.into());
                }
                Op::FocusLine {
                    rect,
                    radius,
                    width,
                    color,
                } => {
                    // The same shape as the Direct2D path, as regions: the
                    // box less the box moved up, cut to the bottom band.
                    // Every region is freed on every path.
                    // A rounded-rectangle region leaves out its right and
                    // bottom edge, `RoundRect` does not: one more to match.
                    let d = radius * 2;
                    let (right, bottom) = (rect.right + 1, rect.bottom + 1);
                    let a = CreateRoundRectRgn(rect.left, rect.top, right, bottom, d, d);
                    let b = CreateRoundRectRgn(
                        rect.left,
                        rect.top - width,
                        right,
                        bottom - width,
                        d,
                        d,
                    );
                    let band = CreateRectRgn(
                        rect.left,
                        focus_band_top(rect.bottom, *radius, *width),
                        rect.right,
                        rect.bottom,
                    );
                    if CombineRgn(Some(a), Some(a), Some(b), RGN_DIFF) != RGN_ERROR
                        && CombineRgn(Some(a), Some(a), Some(band), RGN_AND) != RGN_ERROR
                    {
                        let brush = CreateSolidBrush(COLORREF(*color));
                        let _ = FillRgn(hdc, a, brush);
                        let _ = DeleteObject(brush.into());
                    }
                    for region in [a, b, band] {
                        let _ = DeleteObject(region.into());
                    }
                }
                Op::Text {
                    rect,
                    text,
                    color,
                    face,
                } => {
                    let (font, align) = match face {
                        Face::Body => (body, DT_LEFT),
                        Face::Caption => (caption, DT_LEFT),
                        Face::Icon => (icon, DT_CENTER),
                    };
                    SelectObject(hdc, HGDIOBJ(font.0));
                    SetTextColor(hdc, COLORREF(*color));
                    // #1000: one line, and an ellipsis where it is too wide.
                    let trim = if ellipsizes(*face) {
                        DT_END_ELLIPSIS
                    } else {
                        DRAW_TEXT_FORMAT(0)
                    };
                    draw_text_line(
                        hdc,
                        text,
                        *rect,
                        align | trim | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                    );
                }
            }
        }
        SelectObject(hdc, old_brush);
        SelectObject(hdc, old_font);
        for font in [body, caption, icon] {
            let _ = DeleteObject(font.into());
        }
    }
}

// ---------------------------------------------------------------------------
// Palette state + Win32 message handling
// ---------------------------------------------------------------------------

struct PaletteInner {
    hwnd: HWND,
    edit_hwnd: HWND,
    /// Where [`WM_APP_PALETTE_RUN`] is posted (see [`Palette::set_owner`]).
    owner: Option<HWND>,
    dpi: u32,
    font: HFONT,
    /// Direct2D/DirectWrite resources (#216). `None` only when
    /// [`PaletteRenderer::new`] itself failed (no Direct2D support at all);
    /// [`PaletteInner::try_paint_d2d`] falls back to the GDI path for the
    /// lifetime of this window in that case.
    d2d: Option<PaletteRenderer>,
    /// The full catalogue as of the last [`PaletteInner::show`]; re-filtered
    /// on every `EN_CHANGE` without re-gathering anything (rule 5: no work
    /// while hidden, and no re-gathering while typing either).
    catalogue: Vec<crate::ui::palette_model::PaletteAction>,
    model_configured: bool,
    footer_text: String,
    state: crate::ui::palette_model::PaletteState,
    visible: bool,
    /// #357: the row currently under the mouse cursor (real index into
    /// `state.rows`, same space as `state.selected`), or `None` when the
    /// mouse isn't over the list, isn't over an `Action` row, or hasn't
    /// moved into the window since it was last shown. Separate from
    /// `state.selected` on purpose: moving the mouse off the list must not
    /// forget the keyboard/last-click selection Enter would still run, only
    /// clear the hover highlight (see `WM_MOUSELEAVE` below).
    hover: Option<usize>,
    /// #949: set by the gallery and by pixel tests so the real pointer, which
    /// the system reports to any window it is over, cannot change which row
    /// is hovered. Mouse moves and leaves are then ignored; a state that
    /// wants a hover sets it explicitly. Always false in the live app.
    ignore_mouse: bool,
    /// #357: whether `TrackMouseEvent(TME_LEAVE)` is currently armed for
    /// this window. `TrackMouseEvent` disarms itself the moment it fires
    /// (`WM_MOUSELEAVE`) or the mouse leaves, so it must be re-armed on
    /// every `WM_MOUSEMOVE` that finds it not already tracking -- this flag
    /// avoids the extra syscall on every single mouse-move while still
    /// tracking is armed.
    tracking_leave: bool,
    /// #414: armed around `show`'s own programmatic
    /// `SetWindowTextW(edit_hwnd, "")` clear, which -- MEASURED 2026-09-24 in
    /// a real-window test -- fires a real, synchronous `EN_CHANGE` for this
    /// edit control, not just user typing or `EM_REPLACESEL` as the old
    /// comment here assumed. While armed, `handle_message`'s `EN_CHANGE` arm
    /// treats the notification as the programmatic clear it is and skips the
    /// redundant `rebuild_state` (`show` already calls it with the real,
    /// empty query right after).
    suppress_en_change: bool,
    /// #579: the theme the palette is drawn in, the system colours it was
    /// built from, and the colours themselves. Read when the window is made and
    /// again on every message that can change them (`refresh_theme`), never per
    /// paint.
    theme: Theme,
    sys: SysColors,
    colors: PaletteColors,
    /// The gallery's --theme: later theme messages are ignored.
    theme_forced: bool,
    /// The gallery (#919): losing focus does not hide the palette.
    keep_open: bool,
    /// The brush WM_CTLCOLOREDIT returns for the query box, in the
    /// background colour. Created on first use, dropped when the theme changes.
    edit_brush: HBRUSH,
}

impl Drop for PaletteInner {
    fn drop(&mut self) {
        self.drop_edit_brush();
        if !self.font.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.font.0));
            }
        }
    }
}

impl PaletteInner {
    fn show(
        &mut self,
        catalogue: Vec<crate::ui::palette_model::PaletteAction>,
        model_configured: bool,
        footer_text: String,
    ) {
        self.catalogue = catalogue;
        self.model_configured = model_configured;
        self.footer_text = footer_text;
        self.hover = None;
        self.tracking_leave = false;
        // #414: SetWindowTextW below fires a real, synchronous EN_CHANGE for
        // this edit control -- see `suppress_en_change`'s doc comment. Armed
        // only around this one call, so any later real typing still
        // rebuilds the rows normally.
        self.suppress_en_change = true;
        unsafe {
            let _ = SetWindowTextW(self.edit_hwnd, PCWSTR(wide_z("").as_ptr()));
        }
        self.suppress_en_change = false;
        self.rebuild_state("");
        self.reposition_centered();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            // #395: this window is already WS_EX_TOPMOST, so this call only
            // needs to (re)assert topmost z-order after ShowWindow, never
            // move or resize what reposition_centered() just computed above
            // -- SWP_NOMOVE | SWP_NOSIZE is load-bearing here. The previous
            // 0,0,0,0 call with SWP_NOZORDER (which cancels the HWND_TOPMOST
            // it passed) both moved the window to the origin and collapsed
            // it to 0x0 right after positioning it (issue #395).
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOMOVE | SWP_NOSIZE,
            );
            let _ = SetForegroundWindow(self.hwnd);
            let _ = SetFocus(Some(self.edit_hwnd));
        }
        self.visible = true;
        self.invalidate();
        // Forces the first paint to happen synchronously, inside `show`,
        // rather than waiting for the next message-loop iteration to pick
        // up the invalidated region -- this is what makes "shown" and
        // "painted" the same moment for both real key-press latency and
        // `measure_show_latency` below.
        unsafe {
            let _ = UpdateWindow(self.hwnd);
        }
    }

    fn hide(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
        self.visible = false;
        self.hover = None;
        self.tracking_leave = false;
    }

    fn reposition_centered(&mut self) {
        let Ok(monitor) = crate::capture::active_monitor_rect() else {
            return;
        };
        let w = scale(WINDOW_WIDTH, self.dpi);
        let h = window_height(self.dpi);
        let mon_w = monitor.right - monitor.left;
        let mon_h = monitor.bottom - monitor.top;
        let x = monitor.left + (mon_w - w).max(0) / 2;
        let y = monitor.top + (mon_h - h).max(0) / 3; // upper third, like a launcher
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                None,
                x,
                y,
                w,
                h,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
        }
        self.layout_edit_control();
        // #216: the render target's pixel buffer must match the window's
        // real physical size -- resized here (never recreated) alongside
        // every window resize, mirroring the window/edit-control resize
        // right above rather than waiting for a separate WM_SIZE.
        if let Some(renderer) = &mut self.d2d {
            renderer.resize(w.max(1) as u32, h.max(1) as u32);
        }
    }

    /// Positions the query `EDIT` control in the window -- factored
    /// out of [`PaletteInner::reposition_centered`] so `WM_DPICHANGED`'s
    /// handler can re-run the same layout after a DPI change without
    /// re-centering the window on the active monitor too.
    fn layout_edit_control(&self) {
        let r = query_edit_rect(self.dpi, font_text_height(self.font, self.dpi));
        unsafe {
            let _ = SetWindowPos(
                self.edit_hwnd,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
        }
    }

    /// Draws the placeholder into `hdc`, which the query `edit` has just
    /// painted (#881). Left-aligned with the text the user would type.
    fn draw_query_cue(&self, hdc: HDC, edit: HWND) {
        let mut rc = RECT::default();
        unsafe {
            if GetClientRect(edit, &mut rc).is_err() {
                return;
            }
            let margins = SendMessageW(edit, EM_GETMARGINS, None, None).0 as u32 & 0xFFFF;
            rc.left += margins as i32;
            let old_font = SelectObject(hdc, HGDIOBJ(self.font.0));
            SetBkMode(hdc, TRANSPARENT);
            SetTextColor(hdc, COLORREF(self.colors.cue));
            draw_text_line(
                hdc,
                QUERY_CUE,
                rc,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old_font);
        }
    }

    /// A new font for a new DPI, given to the query box (#881).
    fn rebuild_font(&mut self) {
        let old = self.font;
        self.font = gdi_font(self.dpi, FW_NORMAL.0 as i32);
        unsafe {
            SendMessageW(
                self.edit_hwnd,
                WM_SETFONT,
                Some(WPARAM(self.font.0 as usize)),
                Some(LPARAM(1)),
            );
            if !old.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(old.0));
            }
        }
    }

    fn current_query(&self) -> String {
        unsafe {
            let len = GetWindowTextLengthW(self.edit_hwnd);
            if len <= 0 {
                return String::new();
            }
            let mut buf = vec![0u16; len as usize + 1];
            let read = GetWindowTextW(self.edit_hwnd, &mut buf);
            String::from_utf16_lossy(&buf[..read.max(0) as usize])
        }
    }

    /// Only used by the test-only seam [`Palette::set_query_for_test`]:
    /// production always drives the query through a real `EN_CHANGE`
    /// notification (`handle_message`'s `WM_COMMAND` arm), not by setting
    /// the edit control's text programmatically.
    #[cfg(any(test, debug_assertions))]
    fn set_query(&mut self, query: &str) {
        unsafe {
            let _ = SetWindowTextW(self.edit_hwnd, PCWSTR(wide_z(query).as_ptr()));
        }
        self.rebuild_state(query);
        self.invalidate();
    }

    fn rebuild_state(&mut self, query: &str) {
        let rows =
            crate::ui::palette_model::build_rows(&self.catalogue, query, self.model_configured);
        self.state = crate::ui::palette_model::PaletteState::new(rows);
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// #579: reads the theme, the system colours and the accent again and
    /// repaints. Runs on every message that can change them. It repaints even
    /// when the theme is the same: switching between two high contrast
    /// schemes, or picking a new accent, changes the colours and not the theme.
    fn refresh_theme(&mut self) {
        if self.theme_forced {
            return;
        }
        self.theme = detect_theme();
        self.sys = SysColors::read();
        self.apply_colors();
    }

    fn apply_colors(&mut self) {
        self.colors = palette_colors(self.theme, &self.sys, live_accent(self.theme, &self.sys));
        self.drop_edit_brush();
        // RedrawWindow with no window redraws the desktop.
        if self.hwnd.0.is_null() {
            return;
        }
        unsafe {
            use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
            let _ = RedrawWindow(
                Some(self.hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }

    /// The cached query box brush holds the old background; the next
    /// `WM_CTLCOLOREDIT` makes a new one.
    fn drop_edit_brush(&mut self) {
        if !self.edit_brush.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.edit_brush.0));
            }
            self.edit_brush = HBRUSH(std::ptr::null_mut());
        }
    }

    /// Takes the key-forwarded `WM_COMMAND` id, runs it through the pure
    /// state machine, and acts on the outcome: repaint, dispatch, or hide.
    fn on_palette_key_command(&mut self, key: crate::ui::palette_model::PaletteKey) {
        match crate::ui::palette_model::handle_key(&mut self.state, key) {
            crate::ui::palette_model::PaletteOutcome::None => self.invalidate(),
            crate::ui::palette_model::PaletteOutcome::Hide => self.hide(),
            crate::ui::palette_model::PaletteOutcome::Run(action_id) => {
                if let Some(owner) = self.owner {
                    let run = crate::ui::palette_model::PaletteRun {
                        id: action_id,
                        line: self.current_query(),
                    };
                    let boxed = Box::into_raw(Box::new(run));
                    unsafe {
                        if PostMessageW(
                            Some(owner),
                            WM_APP_PALETTE_RUN,
                            WPARAM(0),
                            LPARAM(boxed as isize),
                        )
                        .is_err()
                        {
                            drop(Box::from_raw(boxed));
                        }
                    }
                }
                self.hide();
            }
        }
    }

    /// #216: tries the D2D/DirectWrite path first
    /// ([`PaletteInner::try_paint_d2d`]); falls back to the original GDI
    /// path ([`PaletteInner::paint_gdi`]) whenever D2D is unavailable for
    /// this window (factory creation failed at window-creation time) or a
    /// paint attempt hits device loss -- see the module doc comment. Either
    /// way, `BeginPaint`/`EndPaint` still bracket the call: D2D renders
    /// straight to the `HWND` and never touches the returned `HDC`, but
    /// Win32 still needs `BeginPaint`/`EndPaint` to validate the update
    /// region or `WM_PAINT` never stops firing.
    fn on_paint(&mut self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            if !self.try_paint_d2d() {
                self.paint_gdi(hdc);
            }
            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    /// Attempts the D2D/DirectWrite paint; returns whether it happened at
    /// all (not whether every individual `DrawText` call succeeded -- a
    /// single row failing to get a brush just leaves that row blank, per
    /// [`draw_text_line_d2d`], rather than failing the whole paint).
    /// `false` means the caller must fall back to GDI this time: either
    /// this window has no usable D2D at all, or `EndDraw` reported
    /// `D2DERR_RECREATE_TARGET` (device loss), in which case the target is
    /// dropped so the NEXT paint's `ensure_target` call rebuilds it fresh.
    fn try_paint_d2d(&mut self) -> bool {
        let colors = self.colors;
        let Some(renderer) = &mut self.d2d else {
            return false;
        };
        let w = scale(WINDOW_WIDTH, self.dpi).max(1) as u32;
        let h = window_height(self.dpi).max(1) as u32;
        if !renderer.ensure_target(self.hwnd, self.dpi, w, h) {
            return false;
        }
        // Cloned out (a cheap COM AddRef, not a real copy) so the
        // `EndDraw` device-loss branch below can still mutate
        // `renderer`/`self.d2d` without fighting the borrow checker over a
        // live `&self.d2d` reference.
        let target = renderer
            .target
            .clone()
            .expect("ensure_target just confirmed Some");
        let format = renderer
            .text_format
            .clone()
            .expect("ensure_target just confirmed Some");

        let body = format;
        let caption = renderer
            .caption_format
            .clone()
            .unwrap_or_else(|| body.clone());
        let icon = renderer.icon_format.clone().unwrap_or_else(|| body.clone());
        // Dp-sized geometry: the render target scales it to the window's DPI.
        let ops = build_ops(
            96,
            &PaintInput {
                rows: &self.state.rows,
                offset: self.state.offset,
                selected: self.state.selected,
                hover: self.hover,
                footer: &self.footer_text,
                colors: &colors,
            },
        );
        unsafe {
            target.BeginDraw();
        }
        paint_ops_d2d(&target, [&body, &caption, &icon], self.dpi, &ops);

        if let Err(e) = unsafe { target.EndDraw(None, None) } {
            if e.code() == D2DERR_RECREATE_TARGET {
                renderer.drop_target();
                self.invalidate();
            }
            // Rule 7: any other `EndDraw` failure is swallowed here, not
            // panicked on -- worst case this one paint is incomplete and
            // the next `Invalidate`/`Show` tries again.
        }
        true
    }

    /// The original GDI `DrawTextW` path (module doc comment's "Rendering"
    /// section): used only as the fallback when D2D is unavailable for this
    /// window or just hit device loss. Kept byte-for-byte equivalent to
    /// what this module painted before #216 so the fallback is exactly as
    /// tested as the path it replaces, not a second, thinner
    /// implementation.
    fn paint_gdi(&self, hdc: windows::Win32::Graphics::Gdi::HDC) {
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(self.hwnd, &mut rc);
        }
        let ops = build_ops(
            self.dpi,
            &PaintInput {
                rows: &self.state.rows,
                offset: self.state.offset,
                selected: self.state.selected,
                hover: self.hover,
                footer: &self.footer_text,
                colors: &self.colors,
            },
        );
        paint_ops_gdi(hdc, self.dpi, rc, &ops);
    }

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                self.on_paint();
                Some(LRESULT(0))
            }
            // #626: `PrintWindow` and screenshot tools ask for the client
            // area in their own DC. Direct2D only draws to the window, so
            // give them the GDI paint, the same picture the fallback makes.
            WM_PRINTCLIENT => {
                if wparam.0 != 0 {
                    self.paint_gdi(windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void));
                }
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)), // we paint the whole client area ourselves
            // #579: without this the query box keeps the stock white
            // background and black text whatever the theme.
            WM_PALETTE_DRAW_CUE => {
                self.draw_query_cue(HDC(wparam.0 as *mut c_void), HWND(lparam.0 as *mut c_void));
                Some(LRESULT(0))
            }
            WM_CTLCOLOREDIT => {
                let hdc = HDC(wparam.0 as *mut c_void);
                unsafe {
                    SetTextColor(hdc, COLORREF(self.colors.text));
                    SetBkColor(hdc, COLORREF(self.colors.box_fill));
                    if self.edit_brush.0.is_null() {
                        self.edit_brush = CreateSolidBrush(COLORREF(self.colors.box_fill));
                    }
                }
                Some(LRESULT(self.edit_brush.0 as isize))
            }
            // #579: the light/dark switch, the high contrast switch and a new
            // contrast scheme or accent. Windows sends these to every top-level
            // window, hidden or not, so the palette hears them with no timer.
            WM_SETTINGCHANGE => {
                if setting_change_affects_theme(wparam.0, &unsafe { setting_change_name(lparam) }) {
                    self.refresh_theme();
                }
                Some(LRESULT(0))
            }
            WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
                self.refresh_theme();
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                let wp = wparam.0 as u32;
                let notify = (wp >> 16) & 0xFFFF;
                let ctrl_id = (wp & 0xFFFF) as i32;
                if notify == EN_CHANGE && ctrl_id == QUERY_EDIT_ID && lparam.0 != 0 {
                    if self.suppress_en_change {
                        // #414: this is `show`'s own programmatic clear, not
                        // real user typing -- `show` will call
                        // `rebuild_state` itself right after.
                        return Some(LRESULT(0));
                    }
                    let query = self.current_query();
                    self.rebuild_state(&query);
                    self.invalidate();
                    return Some(LRESULT(0));
                }
                match ctrl_id {
                    ID_PALETTE_UP => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::Up)
                    }
                    ID_PALETTE_DOWN => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::Down)
                    }
                    ID_PALETTE_PAGE_UP => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::PageUp)
                    }
                    ID_PALETTE_PAGE_DOWN => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::PageDown)
                    }
                    ID_PALETTE_ENTER => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::Enter)
                    }
                    ID_PALETTE_ESCAPE => {
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::Escape)
                    }
                    ID_PALETTE_LOST_FOCUS if !self.keep_open => self.hide(),
                    _ => {}
                }
                Some(LRESULT(0))
            }
            // #217: the mouse wheel moves the viewport only -- selection
            // (and so what Enter would run) is untouched, matching every
            // ordinary scrollable list's behaviour. `WM_MOUSEWHEEL` is
            // delivered to whichever window is under the cursor (the
            // default "scroll inactive windows" Windows setting), which for
            // the row list is this HWND directly -- no subclass forwarding
            // needed, unlike the query edit control's keys.
            WM_MOUSEWHEEL => {
                // HIWORD(wParam) is the signed wheel delta, in multiples of
                // WHEEL_DELTA (120, winuser.h) -- spelled out here the same
                // way this file already spells out EN_CHANGE.
                const WHEEL_DELTA: i32 = 120;
                let raw_delta = ((wparam.0 >> 16) & 0xFFFF) as u16 as i16;
                let notches = raw_delta as i32 / WHEEL_DELTA;
                if notches != 0 {
                    // Wheel-up (positive notches) reveals earlier rows, i.e.
                    // moves the viewport toward the top (negative delta_rows
                    // in `scroll_by`'s convention).
                    self.state.offset = crate::ui::palette_model::scroll_by(
                        self.state.offset,
                        -notches,
                        self.state.rows.len(),
                        crate::ui::palette_model::MAX_VISIBLE_ROWS,
                    );
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            // #357: hover highlight. `row_at` is purely geometric (any row
            // slot), so this also checks the row is really an `Action` --
            // headers/hints/blank space below the last row must never
            // highlight. Re-arms `TrackMouseEvent` on every move that finds
            // tracking not already armed, since `TrackMouseEvent` disarms
            // itself the moment `WM_MOUSELEAVE` fires -- a one-shot device,
            // not a subscription (no polling timer either way, rule 5: this
            // only runs in response to a real mouse message).
            WM_MOUSEMOVE | WM_MOUSELEAVE if self.ignore_mouse => Some(LRESULT(0)),
            WM_MOUSEMOVE => {
                if !self.tracking_leave {
                    let mut tme = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    if unsafe { TrackMouseEvent(&mut tme) }.is_ok() {
                        self.tracking_leave = true;
                    }
                }
                let y = mouse_y(lparam);
                let hit =
                    row_at(y, self.dpi, self.state.offset, self.state.rows.len()).filter(|&i| {
                        matches!(
                            self.state.rows.get(i),
                            Some(crate::ui::palette_model::Row::Action { .. })
                        )
                    });
                if self.hover != hit {
                    self.hover = hit;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_MOUSELEAVE => {
                self.tracking_leave = false;
                if self.hover.is_some() {
                    self.hover = None;
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            // #357: click runs the row exactly as Enter would -- reuses
            // `on_palette_key_command`'s `PaletteKey::Enter` arm (dispatch +
            // hide) after moving `selected` to the clicked row, rather than
            // duplicating that dispatch/hide logic here. Only `Action` rows
            // are clickable (a header/hint hit is a no-op, per the issue's
            // Done-when); `WM_LBUTTONDOWN` is swallowed so the popup window
            // (no `WS_TABSTOP`) doesn't do anything Win32-default with it
            // and the click is a single visible action, on release, like an
            // ordinary button.
            WM_LBUTTONDOWN => Some(LRESULT(0)),
            WM_LBUTTONUP => {
                let y = mouse_y(lparam);
                if let Some(idx) = row_at(y, self.dpi, self.state.offset, self.state.rows.len()) {
                    if matches!(
                        self.state.rows.get(idx),
                        Some(crate::ui::palette_model::Row::Action { .. })
                    ) {
                        self.state.selected = idx;
                        self.on_palette_key_command(crate::ui::palette_model::PaletteKey::Enter);
                    }
                }
                Some(LRESULT(0))
            }
            // #216: the render target's DPI must follow WM_DPICHANGED, not
            // be read once at window creation -- see the module doc
            // comment's "Per-monitor-v2 DPI" paragraph. `wParam`'s low word
            // is the new DPI (identical on x and y, winuser.h); `lParam`
            // points at Windows' suggested new window rect, the standard
            // per-monitor-v2 contract this handler honors so the window's
            // physical size and the render target's DPI change together
            // (leaving one stale relative to the other would stretch or
            // shrink the DIP-laid-out content -- see the doc comment).
            WM_DPICHANGED => {
                self.dpi = ((wparam.0 & 0xFFFF) as u32).max(1);
                let suggested = unsafe { &*(lparam.0 as *const RECT) };
                let w = (suggested.right - suggested.left).max(1);
                let h = (suggested.bottom - suggested.top).max(1);
                unsafe {
                    let _ = SetWindowPos(
                        self.hwnd,
                        None,
                        suggested.left,
                        suggested.top,
                        w,
                        h,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.rebuild_font();
                self.layout_edit_control();
                if let Some(renderer) = &mut self.d2d {
                    renderer.set_dpi(self.dpi);
                    renderer.resize(w as u32, h as u32);
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_DESTROY | WM_NCDESTROY => Some(LRESULT(0)),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------

/// UTF-16, null-terminated -- for Win32 APIs expecting a `PCWSTR`.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16, *not* null-terminated -- for [`draw_text_line_d2d`]'s
/// `ID2D1RenderTarget::DrawText`, which (like `DrawTextW`) takes an
/// explicit slice length rather than scanning for a terminator.
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// #224: `draw_text_line` used to live here as a private copy, structurally
// identical to `crate::ui::text::draw_text_line` (issue #221) -- see that
// module's doc comment for the MEASURED empty-string crash and the guard's
// reasoning, which applies unchanged to every row's text painted below
// (`Row::Action`'s `name`/`Row::Header`'s group name from a hand-editable
// `actions.toml`, none of it validated non-empty at the source). Now imported instead of
// duplicated; the regression coverage for the empty-string guard itself
// lives in `ui::text`'s own tests.
use crate::ui::text::draw_text_line;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::palette_model::PaletteAction;
    use crate::ui::palette_paint::{box_rect, glyph_rect, row_fill, row_slot, thumb_rect};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_RETURN};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, GetWindowRect, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE,
    };

    /// #1039: the GDI picture of the query box has the focus colour in the
    /// bottom corners, not only on the straight run between them. Paints the
    /// paint list into a memory bitmap and reads pixels back; the flat bar the
    /// line used to be leaves the corner square without a single focus pixel.
    #[test]
    fn the_gdi_focus_line_follows_the_box_corners() {
        use crate::ui::theme::HC_BLACK;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetPixel,
            ReleaseDC, SelectObject,
        };
        for dpi in [96u32, 144, 240] {
            let colors = palette_colors(Theme::Dark, &HC_BLACK, 0x00D4_7800);
            let ops = build_ops(
                dpi,
                &PaintInput {
                    rows: &[],
                    offset: 0,
                    selected: 0,
                    hover: None,
                    footer: "",
                    colors: &colors,
                },
            );
            let (w, h) = (scale(WINDOW_WIDTH, dpi), window_height(dpi));
            let client = RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            };
            let b = box_rect(dpi);
            let radius = crate::ui::palette_paint::px(crate::ui::theme::CONTROL_RADIUS_DP, dpi);
            let (mut corner, mut run) = (0, 0);
            unsafe {
                let screen = GetDC(None);
                let mem = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, w, h);
                let old = SelectObject(mem, bmp.into());
                paint_ops_gdi(mem, dpi, client, &ops);
                for y in (b.bottom - radius)..b.bottom {
                    for x in b.left..(b.left + radius) {
                        if GetPixel(mem, x, y).0 == colors.focus_line {
                            corner += 1;
                        }
                    }
                }
                // Rows of the focus colour up from the bottom, middle column.
                let mid = (b.left + b.right) / 2;
                while GetPixel(mem, mid, b.bottom - 1 - run).0 == colors.focus_line {
                    run += 1;
                }
                SelectObject(mem, old);
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
            }
            let width = crate::ui::palette_paint::px(2, dpi).max(1);
            assert_eq!(run, width, "dpi {dpi}: rows of focus colour at the bottom");
            assert!(corner > 0, "dpi {dpi}: no focus pixel in the corner square");
        }
    }

    fn instance() -> HINSTANCE {
        unsafe { GetModuleHandleW(None).unwrap().into() }
    }

    fn free_actions() -> Vec<PaletteAction> {
        vec![
            PaletteAction {
                id: "check-my-work".to_string(),
                name: "Check my work".to_string(),
                group: Some("Study".to_string()),
                requires_model: true,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
            PaletteAction {
                id: "extract-text-to-clipboard".to_string(),
                name: "Copy text from screen".to_string(),
                group: Some("Work".to_string()),
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            },
        ]
    }

    // Drains any messages already queued for `hwnd` (WM_COMMAND posted by
    // the subclass is delivered async, not synchronously by PostMessageW).
    fn pump_pending(hwnd: HWND) {
        unsafe {
            let mut msg = MSG::default();
            while PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }

    // #357: pure hit-test tests, written before `row_at` exists (RED first).
    // `dpi` is 96 throughout (logical == physical) so the expected pixel
    // values are the raw layout constants.
    // #476, #579: every colour the palette draws comes from the theme tokens.
    // The bar carries "this row is selected" (3:1 against what it sits on);
    // text must stay readable on every surface it is drawn on (4.5:1).
    mod colour_tests {
        use super::super::{palette_colors, PaletteColors};
        use crate::ui::theme::{contrast_ratio, relative_luminance, Theme, HC_BLACK, HC_WHITE};

        fn ratio(a: u32, b: u32) -> f64 {
            contrast_ratio(relative_luminance(a), relative_luminance(b))
        }

        /// Every theme, and accents from near-invisible to extreme, so no user
        /// accent can make the bar or the text unreadable.
        fn cases() -> Vec<(String, Theme, PaletteColors)> {
            let accents = [
                0x00D4_7800, // the Windows default blue
                0x0000_F0FF, // pale yellow
                0x0010_0808, // near black
                0x00FF_FFFF, // white
                0x0020_20C0, // red
            ];
            let mut out = Vec::new();
            for (name, theme, sys) in [
                ("light", Theme::Light, HC_BLACK),
                ("dark", Theme::Dark, HC_BLACK),
                ("hc-black", Theme::HighContrast, HC_BLACK),
                ("hc-white", Theme::HighContrast, HC_WHITE),
            ] {
                for accent in accents {
                    out.push((
                        format!("{name} accent {accent:#08x}"),
                        theme,
                        palette_colors(theme, &sys, accent),
                    ));
                }
            }
            out
        }

        #[test]
        fn every_text_colour_has_at_least_4_5_to_1_on_the_background() {
            let cases = cases();
            assert_eq!(cases.len(), 20, "the test must look at every case");
            for (name, _, c) in cases {
                for (what, fg) in [
                    ("text", c.text),
                    ("header", c.header),
                    ("hint", c.hint),
                    ("footer", c.footer),
                    ("cue", c.cue),
                ] {
                    let r = ratio(fg, c.background);
                    assert!(r >= 4.5, "{name}: {what} is {r:.2}:1 on the background");
                }
            }
        }

        #[test]
        fn row_text_has_at_least_4_5_to_1_on_the_selected_row() {
            for (name, _, c) in cases() {
                let r = ratio(c.selected_text, c.selected_fill);
                assert!(r >= 4.5, "{name}: selected text is {r:.2}:1 on its row");
            }
        }

        #[test]
        fn the_hovered_row_differs_from_the_selected_row_and_keeps_readable_text() {
            for (name, theme, c) in cases() {
                let r = ratio(c.text, c.hover_fill);
                assert!(r >= 4.5, "{name}: text is {r:.2}:1 on a hovered row");
                assert_ne!(c.hover_fill, c.selected_fill, "{name}");
                if theme == Theme::HighContrast {
                    // No fill in high contrast: an outline in the text colour.
                    assert_eq!(c.hover_fill, c.background, "{name}");
                    let o = c
                        .hover_outline
                        .expect("high contrast hovers with an outline");
                    assert!(ratio(o, c.background) >= 3.0, "{name}: outline");
                } else {
                    // A quiet fill, lighter than the selection's.
                    assert_ne!(c.hover_fill, c.background, "{name}");
                    assert_eq!(c.hover_outline, None, "{name}");
                    assert!(
                        ratio(c.hover_fill, c.background) < ratio(c.selected_fill, c.background),
                        "{name}: the hover fill is not quieter than the selection"
                    );
                }
            }
        }

        #[test]
        fn the_bar_has_at_least_3_to_1_on_the_background() {
            for (name, theme, c) in cases() {
                // In high contrast the whole row is the system highlight and
                // the bar is that colour: the row itself must stand out.
                let (bar, under) = if theme == Theme::HighContrast {
                    assert_eq!(c.bar, None, "{name}: no decorative bar in high contrast");
                    (c.selected_fill, c.background)
                } else {
                    (c.bar.expect("a bar"), c.background)
                };
                let r = ratio(bar, under);
                assert!(r >= 3.0, "{name}: the selection is {r:.2}:1");
            }
        }

        #[test]
        fn the_scroll_thumb_has_at_least_3_to_1_on_the_background() {
            for (name, _, c) in cases() {
                let r = ratio(c.thumb, c.background);
                assert!(r >= 3.0, "{name}: the thumb is {r:.2}:1");
            }
        }

        #[test]
        fn the_selected_row_is_still_distinct_from_the_background() {
            for (name, _, c) in cases() {
                assert_ne!(c.selected_fill, c.background, "{name}");
            }
        }

        #[test]
        fn light_and_dark_differ_and_high_contrast_uses_the_system_colours() {
            let light = palette_colors(Theme::Light, &HC_BLACK, 0x00D4_7800);
            let dark = palette_colors(Theme::Dark, &HC_BLACK, 0x00D4_7800);
            assert_ne!(light.background, dark.background);
            assert!(relative_luminance(light.background) > relative_luminance(dark.background));
            let hc = palette_colors(Theme::HighContrast, &HC_WHITE, 0x00D4_7800);
            assert_eq!(hc.background, HC_WHITE.window);
            assert_eq!(hc.text, HC_WHITE.window_text);
            assert_eq!(hc.selected_fill, HC_WHITE.highlight);
            assert_eq!(hc.selected_text, HC_WHITE.highlight_text);
        }
    }

    // #579: the palette is a top-level window, so Windows broadcasts every
    // theme change to it even while it is hidden.
    mod live_theme_tests {
        use super::*;
        use crate::ui::theme::HC_BLACK;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, GetBkColor, GetDC, GetTextColor, ReleaseDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::{WM_SYSCOLORCHANGE, WM_THEMECHANGED};

        fn live_colors() -> PaletteColors {
            let (theme, sys) = (detect_theme(), SysColors::read());
            palette_colors(theme, &sys, live_accent(theme, &sys))
        }

        /// Colours that can never equal the live ones, whatever theme this
        /// machine runs: the background is not a valid COLORREF (#886).
        fn stale_colors() -> PaletteColors {
            PaletteColors {
                background: 0x0100_0000,
                ..live_colors()
            }
        }

        #[test]
        fn a_theme_message_makes_the_palette_read_the_system_again() {
            for msg in [WM_THEMECHANGED, WM_SYSCOLORCHANGE] {
                let mut p = Palette::new_for_test(instance()).unwrap();
                // Stale colours, as if the theme had just changed.
                p.inner.colors = stale_colors();
                assert_ne!(p.inner.colors, live_colors(), "the setup must differ");
                unsafe {
                    SendMessageW(p.hwnd(), msg, Some(WPARAM(0)), Some(LPARAM(0)));
                }
                assert_eq!(p.inner.colors, live_colors(), "message {msg:#x}");
            }
        }

        #[test]
        fn a_theme_setting_change_makes_the_palette_read_the_system_again() {
            let mut p = Palette::new_for_test(instance()).unwrap();
            p.inner.colors = stale_colors();
            let name = wide_z("ImmersiveColorSet");
            unsafe {
                SendMessageW(
                    p.hwnd(),
                    WM_SETTINGCHANGE,
                    Some(WPARAM(0)),
                    Some(LPARAM(name.as_ptr() as isize)),
                );
            }
            assert_eq!(p.inner.colors, live_colors());
            // An unrelated setting leaves it alone.
            p.inner.colors = stale_colors();
            let other = wide_z("intl");
            unsafe {
                SendMessageW(
                    p.hwnd(),
                    WM_SETTINGCHANGE,
                    Some(WPARAM(0)),
                    Some(LPARAM(other.as_ptr() as isize)),
                );
            }
            assert_ne!(p.inner.colors, live_colors());
        }

        /// #919: the palette hides when its query box loses focus, so another
        /// window taking the foreground hid it mid-run and `--matrix` stopped.
        /// The gallery asks it to stay up; a normal palette still hides.
        #[test]
        fn a_gallery_palette_stays_up_when_focus_leaves_but_a_normal_one_hides() {
            use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;
            let lost_focus = |p: &Palette| unsafe {
                SendMessageW(
                    p.hwnd(),
                    WM_COMMAND,
                    Some(WPARAM(ID_PALETTE_LOST_FOCUS as usize)),
                    Some(LPARAM(0)),
                );
            };
            let mut normal = Palette::new_for_test(instance()).unwrap();
            normal.show(free_actions(), true, String::new());
            lost_focus(&normal);
            assert!(!unsafe { IsWindowVisible(normal.hwnd()) }.as_bool());

            let mut gallery = Palette::new_for_test(instance()).unwrap();
            gallery.keep_open_for_gallery();
            gallery.show(free_actions(), true, String::new());
            lost_focus(&gallery);
            assert!(unsafe { IsWindowVisible(gallery.hwnd()) }.as_bool());
            assert!(gallery.inner.visible);
        }

        #[test]
        fn a_forced_theme_ignores_theme_messages() {
            let mut p = Palette::new_for_test(instance()).unwrap();
            p.force_theme_for_gallery(Theme::Dark);
            let forced = p.inner.colors;
            assert_eq!(
                forced,
                palette_colors(Theme::Dark, &HC_BLACK, live_accent(Theme::Dark, &HC_BLACK))
            );
            unsafe {
                SendMessageW(p.hwnd(), WM_THEMECHANGED, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            assert_eq!(p.inner.colors, forced);
        }

        /// The query box is a child EDIT: without `WM_CTLCOLOREDIT` it keeps
        /// the stock white background and black text in a dark palette.
        #[test]
        fn the_query_box_gets_the_palette_text_and_background_colours() {
            let mut p = Palette::new_for_test(instance()).unwrap();
            p.force_theme_for_gallery(Theme::Dark);
            let (screen, hdc);
            let result;
            unsafe {
                screen = GetDC(None);
                hdc = CreateCompatibleDC(Some(screen));
                result = SendMessageW(
                    p.hwnd(),
                    WM_CTLCOLOREDIT,
                    Some(WPARAM(hdc.0 as usize)),
                    Some(LPARAM(p.edit_hwnd().0 as isize)),
                );
            }
            let colors = p.inner.colors;
            let (bk, text) = unsafe { (GetBkColor(hdc), GetTextColor(hdc)) };
            unsafe {
                let _ = DeleteDC(hdc);
                ReleaseDC(None, screen);
            }
            assert_ne!(result.0, 0, "WM_CTLCOLOREDIT must return a brush");
            assert_eq!(bk.0, colors.box_fill, "the edit sits on the box fill");
            assert_eq!(text.0, colors.text);
        }
    }
    // The placeholder is painted into the EDIT by the palette (a plain EDIT
    // has no cue banner in the palette's colours), so these print the real
    // control and look at its pixels.
    mod query_cue_tests {
        use super::*;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        /// How many pixels of the printed edit, at or right of `from_x`, are not
        /// the background colour.
        fn ink_right_of(p: &Palette, from_x: i32) -> usize {
            let edit = p.edit_hwnd();
            let mut rc = RECT::default();
            unsafe {
                let _ = GetClientRect(edit, &mut rc);
                let screen = GetDC(None);
                let dc = CreateCompatibleDC(Some(screen));
                let bmp = CreateCompatibleBitmap(screen, rc.right, rc.bottom);
                let old = SelectObject(dc, HGDIOBJ(bmp.0));
                let bg = CreateSolidBrush(COLORREF(p.inner.colors.box_fill));
                FillRect(dc, &rc, bg);
                let _ = DeleteObject(bg.into());
                // PRF_CLIENT
                SendMessageW(
                    edit,
                    WM_PRINTCLIENT,
                    Some(WPARAM(dc.0 as usize)),
                    Some(LPARAM(0x4)),
                );
                let mut n = 0;
                for y in 0..rc.bottom {
                    for x in from_x..rc.right {
                        if GetPixel(dc, x, y).0 != p.inner.colors.box_fill {
                            n += 1;
                        }
                    }
                }
                SelectObject(dc, old);
                let _ = DeleteObject(bmp.into());
                let _ = DeleteDC(dc);
                ReleaseDC(None, screen);
                n
            }
        }

        fn shown() -> Palette {
            let mut p = Palette::new_for_test(instance()).unwrap();
            p.force_theme_for_gallery(Theme::Dark);
            p.show(free_actions(), true, String::new());
            p
        }

        #[test]
        fn an_empty_query_box_shows_the_placeholder() {
            let p = shown();
            assert!(ink_right_of(&p, 0) > 0, "nothing is drawn in an empty box");
        }

        #[test]
        fn typing_removes_the_placeholder() {
            let mut p = shown();
            let from = scale(60, p.inner.dpi);
            assert!(
                ink_right_of(&p, from) > 0,
                "the placeholder is wider than 60 DIP"
            );
            p.set_query_for_test("a");
            assert_eq!(ink_right_of(&p, from), 0, "the placeholder is still drawn");
        }

        #[test]
        fn the_edit_is_one_text_line_high_and_centred_in_the_box() {
            let p = shown();
            let (mut er, mut pr) = (RECT::default(), RECT::default());
            unsafe {
                let _ = GetWindowRect(p.edit_hwnd(), &mut er);
                let _ = GetWindowRect(p.hwnd(), &mut pr);
            }
            let b = box_rect(p.inner.dpi);
            let (top, bottom) = (er.top - pr.top, er.bottom - pr.top);
            assert!(bottom - top < b.bottom - b.top, "{top}..{bottom}");
            assert!(((top - b.top) - (b.bottom - bottom)).abs() <= 1);
            // After the glyph, inside the box.
            let left = er.left - pr.left;
            assert!(left > glyph_rect(p.inner.dpi).right && left < b.right);
        }
    }

    /// #626: the UI gallery asks the window to paint into ITS device context
    /// with `WM_PRINT`, which reaches `WM_PRINTCLIENT`. The Direct2D path only
    /// ever draws to the window itself, and `PrintWindow` reads a surface it
    /// never reaches, so without an arm the picture is a black page. Sends the
    /// message into a memory bitmap and reads the pixels back.
    #[test]
    fn wm_print_paints_the_rows_into_the_given_dc() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetPixel,
            ReleaseDC, SelectObject, HDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::WM_PRINT;
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.ignore_mouse_for_test();
        p.keep_open_for_gallery();
        p.show(free_actions(), true, "mode: Auto".to_string());
        // #991: another test's window taking the foreground makes the query
        // box lose focus, which hides a normal palette and leaves WM_PRINT
        // (PRF_CHECKVISIBLE) nothing to paint. Do that here on purpose, so the
        // check no longer depends on what else is running.
        unsafe {
            SendMessageW(
                p.hwnd(),
                WM_COMMAND,
                Some(WPARAM(ID_PALETTE_LOST_FOCUS as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        let dpi = unsafe { GetDpiForWindow(p.hwnd()) };
        let (w, h) = (scale(WINDOW_WIDTH, dpi), window_height(dpi));
        let (screen, mem, bmp, old);
        unsafe {
            screen = GetDC(None);
            mem = CreateCompatibleDC(Some(screen));
            bmp = CreateCompatibleBitmap(screen, w, h);
            old = SelectObject(mem, bmp.into());
            // PRF_CHECKVISIBLE | PRF_CHILDREN | PRF_CLIENT | PRF_ERASEBKGND
            SendMessageW(
                p.hwnd(),
                WM_PRINT,
                Some(WPARAM(mem.0 as usize)),
                Some(LPARAM(0x1 | 0x4 | 0x8 | 0x10)),
            );
        }
        let pixel = |x: i32, y: i32| unsafe { GetPixel(mem as HDC, x, y) };
        // Rows start below the edit; the first action
        // row (after the group header) is the selected one.
        let fill = row_fill(dpi, 1);
        let selected = pixel(fill.right - scale(8, dpi), (fill.top + fill.bottom) / 2);
        let empty_slot = row_slot(dpi, 8);
        let empty = pixel(w / 2, (empty_slot.top + empty_slot.bottom) / 2);
        assert!(h > empty_slot.bottom);
        unsafe {
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
        assert_eq!(
            selected.0, p.inner.colors.selected_fill,
            "the selected row fill"
        );
        assert_eq!(empty.0, p.inner.colors.background, "the palette background");
    }
    /// #1007 and #987: the gallery lays the palette out at a forced scale and
    /// paints it through the real Direct2D code into a bitmap. At 150 percent
    /// the window is 1.5 times the size, and the Direct2D picture has the
    /// selected row's fill and the background where the paint list puts them.
    #[test]
    fn the_gallery_paints_the_palette_through_direct2d_at_a_forced_scale() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.force_theme_for_gallery(Theme::Dark);
        p.keep_open_for_gallery();
        p.ignore_mouse_for_test();
        p.force_dpi_for_gallery(144);
        p.show(free_actions(), true, "mode: Auto".to_string());
        let dpi = 144;
        let (w, h) = (scale(WINDOW_WIDTH, dpi), window_height(dpi));
        let mut rc = RECT::default();
        unsafe {
            let _ = GetWindowRect(p.hwnd(), &mut rc);
        }
        assert_eq!((rc.right - rc.left, rc.bottom - rc.top), (w, h));
        let fill = row_fill(dpi, 1);
        let empty_slot = row_slot(dpi, 8);
        let (selected, empty, painted);
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp.into());
            painted = p.paint_d2d_into_for_gallery(mem);
            selected = GetPixel(
                mem,
                fill.right - scale(8, dpi),
                (fill.top + fill.bottom) / 2,
            );
            empty = GetPixel(mem, w / 2, (empty_slot.top + empty_slot.bottom) / 2);
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
        painted.expect("Direct2D painted the palette");
        assert_eq!(selected.0, p.inner.colors.selected_fill, "selected row");
        assert_eq!(empty.0, p.inner.colors.background, "background");
    }

    /// The colours of the window's pixels at `points`, read from a `WM_PRINT`
    /// of the palette (the GDI path, like the UI gallery's pictures).
    fn printed_pixels(p: &Palette, points: &[(i32, i32)]) -> Vec<u32> {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::WM_PRINT;
        let (w, h) = (scale(WINDOW_WIDTH, p.inner.dpi), window_height(p.inner.dpi));
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            let old = SelectObject(mem, bmp.into());
            SendMessageW(
                p.hwnd(),
                WM_PRINT,
                Some(WPARAM(mem.0 as usize)),
                Some(LPARAM(0x1 | 0x4 | 0x8 | 0x10)),
            );
            let out = points.iter().map(|&(x, y)| GetPixel(mem, x, y).0).collect();
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
            out
        }
    }

    /// #883: a list longer than the viewport draws a thumb on the right that
    /// sits at the top, then at the bottom once scrolled to the end, and a
    /// list that fits draws none.
    #[test]
    fn a_long_list_draws_a_scroll_thumb_that_follows_the_offset() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.force_theme_for_gallery(Theme::Dark);
        p.keep_open_for_gallery();
        p.show(many_ungrouped_actions(30), true, String::new());
        let dpi = p.inner.dpi;
        let rows = p.inner.state.rows.len();
        let at = |offset: usize| thumb_rect(dpi, offset, rows).expect("30 rows overflow 12");
        let (top_rect, end_rect) = (at(0), at(rows - crate::ui::palette_model::MAX_VISIBLE_ROWS));
        let x = (top_rect.left + top_rect.right) / 2;
        let track_top = top_rect.top;
        let track_bottom = end_rect.bottom;
        let probes = [(x, track_top + 2), (x, track_bottom - 2)];
        let thumb = p.inner.colors.thumb;
        let bg = p.inner.colors.background;

        assert_eq!(printed_pixels(&p, &probes), [thumb, bg], "at the top");
        p.inner.state.offset = rows - crate::ui::palette_model::MAX_VISIBLE_ROWS;
        assert_eq!(
            printed_pixels(&p, &probes),
            [bg, thumb],
            "scrolled to the end"
        );

        p.show(many_ungrouped_actions(5), true, String::new());
        assert_eq!(
            printed_pixels(&p, &probes),
            [bg, bg],
            "a short list has no thumb"
        );
    }

    /// #882: a row the mouse is over is filled lighter than the selected row,
    /// so two rows never look selected. Prints the window and reads the
    /// pixels of the hovered row and of the selected one.
    #[test]
    fn a_hovered_row_is_filled_lighter_than_the_selected_row() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC, GetPixel,
            ReleaseDC, SelectObject, HDC,
        };
        use windows::Win32::UI::WindowsAndMessaging::WM_PRINT;
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.force_theme_for_gallery(Theme::Dark);
        p.ignore_mouse_for_test();
        p.keep_open_for_gallery();
        p.show(free_actions(), true, "mode: Auto".to_string());
        // Rows: header, action (selected), header, action.
        p.inner.hover = Some(3);
        let dpi = p.inner.dpi;
        let (w, h) = (scale(WINDOW_WIDTH, dpi), window_height(dpi));
        let (screen, mem, bmp, old);
        unsafe {
            screen = GetDC(None);
            mem = CreateCompatibleDC(Some(screen));
            bmp = CreateCompatibleBitmap(screen, w, h);
            old = SelectObject(mem, bmp.into());
            SendMessageW(
                p.hwnd(),
                WM_PRINT,
                Some(WPARAM(mem.0 as usize)),
                Some(LPARAM(0x1 | 0x4 | 0x8 | 0x10)),
            );
        }
        let row_y = |pos: i32| {
            let f = row_fill(dpi, pos);
            (f.top + f.bottom) / 2
        };
        let x = row_fill(dpi, 1).right - scale(8, dpi);
        let (selected, hovered) = unsafe {
            (
                GetPixel(mem as HDC, x, row_y(1)).0,
                GetPixel(mem as HDC, x, row_y(3)).0,
            )
        };
        unsafe {
            SelectObject(mem, old);
            let _ = DeleteObject(bmp.into());
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
        let c = p.inner.colors;
        assert_eq!(selected, c.selected_fill);
        assert_eq!(hovered, c.hover_fill, "the hovered row fill");
        assert_ne!(hovered, selected);
        assert_ne!(hovered, c.background);
    }

    #[test]
    fn window_and_edit_control_are_created() {
        let p = Palette::new_for_test(instance()).expect("palette window creation must succeed");
        assert!(!p.hwnd().0.is_null());
        assert!(!p.edit_hwnd().0.is_null());
    }

    #[test]
    fn show_makes_the_window_visible_and_hide_makes_it_not() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        assert!(!p.is_visible());
        p.show(free_actions(), true, "mode: Auto".to_string());
        assert!(p.is_visible());
        p.hide();
        assert!(!p.is_visible());
    }

    /// #395: `show()` must leave the window at the size and position
    /// `reposition_centered()` just computed for it -- not collapsed to
    /// 0x0 at the origin. Replays `reposition_centered`'s own math (monitor
    /// rect, `scale`, `window_height`) against the REAL `GetWindowRect`
    /// after a real `show()`, so a regression that moves/collapses the
    /// window after positioning it (the exact #395 bug: an unqualified
    /// `SetWindowPos(...,0,0,0,0,...)` after `reposition_centered()`) fails
    /// this test even though `is_visible()` still reports `true`.
    #[test]
    fn show_leaves_the_window_at_reposition_centereds_computed_rect() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());

        let mut rect = RECT::default();
        unsafe {
            GetWindowRect(p.hwnd(), &mut rect).expect("GetWindowRect must succeed");
        }
        let w = rect.right - rect.left;
        let h = rect.bottom - rect.top;
        assert!(
            w > 0 && h > 0,
            "palette window must have nonzero size after show(), got {w}x{h} at ({}, {})",
            rect.left,
            rect.top
        );

        let monitor =
            crate::capture::active_monitor_rect().expect("active monitor rect must be readable");
        let dpi = unsafe { GetDpiForWindow(p.hwnd()) }.max(1);
        let expected_w = scale(WINDOW_WIDTH, dpi);
        let expected_h = window_height(dpi);
        let mon_w = monitor.right - monitor.left;
        let mon_h = monitor.bottom - monitor.top;
        let expected_x = monitor.left + (mon_w - expected_w).max(0) / 2;
        let expected_y = monitor.top + (mon_h - expected_h).max(0) / 3;

        assert_eq!(
            (rect.left, rect.top, w, h),
            (expected_x, expected_y, expected_w, expected_h),
            "show() must leave the window exactly where reposition_centered() put it"
        );
    }

    #[test]
    fn empty_query_selects_the_first_action_in_grouped_order() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        // Ungrouped-first rule doesn't apply here (both are grouped); with a
        // provider configured there's no free/gated split either, so
        // catalogue order inside the first group wins: "Study" comes before
        // "Work" alphabetically is NOT guaranteed -- what IS guaranteed is
        // the first action found in `catalogue` order within its group. The
        // selection is whichever the pure model picked; just assert it's a
        // real id, not empty/None (the Win32 wiring reaches the pure model
        // at all -- see the real-Win32 test below for the full key-driven
        // path).
        assert!(p.selected_action_id().is_some());
    }

    /// #92: the line typed in the query box travels with the action id. A
    /// question matches no action name, so the action that takes text is the
    /// only row, and Enter posts it with the whole line. The box is freed by
    /// the receiver here, as `App` does.
    #[test]
    fn real_win32_enter_posts_the_typed_line_with_the_text_action() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        let owner = create_test_owner_window(inst);
        p.set_owner(owner);
        let mut actions = free_actions();
        actions.push(PaletteAction {
            id: "ask-my-documents".to_string(),
            name: "Ask my documents".to_string(),
            group: Some("Work".to_string()),
            requires_model: true,
            effect: crate::ui::palette_model::Effect::None,
            takes_text: true,
        });
        p.show(actions, true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        p.set_query_for_test("When did I start at Acme?");
        pump_pending(p.hwnd());
        // No name matched: nothing is selected, so Enter alone would send
        // nothing. Down selects the text action.
        assert_eq!(p.selected_action_id(), None);
        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_DOWN.0 as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        assert_eq!(p.selected_action_id().as_deref(), Some("ask-my-documents"));
        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_RETURN.0 as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        let mut msg = MSG::default();
        let got = unsafe { GetMessageW(&mut msg, Some(owner), 0, 0) };
        assert!(got.as_bool(), "expected WM_APP_PALETTE_RUN to be queued");
        assert_eq!(msg.message, WM_APP_PALETTE_RUN);
        let run =
            unsafe { *Box::from_raw(msg.lParam.0 as *mut crate::ui::palette_model::PaletteRun) };
        assert_eq!(run.id, "ask-my-documents");
        assert_eq!(run.line, "When did I start at Acme?");
        unsafe {
            let _ = DestroyWindow(owner);
        }
    }

    /// Real Win32 test (per the task's Done-when): creates the palette with
    /// a test-only class name, sets filter text through the real EDIT
    /// control, sends real VK_DOWN/VK_RETURN through the real subclass, and
    /// asserts the dispatched action id -- exercising `wndproc`'s
    /// `GWLP_USERDATA` routing and the subclass's `WM_COMMAND` forwarding,
    /// not just the pure model directly.
    #[test]
    fn real_win32_down_then_enter_dispatches_the_second_action() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        let owner = create_test_owner_window(inst);
        p.set_owner(owner);

        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());

        // Empty query, both actions grouped: whichever sorts first is
        // selected. Move Down once with a REAL keydown sent to the REAL
        // edit control (through its subclass), then Enter.
        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_DOWN.0 as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        let after_down = p.selected_action_id();

        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_RETURN.0 as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());

        // Enter must have posted WM_APP_PALETTE_RUN to the owner, carrying
        // the SAME id that was selected after Down.
        let mut msg = MSG::default();
        let got = unsafe { GetMessageW(&mut msg, Some(owner), 0, 0) };
        assert!(got.as_bool(), "expected WM_APP_PALETTE_RUN to be queued");
        assert_eq!(msg.message, WM_APP_PALETTE_RUN);
        let run =
            unsafe { *Box::from_raw(msg.lParam.0 as *mut crate::ui::palette_model::PaletteRun) };
        assert_eq!(Some(run.id.as_str()), after_down.as_deref());

        // Enter also hides the palette.
        assert!(!p.is_visible());

        unsafe {
            let _ = DestroyWindow(owner);
        }
    }

    /// #949: with the real pointer ignored, a mouse move over an action row
    /// does not hover it and a mouse leave does not clear an explicit hover.
    #[test]
    fn an_ignored_mouse_cannot_change_the_hovered_row() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.ignore_mouse_for_test();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let over_row_1 = LPARAM(((85u32 << 16) | 50u32) as isize);
        unsafe {
            SendMessageW(p.hwnd(), WM_MOUSEMOVE, Some(WPARAM(0)), Some(over_row_1));
        }
        assert_eq!(p.inner.hover, None, "a mouse move must not hover");
        p.set_hover_for_test(Some(3));
        unsafe {
            SendMessageW(p.hwnd(), WM_MOUSELEAVE, Some(WPARAM(0)), Some(LPARAM(0)));
        }
        assert_eq!(p.inner.hover, Some(3), "a leave must not clear it");
    }

    /// #357: real Win32 test mirroring
    /// `real_win32_down_then_enter_dispatches_the_second_action` but through
    /// the mouse -- a real `WM_MOUSEMOVE` over the second row (checked
    /// against `self.inner.hover` for the highlight), then a real
    /// `WM_LBUTTONUP` at the same point, sent straight to the palette's own
    /// `HWND` (mouse messages go to the window under the cursor directly, no
    /// subclass forwarding needed here unlike the query edit control's
    /// keys).
    #[test]
    fn real_win32_mouse_move_then_click_dispatches_the_hovered_row() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        let owner = create_test_owner_window(inst);
        p.set_owner(owner);

        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());

        // Row 1's midpoint, at the window's own scale.
        let slot = row_slot(p.inner.dpi, 1);
        let x: i16 = 50;
        let y: i16 = ((slot.top + slot.bottom) / 2) as i16;
        let lparam = LPARAM(((y as u16 as u32) << 16 | (x as u16 as u32)) as isize);

        unsafe {
            SendMessageW(p.hwnd(), WM_MOUSEMOVE, Some(WPARAM(0)), Some(lparam));
        }
        assert!(
            p.inner.hover.is_some(),
            "hovering an action row must set hover"
        );
        // The row the click below must dispatch: the SAME id the mouse-move
        // just hovered (which is also what Down-then-Enter would have
        // selected, since the hover moved `state.selected` to the same
        // geometric row hovering highlighted).
        let hovered_action_id = p.selected_action_id_at(p.inner.hover.unwrap());

        unsafe {
            SendMessageW(p.hwnd(), WM_LBUTTONUP, Some(WPARAM(0)), Some(lparam));
        }
        pump_pending(p.hwnd());

        let mut msg = MSG::default();
        let got = unsafe { GetMessageW(&mut msg, Some(owner), 0, 0) };
        assert!(
            got.as_bool(),
            "expected WM_APP_PALETTE_RUN to be queued by the click"
        );
        assert_eq!(msg.message, WM_APP_PALETTE_RUN);
        let run =
            unsafe { *Box::from_raw(msg.lParam.0 as *mut crate::ui::palette_model::PaletteRun) };
        assert_eq!(Some(run.id), hovered_action_id);

        // Click also hides the palette, same as Enter.
        assert!(!p.is_visible());

        unsafe {
            let _ = DestroyWindow(owner);
        }
    }

    #[test]
    fn escape_hides_without_posting_a_run_message() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        let owner = create_test_owner_window(inst);
        p.set_owner(owner);
        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());

        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(
                    windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE.0 as usize,
                )),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        assert!(!p.is_visible());

        let mut msg = MSG::default();
        let has_message = unsafe { PeekMessageW(&mut msg, Some(owner), 0, 0, PM_REMOVE) }.as_bool();
        assert!(!has_message, "Esc must not dispatch a run message");

        unsafe {
            let _ = DestroyWindow(owner);
        }
    }

    #[test]
    fn typing_filters_the_list_through_a_real_en_change_notification() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());

        // "copy" is a subsequence of "Copy text from screen" but not of
        // "Check my work" (which has no 'p' at all) -- matches by name,
        // since fuzzy_score ranks against PaletteAction::name, not id.
        p.set_query_for_test("copy");
        pump_pending(p.hwnd());
        assert_eq!(
            p.selected_action_id().as_deref(),
            Some("extract-text-to-clipboard")
        );
    }

    /// #414: the fix for `show`'s programmatic clear must not also swallow
    /// genuine user typing -- a real `EN_CHANGE` from `set_query_for_test`
    /// (which drives the query through the real edit control, not by poking
    /// `PaletteState` directly) must still rebuild the rows.
    #[test]
    fn real_typing_after_show_still_filters_the_rows() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        let before = p.inner.state.rows.len();
        p.set_query_for_test("copy");
        pump_pending(p.hwnd());
        assert!(
            p.inner.state.rows.len() < before,
            "a real EN_CHANGE from user typing must rebuild the rows"
        );
    }

    fn many_ungrouped_actions(n: usize) -> Vec<PaletteAction> {
        (0..n)
            .map(|i| PaletteAction {
                id: format!("action-{i}"),
                name: format!("Action {i}"),
                group: None,
                requires_model: false,
                effect: crate::ui::palette_model::Effect::None,
                takes_text: false,
            })
            .collect()
    }

    /// #217's Done-when: with a catalogue past `MAX_VISIBLE_ROWS`, real
    /// Down presses (through the real edit control and its subclass, same
    /// as `real_win32_down_then_enter_dispatches_the_second_action` above)
    /// must be able to reach a row past row 12, and the viewport must have
    /// actually scrolled to keep it visible -- not just the pure model
    /// (`palette_model`'s own viewport tests already cover that in
    /// isolation), but the real Win32 wiring from keydown to `on_paint`'s
    /// `state.offset`.
    #[test]
    fn real_win32_down_presses_scroll_the_viewport_past_row_twelve() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(many_ungrouped_actions(20), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        assert_eq!(p.state_offset(), 0);

        for _ in 0..15 {
            unsafe {
                SendMessageW(
                    p.edit_hwnd(),
                    WM_KEYDOWN,
                    Some(WPARAM(VK_DOWN.0 as usize)),
                    Some(LPARAM(0)),
                );
            }
            pump_pending(p.hwnd());
        }

        assert_eq!(p.selected_action_id().as_deref(), Some("action-15"));
        assert!(
            p.state_offset() > 0,
            "the viewport must have scrolled to keep row 15 visible"
        );
    }

    /// #217's PageUp/PageDown, through the REAL edit control and its
    /// subclass (`ID_PALETTE_PAGE_DOWN`/`ID_PALETTE_PAGE_UP`'s `WM_COMMAND`
    /// forwarding), not just the pure `handle_key` unit tests in
    /// `palette_model.rs` -- those prove the math, this proves the Win32
    /// wiring from a real VK_NEXT/VK_PRIOR keydown reaches it at all.
    #[test]
    fn real_win32_page_down_then_page_up_through_the_real_subclass() {
        const VK_NEXT: u16 = 0x22; // PageDown
        const VK_PRIOR: u16 = 0x21; // PageUp

        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(many_ungrouped_actions(20), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        assert_eq!(p.selected_action_id().as_deref(), Some("action-0"));

        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_NEXT as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        assert_eq!(p.selected_action_id().as_deref(), Some("action-12"));
        assert!(
            p.state_offset() > 0,
            "PageDown must have scrolled the viewport too"
        );

        unsafe {
            SendMessageW(
                p.edit_hwnd(),
                WM_KEYDOWN,
                Some(WPARAM(VK_PRIOR as usize)),
                Some(LPARAM(0)),
            );
        }
        pump_pending(p.hwnd());
        assert_eq!(p.selected_action_id().as_deref(), Some("action-0"));
        assert_eq!(p.state_offset(), 0);
    }

    #[test]
    fn mouse_wheel_scrolls_the_viewport_without_changing_the_selection() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(many_ungrouped_actions(20), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        let selected_before = p.selected_action_id();
        assert_eq!(p.state_offset(), 0);

        // One wheel notch down: HIWORD(wParam) = -120 (winuser.h's
        // WHEEL_DELTA, negated for "away from the user"/scroll down).
        let wparam = WPARAM(((-120i16 as u16 as u32) as usize) << 16);
        p.handle_message(WM_MOUSEWHEEL, wparam, LPARAM(0));

        assert!(p.state_offset() > 0, "wheel-down must scroll the viewport");
        assert_eq!(
            p.selected_action_id(),
            selected_before,
            "the wheel must never change the selection"
        );
    }

    fn create_test_owner_window(instance: HINSTANCE) -> HWND {
        const OWNER_CLASS: &str = "Wingman.Palette.TestOwner.9c4e2b17";
        static OWNER_INIT: Once = Once::new();
        static OWNER_OK: OnceLock<bool> = OnceLock::new();
        OWNER_INIT.call_once(|| {
            let ok = unsafe { register_class(instance, OWNER_CLASS) };
            let _ = OWNER_OK.set(ok);
        });
        assert!(OWNER_OK.get().copied().unwrap_or(false));
        let class_w = wide_z(OWNER_CLASS);
        let title = wide_z("Wingman palette test owner");
        unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(class_w.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
            .expect("test owner window creation must succeed")
        }
    }

    // -- scale / window_height (pure enough to check directly) -------------

    #[test]
    fn scale_is_identity_at_96_dpi() {
        assert_eq!(scale(100, 96), 100);
    }

    #[test]
    fn scale_grows_with_dpi() {
        assert_eq!(scale(96, 192), 192);
    }

    /// #224 smoke test: `palette.rs` now imports `crate::ui::text::draw_text_line`
    /// rather than carrying its own copy -- this proves the import actually
    /// resolves and behaves the same way at THIS call site (an empty string
    /// must not reach the real `DrawTextW` at all, or this crashes with
    /// `STATUS_ACCESS_VIOLATION`). The guard's own regression coverage
    /// (empty vs. non-empty) lives in `ui::text`'s tests; this is not a
    /// second copy of that, just confirmation the wiring here is live.
    #[test]
    fn draw_text_line_tolerates_an_empty_string() {
        unsafe {
            let hdc = windows::Win32::Graphics::Gdi::GetDC(None);
            assert!(!hdc.is_invalid(), "GetDC failed");
            let rect = RECT {
                left: 0,
                top: 0,
                right: 100,
                bottom: 20,
            };
            draw_text_line(hdc, "", rect, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
            windows::Win32::Graphics::Gdi::ReleaseDC(None, hdc);
        }
    }

    // -- the DirectWrite path is really taken (#216) -------------------------

    /// #216's "wired to nothing" observable, and the reason it needs its own
    /// test: [`PaletteInner::try_paint_d2d`] returns `false` and falls back to
    /// [`PaletteInner::paint_gdi`] whenever the Direct2D renderer is missing or
    /// a device is lost. That fallback is the right behaviour, but it means a
    /// DirectWrite path that never initializes at all looks exactly like a
    /// working one: the palette still paints, every other test still passes,
    /// and the show-latency number stays in budget because GDI was fast too.
    ///
    /// So assert the renderer is actually there after a real show, not just
    /// that painting happened. If this fails while the palette still renders,
    /// #216 has silently regressed to GDI.
    #[test]
    fn show_leaves_a_live_direct2d_renderer_not_a_silent_gdi_fallback() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).expect("palette window creation must succeed");
        p.show(
            free_actions(),
            true,
            "mode: Auto - openai:gpt-5".to_string(),
        );

        let renderer = p
            .inner
            .d2d
            .as_ref()
            .expect("PaletteRenderer::new returned None: Direct2D/DirectWrite factories                      were never created, so every paint silently falls back to GDI");
        assert!(
            renderer.target.is_some(),
            "no ID2D1HwndRenderTarget after a real show: try_paint_d2d returns false              every time and the palette is still a GDI surface"
        );
        assert!(
            renderer.text_format.is_some(),
            "no IDWriteTextFormat after a real show: rows would draw with no text"
        );
    }

    /// #361: the palette is a `WS_POPUP`, which DWM leaves square unless asked.
    #[test]
    fn the_palette_window_asks_for_rounded_corners() {
        use windows::Win32::Graphics::Dwm::{
            DwmGetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
            DWM_WINDOW_CORNER_PREFERENCE,
        };
        let p = Palette::new_for_test(instance()).unwrap();
        let mut pref = DWM_WINDOW_CORNER_PREFERENCE(0);
        unsafe {
            DwmGetWindowAttribute(
                p.hwnd(),
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &mut pref as *mut DWM_WINDOW_CORNER_PREFERENCE as *mut c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            )
            .expect("DwmGetWindowAttribute must read the corner preference back");
        }
        assert_eq!(pref, DWMWCP_ROUND);
    }

    /// #1000: the Direct2D text formats for rows and the footer trim to an
    /// ellipsis by character; the icon format does not. Observable if the
    /// trimming were wired to nothing: the granularity would read back as none.
    #[test]
    fn d2d_text_formats_trim_with_an_ellipsis_except_the_icon() {
        use windows::Win32::Graphics::DirectWrite::{
            DWRITE_TRIMMING, DWRITE_TRIMMING_GRANULARITY_NONE,
        };
        let Some(r) = PaletteRenderer::new() else {
            return; // no Direct2D here: the GDI path is the only one
        };
        for (face, want) in [
            (Face::Body, DWRITE_TRIMMING_GRANULARITY_CHARACTER),
            (Face::Caption, DWRITE_TRIMMING_GRANULARITY_CHARACTER),
            (Face::Icon, DWRITE_TRIMMING_GRANULARITY_NONE),
        ] {
            let format = r
                .build_text_format(
                    D2D_FONT_FAMILY,
                    D2D_FONT_SIZE_DIP,
                    DWRITE_TEXT_ALIGNMENT_LEADING,
                    ellipsizes(face),
                )
                .expect("a text format");
            let mut trimming = DWRITE_TRIMMING::default();
            let mut sign = None;
            unsafe { format.GetTrimming(&mut trimming, &mut sign) }.expect("GetTrimming");
            assert_eq!(trimming.granularity, want, "{face:?}");
            assert_eq!(sign.is_some(), ellipsizes(face), "{face:?}: the sign");
        }
    }
    // -- show latency (#25's Done-when: under 100 ms) -----------------------

    #[test]
    #[ignore = "manual: opens a real palette window 20 times; run with \
                `cargo test ui::palette::tests::measure_show_latency -- --ignored --nocapture`"]
    fn measure_show_latency() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).expect("palette window creation must succeed");
        let mut samples = Vec::with_capacity(20);
        for _ in 0..20 {
            p.hide();
            let start = std::time::Instant::now();
            p.show(
                free_actions(),
                true,
                "mode: Auto - openai:gpt-5".to_string(),
            );
            samples.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        let avg = samples.iter().sum::<f64>() / samples.len() as f64;
        let max = samples.iter().cloned().fold(f64::MIN, f64::max);
        println!(
            "MEASURED: Palette::show (hidden -> visible and painted) over 20 shows: \
             avg {avg:.2} ms, max {max:.2} ms, samples {samples:?}"
        );
        assert!(
            avg < 100.0,
            "#25's Done-when is under 100 ms; measured avg {avg:.2} ms"
        );
    }
}
