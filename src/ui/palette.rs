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
//! The row list, router summary line, and footer render through
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

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, D2DERR_RECREATE_TARGET, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Direct2D::Common::{
    D2D1_ALPHA_MODE_IGNORE, D2D1_COLOR_F, D2D1_PIXEL_FORMAT, D2D_RECT_F, D2D_SIZE_U,
};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory, ID2D1HwndRenderTarget, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, D2D1_HWND_RENDER_TARGET_PROPERTIES,
    D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES, D2D1_RENDER_TARGET_TYPE_DEFAULT,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_MEASURING_MODE_NATURAL, DWRITE_PARAGRAPH_ALIGNMENT_CENTER,
    DWRITE_TEXT_ALIGNMENT_LEADING, DWRITE_WORD_WRAPPING_NO_WRAP,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, EndPaint, FillRect, GetStockObject, InvalidateRect,
    SelectObject, SetBkMode, SetTextColor, UpdateWindow, DEFAULT_GUI_FONT, DT_LEFT, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, HFONT, HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
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
    WM_APP, WM_COMMAND, WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS,
    WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_SETFONT, WNDCLASSEXW, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP,
    WS_VISIBLE,
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
/// action. `lparam` is `Box::into_raw(Box::new(String))` (the selected
/// action id) -- the receiver takes ownership and must reconstruct the
/// `Box` to free it, the same boxed-payload idiom [`crate::hotkey::WM_APP_LEARNED`]
/// uses. Adding another `WM_APP_*` constant anywhere in the crate also means
/// adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue #163).
pub const WM_APP_PALETTE_RUN: u32 = WM_APP + 11;

// Logical (96 DPI) layout constants, scaled by `scale()` at paint/layout
// time.
const WINDOW_WIDTH: i32 = 560;
const EDIT_HEIGHT: i32 = 30;
const ROW_HEIGHT: i32 = 26;
const FOOTER_HEIGHT: i32 = 22;
const PADDING: i32 = 8;
/// #217: rows beyond `crate::ui::palette_model::MAX_VISIBLE_ROWS` are
/// reachable by scrolling (keyboard: Up/Down/PageUp/PageDown drag the
/// viewport via `PaletteState::offset`; mouse: `WM_MOUSEWHEEL` below) rather
/// than being truncated -- that constant is the single source of truth for
/// both the window's fixed height here and the palette-side viewport math in
/// `palette_model.rs`.
/// #24: height of the router's suggestion summary line, always reserved
/// (never grows/shrinks the window -- rule 5's "pre-created, never
/// re-created" applies to size too) between the query box and the row list;
/// blank when [`PaletteInner::router_summary`] is `None`.
const ROUTER_SUMMARY_HEIGHT: i32 = 18;

/// #359: shown in the router summary band from the moment
/// `App::maybe_start_router` actually starts a background request until the
/// result (or a failure) replaces or clears it. No em dash (rule 11).
const ROUTER_LOOKING_TEXT: &str = "Looking at your screen...";

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn window_height(dpi: u32) -> i32 {
    scale(PADDING * 2 + EDIT_HEIGHT, dpi)
        + scale(ROUTER_SUMMARY_HEIGHT, dpi)
        + scale(ROW_HEIGHT, dpi) * crate::ui::palette_model::MAX_VISIBLE_ROWS as i32
        + scale(FOOTER_HEIGHT, dpi)
}

/// #357: pure hit-test -- given a client-area `y` in physical pixels (as
/// `WM_MOUSEMOVE`/`WM_LBUTTONUP` deliver it), the current viewport `offset`
/// and the real row count, returns the real row index (into
/// `PaletteState::rows`, the same space `PaletteState::selected` lives in)
/// under that `y`, or `None` when `y` is above the list (still over the
/// query box/padding/router-summary band), below the last real row, or below
/// the whole painted viewport. Mirrors `on_paint`'s row-geometry math
/// exactly (same constants, same order of additions) so a click always lands
/// on the row it visually looks like it landed on -- see that function's `y`
/// math, which this must never drift from.
fn row_at(y: i32, dpi: u32, offset: usize, row_count: usize) -> Option<usize> {
    let pad = scale(PADDING, dpi);
    let list_top = pad + scale(EDIT_HEIGHT, dpi) + pad + scale(ROUTER_SUMMARY_HEIGHT, dpi);
    let row_h = scale(ROW_HEIGHT, dpi);
    if row_h <= 0 || y < list_top {
        return None;
    }
    let visible_pos = ((y - list_top) / row_h) as usize;
    if visible_pos >= crate::ui::palette_model::MAX_VISIBLE_ROWS {
        return None;
    }
    let idx = offset + visible_pos;
    if idx < row_count {
        Some(idx)
    } else {
        None
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
        let inner = Box::new(PaletteInner {
            hwnd: HWND(std::ptr::null_mut()),
            edit_hwnd: HWND(std::ptr::null_mut()),
            owner: None,
            dpi: 96,
            font: unsafe { HFONT(GetStockObject(DEFAULT_GUI_FONT).0) },
            d2d: None,
            catalogue: Vec::new(),
            model_configured: true,
            footer_text: String::new(),
            state: crate::ui::palette_model::PaletteState::new(Vec::new()),
            visible: false,
            router_generation: 0,
            interacted: false,
            router_summary: None,
            hover: None,
            tracking_leave: false,
            suppress_en_change: false,
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

    /// #24: the generation number stamped on THIS showing of the palette --
    /// `app.rs`'s router hook reads this right after [`Palette::show`]
    /// returns and carries it across the worker thread, so the eventual
    /// result can be checked against whatever this returns AT DELIVERY time
    /// (`crate::router::is_stale`). Bumped on every [`Palette::show`] and
    /// every hide (Esc, lost focus, a dispatched Enter, or a second
    /// `toggle_palette`), so a result from any earlier showing is always
    /// stale by the time it arrives.
    pub fn router_generation(&self) -> u64 {
        self.inner.router_generation
    }

    /// #24: applies (or silently drops) a router result against the
    /// showing identified by `generation`. See
    /// [`PaletteInner::apply_router_suggestion`] for the full decision
    /// chain (staleness, visibility, confidence vs. `threshold`, and
    /// whether the user already typed or moved the selection) -- this is
    /// pure Win32 glue over `crate::router`'s pure decisions and
    /// `palette_model::preselect_action`'s pure state mutation.
    pub fn apply_router_suggestion(
        &mut self,
        generation: u64,
        result: &crate::router::RouterResult,
        threshold: f64,
    ) {
        self.inner
            .apply_router_suggestion(generation, result, threshold);
    }

    /// #359: see [`PaletteInner::set_router_pending`].
    pub fn set_router_pending(&mut self, generation: u64) {
        self.inner.set_router_pending(generation);
    }

    /// #359: see [`PaletteInner::clear_router_pending`].
    pub fn clear_router_pending(&mut self, generation: u64) {
        self.inner.clear_router_pending(generation);
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

    #[cfg(any(test, debug_assertions))]
    pub(crate) fn set_query_for_test(&mut self, query: &str) {
        self.inner.set_query(query);
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
    let pad = scale(PADDING, dpi);
    let rect = RECT {
        left: pad,
        top: pad,
        right: scale(WINDOW_WIDTH, dpi) - pad,
        bottom: pad + scale(EDIT_HEIGHT, dpi),
    };
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
            self.text_format = self.build_text_format();
        }
        self.target.is_some() && self.text_format.is_some()
    }

    fn build_text_format(&self) -> Option<IDWriteTextFormat> {
        let family = wide_z(D2D_FONT_FAMILY);
        let locale = wide_z(D2D_LOCALE);
        let format = unsafe {
            self.dwrite_factory.CreateTextFormat(
                PCWSTR(family.as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                D2D_FONT_SIZE_DIP,
                PCWSTR(locale.as_ptr()),
            )
        }
        .ok()?;
        unsafe {
            // DT_LEFT | DT_VCENTER | DT_SINGLELINE's DirectWrite equivalent:
            // leading (left) horizontal alignment, centered vertically
            // within the layout box, no wrapping (every row is one line).
            let _ = format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING);
            let _ = format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
            let _ = format.SetWordWrapping(DWRITE_WORD_WRAPPING_NO_WRAP);
        }
        Some(format)
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

/// D2D/DirectWrite equivalent of `draw_text_line` (imported from
/// `crate::ui::text` for the GDI fallback path below): draws one line of
/// `text` in `rect` (logical/DIP coordinates -- see the module doc comment)
/// with `color`, or does nothing at all when `text` is empty. Unlike the GDI
/// version, the empty-string check here is a plain optimization (skip a
/// wasted brush + draw call), not a crash guard -- `IDWriteFactory`/
/// `ID2D1RenderTarget` do not share `DrawTextW`'s zero-length-buffer bug.
fn draw_text_line_d2d(
    target: &ID2D1HwndRenderTarget,
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
    /// #24: bumped on every [`PaletteInner::show`] AND every
    /// [`PaletteInner::hide`] -- see [`Palette::router_generation`]'s doc
    /// comment for the full staleness story. `u64::wrapping_add` because
    /// this is a long-running tray app's counter, not a value that should
    /// ever panic on overflow (which would take longer than the app's
    /// uptime to reach in practice regardless).
    router_generation: u64,
    /// #24: whether the user has typed into the query box or moved the
    /// selection (Up/Down/PageUp/PageDown) since THIS showing -- reset by
    /// [`PaletteInner::show`], set by the `EN_CHANGE`/movement handlers
    /// below. `crate::router::should_apply` reads this to decide whether a
    /// late-arriving suggestion may still take the selection.
    interacted: bool,
    /// #24: the router's one-line summary, shown as a subtle line between
    /// the query box and the row list once a suggestion is applied (see
    /// `on_paint`). `None` most of the time -- no router result yet, the
    /// result didn't clear the threshold, or the user already interacted.
    router_summary: Option<String>,
    /// #357: the row currently under the mouse cursor (real index into
    /// `state.rows`, same space as `state.selected`), or `None` when the
    /// mouse isn't over the list, isn't over an `Action` row, or hasn't
    /// moved into the window since it was last shown. Separate from
    /// `state.selected` on purpose: moving the mouse off the list must not
    /// forget the keyboard/last-click selection Enter would still run, only
    /// clear the hover highlight (see `WM_MOUSELEAVE` below).
    hover: Option<usize>,
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
    /// treats the notification as the programmatic clear it is: it skips
    /// marking `interacted`/clearing `router_summary` (both already just set
    /// by `show`) and skips the redundant `rebuild_state` (`show` already
    /// calls it with the real, empty query right after).
    suppress_en_change: bool,
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
        // #24: a fresh showing starts a fresh router session -- any result
        // still in flight for the PREVIOUS showing (this bump changes what
        // `router_generation()` returns) is now stale, and the user hasn't
        // interacted with this one yet.
        self.router_generation = self.router_generation.wrapping_add(1);
        self.interacted = false;
        self.router_summary = None;
        self.hover = None;
        self.tracking_leave = false;
        // #414: SetWindowTextW below fires a real, synchronous EN_CHANGE for
        // this edit control -- see `suppress_en_change`'s doc comment. Armed
        // only around this one call, so a genuine keystroke that lands
        // between `show` calls (impossible: this is all synchronous) or any
        // later real typing still sets `interacted` normally.
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
        // #24: invalidates any router request still in flight for this
        // showing -- see `router_generation`'s doc comment. Cheap and always
        // safe to do even when nothing was in flight.
        self.router_generation = self.router_generation.wrapping_add(1);
        self.router_summary = None;
        self.hover = None;
        self.tracking_leave = false;
    }

    /// #24: see [`Palette::apply_router_suggestion`]'s doc comment for the
    /// public entry point. Order matters: staleness and visibility are
    /// checked before touching `crate::router::should_apply` at all, since
    /// neither needs the confidence/threshold/interacted comparison to
    /// already say no.
    fn apply_router_suggestion(
        &mut self,
        generation: u64,
        result: &crate::router::RouterResult,
        threshold: f64,
    ) {
        if crate::router::is_stale(generation, self.router_generation) {
            return;
        }
        if !self.visible {
            return;
        }
        // #359: whatever happens from here, the "Looking at your screen..."
        // placeholder this session's request started with must not survive
        // the result -- either it becomes the real summary below, or this
        // clears it back to None.
        let Some(intent_id) = result.intent.as_deref() else {
            self.router_summary = None;
            self.invalidate();
            return;
        };
        if !crate::router::should_apply(
            Some(intent_id),
            result.confidence,
            threshold,
            self.interacted,
        ) {
            self.router_summary = None;
            self.invalidate();
            return;
        }
        if crate::ui::palette_model::preselect_action(&mut self.state, intent_id) {
            self.router_summary = Some(result.summary.clone());
        } else {
            self.router_summary = None;
        }
        self.invalidate();
    }

    /// #359: called right when [`App::maybe_start_router`] actually starts a
    /// background request (after the Paused/no-provider/capture-failure
    /// checks all pass), so the reserved summary band shows something is
    /// happening instead of sitting blank until the result arrives. Cleared
    /// the same way a real summary is: by
    /// [`PaletteInner::apply_router_suggestion`] on arrival,
    /// [`PaletteInner::clear_router_pending`] on failure, or any of the
    /// existing "user already interacted" paths above (`show`, `hide`,
    /// `on_palette_key_command`'s Up/Down/PageUp/PageDown arm, `EN_CHANGE`).
    /// No timer involved (rule 5): this only ever changes on those events.
    fn set_router_pending(&mut self, generation: u64) {
        if crate::router::is_stale(generation, self.router_generation) {
            return;
        }
        if !self.visible {
            return;
        }
        self.router_summary = Some(ROUTER_LOOKING_TEXT.to_string());
        self.invalidate();
    }

    /// #359: the router's background request failed (or no provider ended up
    /// ready by the time the worker thread ran) -- clears whatever
    /// [`PaletteInner::set_router_pending`] showed rather than leaving it
    /// stuck. A no-op if the user already interacted or hid the palette,
    /// both of which already cleared it.
    fn clear_router_pending(&mut self, generation: u64) {
        if crate::router::is_stale(generation, self.router_generation) {
            return;
        }
        if !self.visible {
            return;
        }
        if self.router_summary.is_some() {
            self.router_summary = None;
            self.invalidate();
        }
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
        self.layout_edit_control(w);
        // #216: the render target's pixel buffer must match the window's
        // real physical size -- resized here (never recreated) alongside
        // every window resize, mirroring the window/edit-control resize
        // right above rather than waiting for a separate WM_SIZE.
        if let Some(renderer) = &mut self.d2d {
            renderer.resize(w.max(1) as u32, h.max(1) as u32);
        }
    }

    /// Positions the query `EDIT` control within a window of logical width
    /// `w` (physical pixels, already DPI-scaled by the caller) -- factored
    /// out of [`PaletteInner::reposition_centered`] so `WM_DPICHANGED`'s
    /// handler can re-run the same layout after a DPI change without
    /// re-centering the window on the active monitor too.
    fn layout_edit_control(&self, w: i32) {
        let pad = scale(PADDING, self.dpi);
        unsafe {
            let _ = SetWindowPos(
                self.edit_hwnd,
                None,
                pad,
                pad,
                w - pad * 2,
                scale(EDIT_HEIGHT, self.dpi),
                windows::Win32::UI::WindowsAndMessaging::SWP_NOZORDER,
            );
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

    /// Takes the key-forwarded `WM_COMMAND` id, runs it through the pure
    /// state machine, and acts on the outcome: repaint, dispatch, or hide.
    fn on_palette_key_command(&mut self, key: crate::ui::palette_model::PaletteKey) {
        // #24: moving the selection counts as "the user already chose" --
        // `crate::router::should_apply` must never yank the selection back
        // to a router suggestion after this. Also drops any summary line
        // already shown, since it describes a selection the user just left.
        if matches!(
            key,
            crate::ui::palette_model::PaletteKey::Up
                | crate::ui::palette_model::PaletteKey::Down
                | crate::ui::palette_model::PaletteKey::PageUp
                | crate::ui::palette_model::PaletteKey::PageDown
        ) {
            self.interacted = true;
            self.router_summary = None;
        }
        match crate::ui::palette_model::handle_key(&mut self.state, key) {
            crate::ui::palette_model::PaletteOutcome::None => self.invalidate(),
            crate::ui::palette_model::PaletteOutcome::Hide => self.hide(),
            crate::ui::palette_model::PaletteOutcome::Run(action_id) => {
                if let Some(owner) = self.owner {
                    let boxed = Box::into_raw(Box::new(action_id));
                    unsafe {
                        let _ = PostMessageW(
                            Some(owner),
                            WM_APP_PALETTE_RUN,
                            WPARAM(0),
                            LPARAM(boxed as isize),
                        );
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

        let pad = PADDING as f32;
        let row_h = ROW_HEIGHT as f32;
        let content_w = WINDOW_WIDTH as f32;
        let content_h = window_height(96) as f32; // logical/DIP total height -- see the module doc comment
        let mut y = pad + EDIT_HEIGHT as f32 + pad;

        unsafe {
            target.BeginDraw();
            target.Clear(Some(&colorref_to_d2d(COLORREF(0x0026_2626)) as *const _));
        }

        // #24: the router summary band is always reserved (see
        // ROUTER_SUMMARY_HEIGHT's doc comment) so the window never
        // resizes; only its text is conditional.
        if let Some(summary) = &self.router_summary {
            let band_rect = D2D_RECT_F {
                left: pad,
                top: y,
                right: content_w - pad,
                bottom: y + ROUTER_SUMMARY_HEIGHT as f32,
            };
            draw_text_line_d2d(&target, &format, summary, band_rect, COLORREF(0x0080_B080));
        }
        y += ROUTER_SUMMARY_HEIGHT as f32;

        // #217: only the rows inside [offset, offset + MAX_VISIBLE_ROWS) are
        // painted; `i` stays the row's real index into `state.rows` (what
        // `state.selected` compares against for the highlight), while
        // `visible_pos` (i - offset) is what actually places it vertically.
        for (i, row) in self
            .state
            .rows
            .iter()
            .enumerate()
            .skip(self.state.offset)
            .take(crate::ui::palette_model::MAX_VISIBLE_ROWS)
        {
            let visible_pos = (i - self.state.offset) as f32;
            let row_rect = D2D_RECT_F {
                left: pad,
                top: y + row_h * visible_pos,
                right: content_w - pad,
                bottom: y + row_h * (visible_pos + 1.0),
            };
            match row {
                crate::ui::palette_model::Row::Header(name) => {
                    draw_text_line_d2d(&target, &format, name, row_rect, COLORREF(0x0090_9090));
                }
                crate::ui::palette_model::Row::Hint(text) => {
                    draw_text_line_d2d(&target, &format, text, row_rect, COLORREF(0x0080_8080));
                }
                crate::ui::palette_model::Row::Action { name, .. } => {
                    if i == self.state.selected || self.hover == Some(i) {
                        if let Ok(hl) = unsafe {
                            target.CreateSolidColorBrush(
                                &colorref_to_d2d(COLORREF(0x0045_3A2E)) as *const _,
                                None,
                            )
                        } {
                            unsafe {
                                target.FillRectangle(&row_rect as *const _, &hl);
                            }
                        }
                    }
                    draw_text_line_d2d(&target, &format, name, row_rect, COLORREF(0x00E6_E6E6));
                }
            }
        }

        let footer_rect = D2D_RECT_F {
            left: pad,
            top: content_h - FOOTER_HEIGHT as f32,
            right: content_w - pad,
            bottom: content_h,
        };
        draw_text_line_d2d(
            &target,
            &format,
            &self.footer_text,
            footer_rect,
            COLORREF(0x0080_8080),
        );

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
        unsafe {
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);

            let bg = CreateSolidBrush(COLORREF(0x0026_2626));
            FillRect(hdc, &rc, bg);
            let _ = DeleteObject(bg.into());

            SetBkMode(hdc, TRANSPARENT);
            let old_font = SelectObject(hdc, HGDIOBJ(self.font.0));

            let pad = scale(PADDING, self.dpi);
            let row_h = scale(ROW_HEIGHT, self.dpi);
            let mut y = pad + scale(EDIT_HEIGHT, self.dpi) + pad;

            // #24: the router summary band is always reserved (see
            // ROUTER_SUMMARY_HEIGHT's doc comment) so the window never
            // resizes; only its text is conditional.
            if let Some(summary) = &self.router_summary {
                let band_rect = RECT {
                    left: pad,
                    top: y,
                    right: rc.right - pad,
                    bottom: y + scale(ROUTER_SUMMARY_HEIGHT, self.dpi),
                };
                SetTextColor(hdc, COLORREF(0x0080_B080));
                draw_text_line(
                    hdc,
                    summary,
                    band_rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
            y += scale(ROUTER_SUMMARY_HEIGHT, self.dpi);

            // #217: only the rows inside [offset, offset + MAX_VISIBLE_ROWS)
            // are painted; `i` stays the row's real index into `state.rows`
            // (what `state.selected` compares against for the highlight),
            // while `visible_pos` (i - offset) is what actually places it
            // vertically.
            for (i, row) in self
                .state
                .rows
                .iter()
                .enumerate()
                .skip(self.state.offset)
                .take(crate::ui::palette_model::MAX_VISIBLE_ROWS)
            {
                let visible_pos = (i - self.state.offset) as i32;
                let row_rect = RECT {
                    left: pad,
                    top: y + row_h * visible_pos,
                    right: rc.right - pad,
                    bottom: y + row_h * (visible_pos + 1),
                };
                match row {
                    crate::ui::palette_model::Row::Header(name) => {
                        SetTextColor(hdc, COLORREF(0x0090_9090));
                        draw_text_line(
                            hdc,
                            name,
                            row_rect,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                    crate::ui::palette_model::Row::Hint(text) => {
                        SetTextColor(hdc, COLORREF(0x0080_8080));
                        draw_text_line(
                            hdc,
                            text,
                            row_rect,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                    crate::ui::palette_model::Row::Action { name, .. } => {
                        if i == self.state.selected || self.hover == Some(i) {
                            let hl = CreateSolidBrush(COLORREF(0x0045_3A2E));
                            FillRect(hdc, &row_rect, hl);
                            let _ = DeleteObject(hl.into());
                        }
                        SetTextColor(hdc, COLORREF(0x00E6_E6E6));
                        draw_text_line(
                            hdc,
                            name,
                            row_rect,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                }
            }

            let footer_rect = RECT {
                left: pad,
                top: rc.bottom - scale(FOOTER_HEIGHT, self.dpi),
                right: rc.right - pad,
                bottom: rc.bottom,
            };
            SetTextColor(hdc, COLORREF(0x0080_8080));
            draw_text_line(
                hdc,
                &self.footer_text,
                footer_rect,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            SelectObject(hdc, old_font);
        }
    }

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                self.on_paint();
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)), // we paint the whole client area ourselves
            WM_COMMAND => {
                let wp = wparam.0 as u32;
                let notify = (wp >> 16) & 0xFFFF;
                let ctrl_id = (wp & 0xFFFF) as i32;
                if notify == EN_CHANGE && ctrl_id == QUERY_EDIT_ID && lparam.0 != 0 {
                    if self.suppress_en_change {
                        // #414: this is `show`'s own programmatic clear, not
                        // real user typing -- `show` already reset
                        // `interacted`/`router_summary` and will call
                        // `rebuild_state` itself right after.
                        return Some(LRESULT(0));
                    }
                    // #24: real user typing -- the router's suggestion must
                    // not steal the selection back from here on for this
                    // showing.
                    self.interacted = true;
                    self.router_summary = None;
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
                    ID_PALETTE_LOST_FOCUS => self.hide(),
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
                self.layout_edit_control(w);
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
// (`router_summary` from a live model's JSON response, `Row::Action`'s
// `name`/`Row::Header`'s group name from a hand-editable `actions.toml`,
// none of it validated non-empty at the source). Now imported instead of
// duplicated; the regression coverage for the empty-string guard itself
// lives in `ui::text`'s own tests.
use crate::ui::text::draw_text_line;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::palette_model::PaletteAction;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_RETURN};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, GetWindowRect, PeekMessageW, TranslateMessage, MSG,
        PM_REMOVE,
    };

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
            },
            PaletteAction {
                id: "extract-text-to-clipboard".to_string(),
                name: "Copy text from screen".to_string(),
                group: Some("Work".to_string()),
                requires_model: false,
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
    mod row_at_tests {
        use super::super::row_at;

        const DPI: u32 = 96;
        // list_top = PADDING + EDIT_HEIGHT + PADDING + ROUTER_SUMMARY_HEIGHT
        //          = 8 + 30 + 8 + 18 = 64
        const LIST_TOP: i32 = 64;
        const ROW_H: i32 = 26;

        #[test]
        fn y_in_the_header_padding_above_the_list_is_none() {
            assert_eq!(row_at(0, DPI, 0, 10), None);
            assert_eq!(row_at(LIST_TOP - 1, DPI, 0, 10), None);
        }

        #[test]
        fn y_at_the_top_of_the_first_row_is_row_zero() {
            assert_eq!(row_at(LIST_TOP, DPI, 0, 10), Some(0));
        }

        #[test]
        fn y_between_rows_lands_on_the_row_it_falls_in() {
            // Middle of row 2 (index 2): list_top + 2*ROW_H + ROW_H/2.
            let y = LIST_TOP + 2 * ROW_H + ROW_H / 2;
            assert_eq!(row_at(y, DPI, 0, 10), Some(2));
            // Exactly on the boundary between row 2 and row 3 -> row 3.
            let boundary = LIST_TOP + 3 * ROW_H;
            assert_eq!(row_at(boundary, DPI, 0, 10), Some(3));
        }

        #[test]
        fn y_below_the_last_real_row_is_none() {
            // Only 2 rows exist; the geometric slot for row 2 is still inside
            // the painted viewport but there is no such row.
            let y = LIST_TOP + 2 * ROW_H + 1;
            assert_eq!(row_at(y, DPI, 0, 2), None);
        }

        #[test]
        fn y_below_the_whole_viewport_is_none() {
            let y = LIST_TOP + ROW_H * crate::ui::palette_model::MAX_VISIBLE_ROWS as i32 + 5;
            assert_eq!(row_at(y, DPI, 0, 100), None);
        }

        #[test]
        fn scrolled_offset_shifts_the_returned_index() {
            // With offset 5, the row painted at visible position 0 is real
            // index 5.
            assert_eq!(row_at(LIST_TOP, DPI, 5, 20), Some(5));
            let y = LIST_TOP + 2 * ROW_H + 1;
            assert_eq!(row_at(y, DPI, 5, 20), Some(7));
        }
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
        let action_id = unsafe { *Box::from_raw(msg.lParam.0 as *mut String) };
        assert_eq!(Some(action_id.as_str()), after_down.as_deref());

        // Enter also hides the palette.
        assert!(!p.is_visible());

        unsafe {
            let _ = DestroyWindow(owner);
        }
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

        // list_top (96 DPI) = PADDING + EDIT_HEIGHT + PADDING +
        // ROUTER_SUMMARY_HEIGHT = 8 + 30 + 8 + 18 = 64; row 1's midpoint is
        // list_top + ROW_HEIGHT + ROW_HEIGHT/2 = 64 + 26 + 13 = 103.
        let x: i16 = 50;
        let y: i16 = 103;
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
        let action_id = unsafe { *Box::from_raw(msg.lParam.0 as *mut String) };
        assert_eq!(Some(action_id), hovered_action_id);

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
    /// `PaletteState` directly) must still mark `interacted`.
    #[test]
    fn real_typing_after_show_still_marks_interacted() {
        let inst = instance();
        let mut p = Palette::new_for_test(inst).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        pump_pending(p.hwnd());
        assert!(
            !p.inner.interacted,
            "show() itself must not mark interacted"
        );
        p.set_query_for_test("copy");
        pump_pending(p.hwnd());
        assert!(
            p.inner.interacted,
            "a real EN_CHANGE from user typing must mark interacted"
        );
    }

    fn many_ungrouped_actions(n: usize) -> Vec<PaletteAction> {
        (0..n)
            .map(|i| PaletteAction {
                id: format!("action-{i}"),
                name: format!("Action {i}"),
                group: None,
                requires_model: false,
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

    // -- #359: "Looking at your screen..." while the router works ----------

    fn router_result(intent: Option<&str>, confidence: f64) -> crate::router::RouterResult {
        crate::router::RouterResult {
            summary: "Check my work".to_string(),
            intent: intent.map(|s| s.to_string()),
            confidence,
        }
    }

    #[test]
    fn set_router_pending_shows_the_looking_text() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        assert_eq!(p.inner.router_summary, None);
        p.set_router_pending(generation);
        assert_eq!(p.inner.router_summary.as_deref(), Some(ROUTER_LOOKING_TEXT));
    }

    #[test]
    fn set_router_pending_is_a_noop_for_a_stale_generation() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let stale_generation = p.router_generation().wrapping_sub(1);
        p.set_router_pending(stale_generation);
        assert_eq!(p.inner.router_summary, None);
    }

    /// #414: `show()`'s own programmatic `SetWindowTextW(edit_hwnd, "")`
    /// clear must never look like real user input -- if it does, `interacted`
    /// is `true` immediately after every `show()`, and
    /// `crate::router::should_apply` (which refuses once `interacted`) means
    /// the router's preselect (#24) can never apply in the real app. This
    /// calls only `show()` then `apply_router_suggestion`, with no manual
    /// `interacted` reset, unlike the tests below.
    #[test]
    fn show_does_not_mark_interacted_so_a_router_suggestion_still_applies() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.apply_router_suggestion(generation, &router_result(Some("check-my-work"), 0.9), 0.5);
        assert_eq!(
            p.inner.router_summary.as_deref(),
            Some("Check my work"),
            "show()'s own programmatic text clear must not mark interacted, \
             or a genuine above-threshold router suggestion can never apply"
        );
    }

    #[test]
    fn apply_router_suggestion_replaces_pending_text_with_the_real_summary() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.set_router_pending(generation);
        p.apply_router_suggestion(generation, &router_result(Some("check-my-work"), 0.9), 0.5);
        assert_eq!(
            p.inner.router_summary.as_deref(),
            Some("Check my work"),
            "a successful, above-threshold result must replace the placeholder"
        );
    }

    #[test]
    fn apply_router_suggestion_below_threshold_clears_the_pending_text() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.set_router_pending(generation);
        // Confidence below the threshold: should_apply is false, so the
        // placeholder must be cleared rather than left stuck.
        p.apply_router_suggestion(generation, &router_result(Some("check-my-work"), 0.1), 0.5);
        assert_eq!(p.inner.router_summary, None);
    }

    #[test]
    fn apply_router_suggestion_with_no_intent_clears_the_pending_text() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.set_router_pending(generation);
        p.apply_router_suggestion(generation, &router_result(None, 0.9), 0.5);
        assert_eq!(p.inner.router_summary, None);
    }

    #[test]
    fn clear_router_pending_clears_the_looking_text_on_failure() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.set_router_pending(generation);
        p.clear_router_pending(generation);
        assert_eq!(p.inner.router_summary, None);
    }

    #[test]
    fn clear_router_pending_is_a_noop_for_a_stale_generation() {
        let mut p = Palette::new_for_test(instance()).unwrap();
        p.show(free_actions(), true, "mode: Auto".to_string());
        let generation = p.router_generation();
        p.set_router_pending(generation);
        // A new showing bumps the generation; the old request's eventual
        // failure must not clear the NEW showing's (currently empty) band.
        p.show(free_actions(), true, "mode: Auto".to_string());
        p.clear_router_pending(generation);
        assert_eq!(p.inner.router_summary, None);
    }
}
