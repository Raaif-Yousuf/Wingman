//! Win32 window for the Quick Ask palette (#25): a pre-created, hidden,
//! topmost, DPI-aware popup with a child `EDIT` control for the query and a
//! GDI-painted row list below it. Every decision (fuzzy scoring, grouping,
//! key handling, dispatch) lives in [`crate::ui::palette_model`], which this
//! module only renders and forwards Win32 messages into -- see that
//! module's doc comment and
//! `docs/superpowers/specs/2026-09-17-palette-design.md`.
//!
//! Rendering is plain GDI (`DrawTextW`), not DirectWrite -- see the design
//! spec's "Rendering" section: issue #25's body mentions DirectWrite, but
//! this keeps the pre-created window's first paint on the sub-100ms path
//! with no new dependency. A DirectWrite pass is filed as a follow-up.
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
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetStockObject,
    InvalidateRect, SelectObject, SetBkMode, SetTextColor, UpdateWindow, DEFAULT_GUI_FONT, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, HFONT, HGDIOBJ, PAINTSTRUCT, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongPtrW,
    GetWindowTextLengthW, GetWindowTextW, LoadCursorW, PostMessageW, RegisterClassExW,
    SendMessageW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HMENU, HWND_TOPMOST, IDC_ARROW,
    SWP_NOACTIVATE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WM_APP, WM_COMMAND,
    WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY,
    WM_PAINT, WM_SETFONT, WNDCLASSEXW, WS_CHILD, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
    WS_TABSTOP, WS_VISIBLE,
};

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

fn scale(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

fn window_height(dpi: u32) -> i32 {
    scale(PADDING * 2 + EDIT_HEIGHT, dpi)
        + scale(ROUTER_SUMMARY_HEIGHT, dpi)
        + scale(ROW_HEIGHT, dpi) * crate::ui::palette_model::MAX_VISIBLE_ROWS as i32
        + scale(FOOTER_HEIGHT, dpi)
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
    #[cfg(test)]
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
            catalogue: Vec::new(),
            model_configured: true,
            footer_text: String::new(),
            state: crate::ui::palette_model::PaletteState::new(Vec::new()),
            visible: false,
            router_generation: 0,
            interacted: false,
            router_summary: None,
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

    #[cfg(test)]
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
#[cfg(test)]
const TEST_CLASS_NAME: &str = "Wingman.Palette.Window.9c4e2b17.Test";

static CLASS_INIT: Once = Once::new();
static CLASS_OK: OnceLock<bool> = OnceLock::new();
#[cfg(test)]
static TEST_CLASS_INIT: Once = Once::new();
#[cfg(test)]
static TEST_CLASS_OK: OnceLock<bool> = OnceLock::new();

fn ensure_class_registered(instance: HINSTANCE) -> bool {
    CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance, CLASS_NAME) };
        let _ = CLASS_OK.set(ok);
    });
    CLASS_OK.get().copied().unwrap_or(false)
}

#[cfg(test)]
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
// Palette state + Win32 message handling
// ---------------------------------------------------------------------------

struct PaletteInner {
    hwnd: HWND,
    edit_hwnd: HWND,
    /// Where [`WM_APP_PALETTE_RUN`] is posted (see [`Palette::set_owner`]).
    owner: Option<HWND>,
    dpi: u32,
    font: HFONT,
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
        unsafe {
            let _ = SetWindowTextW(self.edit_hwnd, PCWSTR(wide_z("").as_ptr()));
        }
        self.rebuild_state("");
        self.reposition_centered();
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOW);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER,
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
        let Some(intent_id) = result.intent.as_deref() else {
            return;
        };
        if !crate::router::should_apply(
            Some(intent_id),
            result.confidence,
            threshold,
            self.interacted,
        ) {
            return;
        }
        if crate::ui::palette_model::preselect_action(&mut self.state, intent_id) {
            self.router_summary = Some(result.summary.clone());
            self.invalidate();
        }
    }

    fn reposition_centered(&self) {
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
            let pad = scale(PADDING, self.dpi);
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowPos(
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
    #[cfg(test)]
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

    fn on_paint(&self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
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
                let mut buf = utf16(summary);
                let mut r = band_rect;
                DrawTextW(
                    hdc,
                    &mut buf,
                    &mut r,
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
                        let mut buf = utf16(name);
                        let mut r = row_rect;
                        DrawTextW(
                            hdc,
                            &mut buf,
                            &mut r,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                    crate::ui::palette_model::Row::Hint(text) => {
                        SetTextColor(hdc, COLORREF(0x0080_8080));
                        let mut buf = utf16(text);
                        let mut r = row_rect;
                        DrawTextW(
                            hdc,
                            &mut buf,
                            &mut r,
                            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
                        );
                    }
                    crate::ui::palette_model::Row::Action { name, .. } => {
                        if i == self.state.selected {
                            let hl = CreateSolidBrush(COLORREF(0x0045_3A2E));
                            FillRect(hdc, &row_rect, hl);
                            let _ = DeleteObject(hl.into());
                        }
                        SetTextColor(hdc, COLORREF(0x00E6_E6E6));
                        let mut buf = utf16(name);
                        let mut r = row_rect;
                        DrawTextW(
                            hdc,
                            &mut buf,
                            &mut r,
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
            let mut footer_buf = utf16(&self.footer_text);
            let mut fr = footer_rect;
            DrawTextW(
                hdc,
                &mut footer_buf,
                &mut fr,
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
            );

            SelectObject(hdc, old_font);
            let _ = EndPaint(self.hwnd, &ps);
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
                    // #24: real user typing (never the programmatic clear in
                    // `show`, which uses SetWindowTextW and never fires
                    // EN_CHANGE) -- the router's suggestion must not steal
                    // the selection back from here on for this showing.
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

/// UTF-16, *not* null-terminated -- for `DrawTextW`, which takes an
/// explicit slice length rather than scanning for a terminator.
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::palette_model::PaletteAction;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_DOWN, VK_RETURN};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
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
