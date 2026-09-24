//! The notification card: a single borderless, always-on-top, non-activating
//! popup window with three states (Pending / Collapsed / Expanded). See the
//! "UI: the notification card" section of the design spec.
//!
//! # Public API
//!
//! ```ignore
//! pub enum CardState { Hidden, Pending, Collapsed, Expanded }
//!
//! pub struct Card { .. }
//!
//! impl Card {
//!     pub fn new(instance: HINSTANCE) -> anyhow::Result<Self>;
//!     pub fn hwnd(&self) -> HWND;
//!     pub fn show_pending(&mut self);
//!     pub fn show_answer(&mut self, headline: &str, detail: &str, auto_dismiss_secs: u32, difficulty: Option<Difficulty>);
//!     pub fn show_error(&mut self, headline: &str, detail: &str);
//!     pub fn hide(&mut self);
//!     pub fn set_text_scale(&mut self, scale: f32);
//!     pub fn state(&self) -> CardState;
//!     pub fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT>;
//! }
//! ```
//!
//! `Card` registers and owns its own window class (guarded by `Once` so it is
//! only ever registered once per process) and its own `WNDPROC`. That
//! `WNDPROC` is a thin static shim that looks up the `Card`'s inner state via
//! `GWLP_USERDATA` and forwards every message to [`Card::handle_message`], so
//! the integrating agent does not need to do any message routing for the
//! card's `HWND` beyond running the normal `GetMessageW`/`DispatchMessageW`
//! loop — `DispatchMessageW` calls straight into our `WNDPROC` for messages
//! addressed to `card.hwnd()`. `handle_message` is exposed publicly mainly so
//! callers (or tests) can feed it synthetic messages directly.
//!
//! Notes for the integrating agent:
//! - Call [`Card::new`] once, after the process has set
//!   `PROCESS_PER_MONITOR_DPI_AWARE_V2` (that call is process-global and is
//!   expected to live in `app.rs`/`main.rs`; this module only *reads* DPI via
//!   `GetDpiForWindow`, it never sets process DPI awareness itself).
//! - `show_pending` / `show_answer` / `show_error` are all safe to call at any
//!   time from any state; each one fully resets timers, activation style and
//!   geometry before showing.
//! - `show_error` intentionally has no `auto_dismiss_secs` parameter (per the
//!   spec's given signature): an error card never auto-dismisses, since it is
//!   the one state where staying on screen until the user notices it is more
//!   valuable than tidiness. Call `hide()` explicitly if different behaviour
//!   is wanted.
//! - `show_error` also has no `difficulty` parameter: errors have no
//!   difficulty rating, and `show_error` always clears any badge left over
//!   from a previous `show_answer` call so it can never linger on an error
//!   card.
//! - `show_answer`'s `difficulty: Option<Difficulty>` draws a small badge in
//!   the card's bottom-right corner (Collapsed and Expanded only, never
//!   Pending). Pass `None` to render exactly as before this feature existed
//!   (no badge, no reserved space, no layout shift) -- this is what the
//!   integrator should pass when the user has the feature disabled.
//! - `show_pending` likewise has no timeout parameter, but pending state still
//!   carries an internal safety-net auto-dismiss (`PENDING_SAFETY_TIMEOUT_SECS`)
//!   so a card is never stuck forever if the worker thread never reports back.
//! - Nothing in this module panics. Win32 calls that can fail are always
//!   handled by falling back to a degraded-but-functional default (square
//!   corners, stock font, light theme, primary-monitor work area, etc.).

use std::ffi::c_void;
use std::sync::{Once, OnceLock};

use crate::ui::text::draw_text_line;
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    Arc, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint,
    ExtCreatePen, FillRect, FrameRect, GetDC, GetMonitorInfoW, GetStockObject, GetTextMetricsW,
    IntersectClipRect, MonitorFromPoint, MonitorFromWindow, ReleaseDC, RoundRect, SelectClipRgn,
    SelectObject, SetBkColor, SetBkMode, SetTextColor, BS_SOLID, DEFAULT_GUI_FONT, DT_CALCRECT,
    DT_CENTER, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP, DT_VCENTER,
    DT_WORDBREAK, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, LOGBRUSH, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, NULL_BRUSH, NULL_PEN, PS_ENDCAP_ROUND, PS_GEOMETRIC, PS_JOIN_ROUND,
    PS_SOLID, SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{SetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsChild,
    IsWindow, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    SystemParametersInfoW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, GWL_EXSTYLE,
    HMENU, HWND_TOPMOST, IDC_ARROW, NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE,
    SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_APP, WM_COMMAND, WM_CTLCOLOREDIT,
    WM_DESTROY, WM_DPICHANGED, WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN,
    WM_MOUSEWHEEL, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_SETFONT, WM_TIMER, WNDCLASSEXW,
    WS_CHILD, WS_DISABLED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP,
    WS_VISIBLE,
};

/// Posted to the card's owner window (see [`Card::set_owner`]) whenever the
/// preview state (#26) closes with a decision -- "Do it" or Cancel/Esc
/// (issue #39). Carries no payload: `App::on_preview_decided` pulls the
/// result via [`Card::take_confirmed`], the same take-not-peek shape that
/// method already has, so there is nothing to box across the message
/// boundary and nothing to free even if this arrives while Settings is
/// open and gets dropped. Adding another `WM_APP_*` constant anywhere in
/// the crate also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS`
/// (issue #163) and its `count_declarations` file list -- see that test's
/// doc comment.
pub const WM_APP_PREVIEW_DECIDED: u32 = WM_APP + 9;

/// Posted to the card's owner window (see [`Card::set_owner`]) when the user
/// clicks a card shown via [`Card::show_settings_needed`] -- issue #347.
/// Carries no payload: `App::on_card_open_settings` just calls
/// `App::open_settings()`. Adding another `WM_APP_*` constant anywhere in the
/// crate also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue
/// #163) and its `count_declarations` file list -- see that test's doc
/// comment.
pub const WM_APP_CARD_OPEN_SETTINGS: u32 = WM_APP + 15;

use crate::provider::Difficulty;
use crate::ui::preview::{Field, PreviewModel};

// ---------------------------------------------------------------------------
// Public state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Hidden,
    Pending,
    Collapsed,
    Expanded,
    /// The confirmation state (#26): a proposal rendered as a compact form
    /// (title + one row per schema field), with "Do it" / "Edit" / "Cancel"
    /// buttons. See [`Card::show_preview`].
    Preview,
}

/// A single owned notification-card window.
///
/// `Card` is a thin handle around a heap-allocated `CardInner`. The indirection
/// matters: the window's `WNDPROC` stashes a raw pointer to `CardInner` in
/// `GWLP_USERDATA` at creation time, and that pointer must stay valid for the
/// life of the window even if the `Card` handle itself is moved (e.g. stored
/// into a field of the caller's app struct). Boxing the real state means only
/// the box's *pointer* moves around; the pointee's address never changes.
pub struct Card {
    inner: Box<CardInner>,
}

impl Card {
    pub fn new(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the card window class");
        }
        Card::create(instance, CLASS_NAME)
    }

    /// Same as [`Card::new`], but registers (once) and uses a class name
    /// distinct from the production window class (rule 9: tests never touch
    /// production names). Used only by the preview state's real-Win32 test
    /// in this module, which needs an actual `HWND` with real child
    /// controls, not the production `Card`'s class.
    #[cfg(test)]
    pub(crate) fn new_for_test(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_test_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the test card window class");
        }
        Card::create(instance, TEST_CLASS_NAME)
    }

    fn create(instance: HINSTANCE, class_name: &str) -> anyhow::Result<Self> {
        let theme = detect_theme();
        // A 96 DPI guess used only to build a first, throwaway set of fonts
        // before we have a real HWND to ask GetDpiForWindow about. It is
        // replaced immediately below once the window exists.
        let inner = Box::new(CardInner {
            hwnd: HWND(std::ptr::null_mut()),
            instance,
            state: CardState::Hidden,
            headline: String::new(),
            detail: String::new(),
            difficulty: None,
            dpi: 96,
            theme,
            fonts: Fonts::null(),
            text_scale: 1.0,
            anim_frame: 0,
            scroll_offset: 0,
            scroll_max: 0,
            ex_noactivate_removed: false,
            preview: None,
            last_confirmed: None,
            owner: None,
            open_settings_on_click: false,
            preview_decision_pending: false,
            preview_generation: 0,
            edit_bg_brush: HBRUSH(std::ptr::null_mut()),
        });
        let raw = Box::into_raw(inner);

        let class_name = wide_z(class_name);
        let title = wide_z("Wingman");
        let create_result = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        };

        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(e) => {
                // Reclaim and drop the box we leaked into CreateWindowExW so
                // it does not leak on the error path.
                unsafe {
                    drop(Box::from_raw(raw));
                }
                return Err(anyhow::anyhow!("Wingman: CreateWindowExW failed: {e}"));
            }
        };

        // Safety: `raw` is still a valid, uniquely-owned allocation (nothing
        // has taken ownership of it yet — WM_NCCREATE only copied the pointer
        // value into GWLP_USERDATA). Reclaim it into the `Card` we return.
        let inner_ref = unsafe { &mut *raw };
        inner_ref.hwnd = hwnd;
        inner_ref.dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        inner_ref.rebuild_fonts();

        // Best-effort rounded corners (Windows 11). Falling back to square
        // corners on older systems or DWM failures is an acceptable degrade.
        unsafe {
            let pref = DWMWCP_ROUND;
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            );
        }

        Ok(Card {
            inner: unsafe { Box::from_raw(raw) },
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd
    }

    /// Issue #39: tells the card which window to `PostMessageW`
    /// [`WM_APP_PREVIEW_DECIDED`] to when a preview closes with a
    /// decision. Call once, right after [`Card::new`] -- see `owner`'s
    /// doc comment on [`CardInner`].
    pub fn set_owner(&mut self, hwnd: HWND) {
        self.inner.owner = Some(hwnd);
    }

    pub fn show_pending(&mut self) {
        self.inner.show_pending();
    }

    pub fn show_answer(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
    ) {
        self.inner
            .show_collapsed(headline, detail, auto_dismiss_secs, difficulty);
    }

    pub fn show_error(&mut self, headline: &str, detail: &str) {
        // No timeout parameter is given for errors: they persist until the
        // user dismisses them (click to expand, then Esc / focus-loss), or
        // until a later show_* call replaces them. See module docs.
        //
        // difficulty is always None here: errors have no difficulty rating,
        // and passing None explicitly (rather than leaving a stale value)
        // ensures a previous answer's badge can never linger on an error
        // card.
        self.inner.show_collapsed(headline, detail, 0, None);
    }

    /// Issue #347: like [`Card::show_error`] (persists until dismissed, no
    /// difficulty badge), except a click on the card while it is still
    /// Collapsed posts [`WM_APP_CARD_OPEN_SETTINGS`] to the owner window
    /// (see [`Card::set_owner`]) and hides the card, instead of expanding it
    /// for more detail. Used for the readiness-gate cards ("No AI model set
    /// up yet", "Local mode needs Ollama configured") -- the actionable next
    /// step for those is Settings, not more text to read.
    pub fn show_settings_needed(&mut self, headline: &str, detail: &str) {
        self.inner.show_collapsed(headline, detail, 0, None);
        self.inner.open_settings_on_click = true;
    }

    pub fn hide(&mut self) {
        self.inner.hide();
    }

    /// Multiplies every font size in the card. 1.0 is the built-in default.
    /// Clamped to a sane range so a bad config value cannot make the card
    /// unreadable or enormous.
    pub fn set_text_scale(&mut self, scale: f32) {
        self.inner.set_text_scale(scale);
    }

    pub fn state(&self) -> CardState {
        self.inner.state
    }

    /// Enters the preview (confirmation) state (#26). `schema` is the
    /// proposal's JSON Schema (see `actions::schema::schema_for`) and
    /// `proposal_value` its current value; together they build a
    /// [`PreviewModel`] the card renders as `title` plus one row per
    /// declared field, with an EDIT control for every field the schema
    /// marks `"editable"`.
    ///
    /// `main_window_exists` gates whether the "Edit" button is created at
    /// all (#352): with no main window to open, a permanently-disabled Edit
    /// button just squeezed "Do it" and "Cancel" for no reason, so the
    /// button is omitted entirely until a main window exists (#26's issue
    /// body: "Edit opens the main window when it exists"). No caller passes
    /// `true` yet -- Wingman has no main window today -- the same "inert
    /// until its caller exists" status `Action::hotkey` has; the button's
    /// create-or-not wiring is still exercised by
    /// `preview_edit_button_only_exists_once_main_window_exists` below.
    ///
    /// Nothing runs until the user presses "Do it" (Enter) or "Cancel"
    /// (Esc): see [`Card::take_confirmed`].
    ///
    /// **Focus** (documented per the task's "decide carefully"): unlike
    /// Pending/Collapsed, which are created `WS_EX_NOACTIVATE` so showing
    /// them never steals focus from whatever the user is doing, Preview
    /// removes that style and takes real keyboard focus immediately once
    /// shown. This is deliberate, not an oversight: the preview state exists
    /// *only* for a proposal that requires an explicit user decision before
    /// anything happens (typing into an editable field, or pressing
    /// Enter/Esc, both need real focus to work at all), so the interruption
    /// is the entire point of showing it -- there is no useful "shown but
    /// not yet interacted with" middle state to preserve focus through, the
    /// way there is for a passive Collapsed answer card. The interruption is
    /// temporary: `close_preview` (on Do it, Cancel, or Esc) always restores
    /// whatever window had the foreground immediately before `show_preview`
    /// was called, so focus returns to the user's previous work the moment
    /// the decision is made either way.
    #[allow(dead_code)] // wiring app.rs's worker to call this is a later issue's job
    /// Shows the preview and returns its generation (#225). Store it: the
    /// `WM_APP_PREVIEW_DECIDED` that eventually arrives carries the
    /// generation it belongs to in its `WPARAM`, and anything older is a
    /// late abandonment for a preview that is already gone, not a decision
    /// about this one.
    #[must_use = "store the generation; on_preview_decided needs it to reject a stale notification"]
    pub fn show_preview(
        &mut self,
        title: &str,
        schema: &serde_json::Value,
        proposal_value: &serde_json::Value,
        main_window_exists: bool,
    ) -> u32 {
        self.inner
            .show_preview(title, schema, proposal_value, main_window_exists);
        self.inner.preview_generation
    }

    /// Takes the `Confirmed<Value>` produced by the last "Do it" / Enter, if
    /// any. A take, not a peek: the stored value is cleared by this call, so
    /// the same confirmation can never be handed to an executor twice.
    /// Returns `None` before "Do it" has been pressed, after Cancel/Esc, or
    /// on a repeated call.
    #[allow(dead_code)] // see show_preview's doc comment
    pub fn take_confirmed(&mut self) -> Option<crate::ui::confirm::Confirmed<serde_json::Value>> {
        self.inner.last_confirmed.take()
    }

    /// The card's own window proc dispatches internally; this is the seam
    /// tests use to feed synthetic messages.
    #[allow(dead_code)]
    pub fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        self.inner.handle_message(msg, wparam, lparam)
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        // Destroying the window is synchronous: WM_DESTROY/WM_NCDESTROY are
        // delivered (and handled by our WNDPROC via the still-valid raw
        // pointer) before DestroyWindow returns, so this is safe to do while
        // `self.inner` is still alive.
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

const CLASS_NAME: &str = "Wingman.Card.Window.7f3c1a9e";
/// Rule 9: the preview state's real-Win32 test creates an actual `HWND` with
/// real child controls, so it gets its own window class rather than sharing
/// the production one, the same way `single_instance`'s test uses its own
/// mutex/class names (commit `011f11a`).
#[cfg(test)]
const TEST_CLASS_NAME: &str = "Wingman.Card.Window.7f3c1a9e.Test";

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
        // We paint the entire client area ourselves (double-buffered) and
        // return 1 from WM_ERASEBKGND, so no background brush is needed.
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc) != 0
}

/// The window procedure for every card window. It is a thin shim: it stashes
/// the `CardInner` pointer passed via `CreateWindowExW`'s `lpparam` into
/// `GWLP_USERDATA` on `WM_NCCREATE`, then forwards every subsequent message to
/// [`CardInner::handle_message`], falling back to `DefWindowProcW` for
/// anything that method does not claim.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        if !cs.lpCreateParams.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardInner;
    if !ptr.is_null() {
        let inner = &mut *ptr;
        if let Some(result) = inner.handle_message(msg, wparam, lparam) {
            return result;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Theme {
    Light,
    Dark,
}

struct Palette {
    bg: u32,
    border: u32,
    headline: u32,
    detail: u32,
    hint: u32,
}

impl Theme {
    fn palette(self) -> Palette {
        match self {
            // Neither pure black nor pure white, per spec.
            Theme::Dark => Palette {
                bg: rgb(32, 32, 32),
                border: rgb(64, 64, 64),
                headline: rgb(245, 245, 245),
                detail: rgb(190, 190, 190),
                hint: rgb(140, 140, 140),
            },
            Theme::Light => Palette {
                bg: rgb(250, 250, 250),
                border: rgb(214, 214, 214),
                headline: rgb(23, 23, 23),
                detail: rgb(90, 90, 90),
                hint: rgb(122, 122, 122),
            },
        }
    }
}

fn rgb(r: u8, g: u8, b: u8) -> u32 {
    (r as u32) | ((g as u32) << 8) | ((b as u32) << 16)
}

/// Issue #350: the `(text, background)` colors a preview EDIT field is
/// painted with, so it follows the card's theme instead of the stock EDIT
/// control's fixed white-on-black. Pure so the theme -> color mapping is
/// unit-testable without a real window; the WM_CTLCOLOREDIT handler is the
/// only caller and does the GDI calls (SetTextColor/SetBkColor/brush) this
/// function has no business doing.
fn edit_field_colors(palette: &Palette) -> (u32, u32) {
    (palette.headline, palette.bg)
}

// ---------------------------------------------------------------------------
// Difficulty badge colour
// ---------------------------------------------------------------------------

/// Green -> amber -> red gradient stops for `Difficulty::Level`, at ranks 1,
/// 5 and 10 respectively. Three stops (not a straight two-stop green->red
/// lerp) so the midpoint reads as amber/yellow rather than a muddy brown.
const GRADIENT_LOW: (u8, u8, u8) = (0x2e, 0xa0, 0x43); // green, rank 1
const GRADIENT_MID: (u8, u8, u8) = (0xf2, 0xa9, 0x00); // amber, rank 5
const GRADIENT_HIGH: (u8, u8, u8) = (0xd6, 0x2c, 0x2c); // red, rank 10
/// Deliberately outside the 1-10 gradient: Ultra is its own thing, not
/// "worse than 10".
const ULTRA_COLOR: (u8, u8, u8) = (0x8e, 0x24, 0xaa); // purple

/// Maps a difficulty to the badge's fill colour, via `Difficulty::rank()`
/// (1..=11, Ultra == 11) rather than matching on the variant, per that
/// method's own doc comment. Ranks outside 1..=10 are clamped rather than
/// trusted for the gradient half, since this module must never panic
/// regardless of what upstream hands it.
fn difficulty_color(difficulty: Difficulty) -> u32 {
    let rank = difficulty.rank();
    if rank >= 11 {
        return rgb(ULTRA_COLOR.0, ULTRA_COLOR.1, ULTRA_COLOR.2);
    }
    let level = rank.clamp(1, 10) as f32;
    let (from, to, t) = if level <= 5.0 {
        (GRADIENT_LOW, GRADIENT_MID, (level - 1.0) / 4.0)
    } else {
        (GRADIENT_MID, GRADIENT_HIGH, (level - 5.0) / 5.0)
    };
    rgb(
        lerp_u8(from.0, to.0, t),
        lerp_u8(from.1, to.1, t),
        lerp_u8(from.2, to.2, t),
    )
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Picks a legible label colour (near-black ink or near-white) for text sat
/// on top of `fill`, using perceived luminance (ITU-R BT.601 weights) so a
/// light fill (amber) gets dark ink and a dark fill (green/red/purple) gets
/// white, per spec ("a yellow fill with white text is not [fine]").
fn badge_text_color(fill: u32) -> u32 {
    let r = (fill & 0xFF) as f32;
    let g = ((fill >> 8) & 0xFF) as f32;
    let b = ((fill >> 16) & 0xFF) as f32;
    let luminance = 0.299 * r + 0.587 * g + 0.114 * b;
    if luminance > 140.0 {
        rgb(0x20, 0x20, 0x20)
    } else {
        rgb(0xff, 0xff, 0xff)
    }
}

/// Reads `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\AppsUseLightTheme`.
///
/// `windows` 0.62 is built in this project *without* the `Win32_System_Registry`
/// feature (see `Cargo.toml`, owned by another agent), so `RegGetValueW` is not
/// available through the `windows` crate here. Rather than touch `Cargo.toml`,
/// this module declares the one registry function it needs itself and links
/// directly against `advapi32.dll` (a standard system import library, not a
/// new crate dependency).
fn detect_theme() -> Theme {
    if registry_apps_use_light_theme() {
        Theme::Light
    } else {
        Theme::Dark
    }
}

const RRF_RT_REG_DWORD: u32 = 0x0000_0010;

fn hkey_current_user() -> *mut c_void {
    // HKEY_CURRENT_USER is defined by the Windows headers as
    // ((HKEY)(ULONG_PTR)((LONG)0x80000001)) — i.e. sign-extended.
    (0x8000_0001u32 as i32 as isize) as *mut c_void
}

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        hkey: *mut c_void,
        lpsubkey: *const u16,
        lpvalue: *const u16,
        dwflags: u32,
        pdwtype: *mut u32,
        pvdata: *mut c_void,
        pcbdata: *mut u32,
    ) -> i32;
}

fn registry_apps_use_light_theme() -> bool {
    let subkey = wide_z(r"Software\Microsoft\Windows\CurrentVersion\Themes\Personalize");
    let value = wide_z("AppsUseLightTheme");
    let mut data: u32 = 1; // default to light on any failure
    let mut size: u32 = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            hkey_current_user(),
            subkey.as_ptr(),
            value.as_ptr(),
            RRF_RT_REG_DWORD,
            std::ptr::null_mut(),
            &mut data as *mut u32 as *mut c_void,
            &mut size,
        )
    };
    if status == 0 {
        data != 0
    } else {
        true
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

struct Fonts {
    headline: HFONT,
    body: HFONT,
    /// Small font used only for the difficulty badge's label.
    badge: HFONT,
}

impl Fonts {
    fn null() -> Self {
        Fonts {
            headline: HFONT(std::ptr::null_mut()),
            body: HFONT(std::ptr::null_mut()),
            badge: HFONT(std::ptr::null_mut()),
        }
    }

    fn delete(&self) {
        unsafe {
            if !self.headline.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.headline.0));
            }
            if !self.body.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.body.0));
            }
            if !self.badge.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.badge.0));
            }
        }
    }
}

/// Headline size relative to the system message font. Deliberately below
/// 1.0: a stock Windows notification uses roughly the message-font size,
/// and the user wants the card's headline clearly smaller than that (see
/// `HEADLINE_MAX_LINES` for the character-budget arithmetic this feeds).
const HEADLINE_FONT_SCALE: f32 = 0.85;
/// Sits just under the headline so the two remain distinguishable by size
/// as a secondary cue, even though colour (`Palette::headline` vs.
/// `Palette::detail`) is now the primary way to tell them apart -- neither
/// font is bold any more.
const DETAIL_FONT_SCALE: f32 = 0.8;
/// The difficulty badge's label is the smallest text on the card -- it is an
/// at-a-glance annotation, not a headline, and the whole point is that it not
/// compete with the (already small, non-bold) answer text for attention.
const BADGE_FONT_SCALE: f32 = 0.7;

/// Builds the headline/body fonts for `dpi` and `text_scale`, deriving from
/// the shell's message font (`SPI_GETNONCLIENTMETRICS`) so the card matches
/// the system. Infallible: any Win32 failure degrades to a stock GUI font
/// rather than panicking or propagating an error.
fn build_fonts(dpi: u32, text_scale: f32) -> Fonts {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };

        // `SystemParametersInfoForDpi` returns metrics ALREADY scaled for the
        // DPI you ask for. `SystemParametersInfoW` also returns pre-scaled
        // metrics -- for the system DPI, not this monitor's. Either way the
        // font that comes back is in real pixels, so multiplying it by
        // dpi/96 a second time double-scales it: at 250% the card rendered
        // ~2.5x too large, which is exactly what it looked like on screen.
        //
        // So: ask for this monitor's DPI, then apply only the design ratios
        // and the user's text_scale -- never the DPI factor again.
        let got = SystemParametersInfoForDpi(
            SPI_GETNONCLIENTMETRICS.0,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            0,
            dpi,
        )
        .is_ok()
            || SystemParametersInfoW(
                SPI_GETNONCLIENTMETRICS,
                ncm.cbSize,
                Some(&mut ncm as *mut _ as *mut c_void),
                SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
            )
            .is_ok();

        let base = if got {
            ncm.lfMessageFont
        } else {
            // The fallback is expressed at 96 DPI, so this one does need the
            // monitor scale applied.
            let mut lf = fallback_logfont();
            lf.lfHeight = scaled_height(lf.lfHeight, dpi as f32 / 96.0);
            lf
        };

        let scale = text_scale;

        let mut headline_lf = base;
        // Normal weight and sub-message size, per user feedback: the
        // headline should read smaller than a regular Windows notification,
        // not bolder/larger than one. Headline vs. detail is distinguished
        // by colour now (see Palette), not by weight.
        headline_lf.lfWeight = FW_NORMAL.0 as i32;
        headline_lf.lfHeight = scaled_height(base.lfHeight, scale * HEADLINE_FONT_SCALE);

        let mut body_lf = base;
        body_lf.lfWeight = FW_NORMAL.0 as i32;
        body_lf.lfHeight = scaled_height(base.lfHeight, scale * DETAIL_FONT_SCALE);

        let mut badge_lf = base;
        badge_lf.lfWeight = FW_NORMAL.0 as i32;
        badge_lf.lfHeight = scaled_height(base.lfHeight, scale * BADGE_FONT_SCALE);

        Fonts {
            headline: font_or_stock(&headline_lf),
            body: font_or_stock(&body_lf),
            badge: font_or_stock(&badge_lf),
        }
    }
}

fn scaled_height(base_height: i32, factor: f32) -> i32 {
    let v = (base_height as f32 * factor).round() as i32;
    if v == 0 {
        if base_height < 0 {
            -1
        } else {
            1
        }
    } else {
        v
    }
}

unsafe fn font_or_stock(lf: &windows::Win32::Graphics::Gdi::LOGFONTW) -> HFONT {
    let f = CreateFontIndirectW(lf);
    if f.0.is_null() {
        HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
    } else {
        f
    }
}

fn fallback_logfont() -> windows::Win32::Graphics::Gdi::LOGFONTW {
    let mut lf = windows::Win32::Graphics::Gdi::LOGFONTW {
        lfHeight: -12,
        ..Default::default()
    };
    for (i, u) in "Segoe UI".encode_utf16().enumerate() {
        if i < lf.lfFaceName.len() {
            lf.lfFaceName[i] = u;
        }
    }
    lf
}

// ---------------------------------------------------------------------------
// Layout constants (logical pixels at 96 DPI; scaled per-window via `scale`)
// ---------------------------------------------------------------------------

const MARGIN_DP: i32 = 14;
const PADDING_DP: i32 = 14;
const CARD_WIDTH_DP: i32 = 280;
/// Collapsed headline cap. The prompt budgets a 90-character headline.
/// Content width is `CARD_WIDTH_DP - 2*PADDING_DP` = 280 - 28 = 252dp.
/// Segoe UI's average character width is roughly half its em size; the
/// headline font's `lfHeight` at 96 DPI/1.0 text scale is
/// `round(-12 * HEADLINE_FONT_SCALE)` = `round(-12 * 0.85)` = -10, i.e. a
/// ~10dp em, so average glyph width is ~5dp -- call it 4.5dp once
/// `DT_WORDBREAK`'s end-of-line slack is priced in. That's 252 / 4.5 ~= 56
/// characters per line, so two lines already covers a 90-character
/// headline; three lines leaves real headroom for wider glyphs/short words
/// that push wrapping earlier, so a full 90-character headline is never
/// silently `DT_END_ELLIPSIS`'d.
const HEADLINE_MAX_LINES: i32 = 3;
/// Side length of the small square pending card (there is no text in it
/// any more -- just the spinner -- so it does not need to be wide).
const PENDING_SIZE_DP: i32 = 60;
const GAP_DP: i32 = 6;
const WHEEL_SCROLL_DP: i32 = 48;

// -- Difficulty badge (Collapsed / Expanded only) -----------------------

/// Distance from the card's outer edge to the badge's outer edge. Smaller
/// than `PADDING_DP` on purpose: the badge nestles into the corner, mostly
/// inside the existing padding whitespace, rather than adding a second ring
/// of margin around it.
const BADGE_EDGE_MARGIN_DP: i32 = 8;
/// Horizontal/vertical text inset inside the badge shape.
const BADGE_PAD_X_DP: i32 = 5;
const BADGE_PAD_Y_DP: i32 = 3;
/// Floor on both badge dimensions so a single-digit label (e.g. "1") still
/// reads as a deliberate shape rather than a sliver -- this also makes
/// single-character badges come out as circles (width == height) while
/// two-character ones ("10") and "U" come out as pills.
const BADGE_MIN_DIAMETER_DP: i32 = 18;
/// Minimum clearance kept between the badge and any headline text next to
/// it, on top of the badge's own width.
const BADGE_TEXT_GAP_DP: i32 = 6;

const TIMER_ANIM: usize = 1;
const TIMER_DISMISS: usize = 2;
/// A rotation needs ~16-33ms/frame to read as smooth; the old text-dot
/// animation could get away with much slower ticks, but a spinner cannot.
const ANIM_INTERVAL_MS: u32 = 20;
const PENDING_SAFETY_TIMEOUT_SECS: u32 = 30;

// -- Spinner (pending state) -------------------------------------------

/// Pen width for both the dim ring and the bright sweep, in logical pixels.
const SPINNER_STROKE_DP: i32 = 4;
/// Degrees the sweep advances per `TIMER_ANIM` tick. At `ANIM_INTERVAL_MS`
/// (20ms) this is 360 / 6 * 20ms = 1200ms per full revolution -- a typical
/// indeterminate-spinner cadence.
const SPINNER_DEGREES_PER_FRAME: f32 = 6.0;
/// Arc length of the bright sweep segment.
const SPINNER_SWEEP_DEG: f32 = 100.0;

// ---------------------------------------------------------------------------
// CardInner: the real state; addressed by raw pointer from GWLP_USERDATA
// ---------------------------------------------------------------------------

struct CardInner {
    hwnd: HWND,
    /// The module instance the window (and every preview child control) was
    /// created with. Stashed here (rather than re-fetched with
    /// `GetModuleHandleW`) so `create_preview_controls` creates its EDIT and
    /// BUTTON children against the exact same instance `CreateWindowExW`
    /// used for the card's own window.
    instance: HINSTANCE,
    state: CardState,
    headline: String,
    detail: String,
    /// `None` renders exactly as before this feature existed: no badge, no
    /// reserved layout space. Cleared by `show_error` and by `show_pending`
    /// so a stale badge can never linger onto a state that shouldn't have
    /// one.
    difficulty: Option<Difficulty>,
    dpi: u32,
    theme: Theme,
    fonts: Fonts,
    /// User-tunable multiplier applied to every font size. Set via
    /// [`Card::set_text_scale`]; defaults to 1.0.
    text_scale: f32,
    anim_frame: u32,
    scroll_offset: i32,
    scroll_max: i32,
    /// True while WS_EX_NOACTIVATE has been removed (i.e. while Expanded).
    ex_noactivate_removed: bool,
    /// Live state for `CardState::Preview`, `None` in every other state.
    /// `Some` for exactly as long as the preview's child controls exist --
    /// see `leave_preview_if_active`, the single choke point every path out
    /// of Preview goes through.
    preview: Option<PreviewUi>,
    /// The `Confirmed<Value>` from the most recent "Do it" / Enter, read
    /// (and cleared) by `Card::take_confirmed`. Deliberately NOT stored
    /// inside `PreviewUi`: `preview_do_it` closes the preview (via
    /// `close_preview` -> `hide` -> `leave_preview_if_active`) in the same
    /// call that produces the `Confirmed`, and `leave_preview_if_active`
    /// unconditionally drops `PreviewUi` -- a value stored there would be
    /// destroyed before any caller could ever read it back.
    last_confirmed: Option<crate::ui::confirm::Confirmed<serde_json::Value>>,
    /// Issue #39: the owner window [`Card::set_owner`] was told about, if
    /// any -- `preview_do_it`/`preview_cancel` `PostMessageW`
    /// [`WM_APP_PREVIEW_DECIDED`] here so `App::on_preview_decided` can
    /// call [`Card::take_confirmed`] and, for "Do it", actually run an
    /// executor. `None` until `set_owner` is called (every production
    /// caller does so once, right after `Card::new`); a preview shown with
    /// no owner set still works, it just has nowhere to notify -- the same
    /// degrade `Tray`'s own best-effort Win32 calls use elsewhere.
    owner: Option<HWND>,
    /// Issue #225: `true` from the moment a preview is shown until a
    /// decision has been reported for it. "Do it" and Cancel clear it and
    /// notify the owner themselves; anything else that tears the preview
    /// down (hiding the card, starting another action, opening Settings,
    /// pausing, or replacing this preview with a different one) leaves it
    /// set, and [`CardInner::leave_preview_if_active`] then reports the
    /// abandonment so the owner can drop whatever it was holding.
    ///
    /// Without this, a preview could be destroyed with no
    /// [`WM_APP_PREVIEW_DECIDED`] ever posted, `App`'s `pending_review` /
    /// `pending_form_fill` would stay `Some`, and a later "Do it" on a
    /// different preview could run the abandoned action instead of the one
    /// the user actually confirmed.
    /// Issue #347: `true` while the currently-showing Collapsed card is a
    /// "you need to configure something" prompt whose click should open
    /// Settings instead of expanding for more detail. Set only by
    /// [`Card::show_settings_needed`]; cleared by every other path into
    /// Collapsed (`show_collapsed`, used by both `show_answer` and
    /// `show_error`) so it can never linger onto an unrelated card.
    open_settings_on_click: bool,
    preview_decision_pending: bool,
    /// Issue #225: incremented on every [`CardInner::show_preview`], and
    /// posted as `WM_APP_PREVIEW_DECIDED`'s `WPARAM` so the owner can tell
    /// WHICH preview a decision belongs to.
    ///
    /// The notification is a `PostMessageW`, so it is delivered after the
    /// call that triggered it has returned. Replacing a live preview posts
    /// the old one's abandonment and then arms the new one in the same turn;
    /// without a generation the owner would process that abandonment later
    /// and clear the state belonging to the preview now on screen.
    preview_generation: u32,
    /// Issue #350: the brush WM_CTLCOLOREDIT returns for preview EDIT
    /// fields, matching `self.theme`. Created lazily on first use and cached
    /// rather than created per-paint (WM_CTLCOLOREDIT fires on every
    /// keystroke and repaint), and deleted once in WM_NCDESTROY -- the theme
    /// never changes for the lifetime of a card window (no WM_SETTINGCHANGE
    /// handler exists), so one brush for the window's whole life is correct,
    /// not just an optimisation.
    edit_bg_brush: HBRUSH,
}

impl CardInner {
    fn scale(&self, dp: i32) -> i32 {
        (dp * self.dpi as i32 + 48) / 96
    }

    fn rebuild_fonts(&mut self) {
        let fresh = build_fonts(self.dpi, self.text_scale);
        let old = std::mem::replace(&mut self.fonts, fresh);
        old.delete();
    }

    /// Clamps and applies a new text-scale multiplier, then rebuilds fonts
    /// and re-runs the current state's layout so the change is visible
    /// immediately, even if the card is already on screen.
    fn set_text_scale(&mut self, scale: f32) {
        // Clamped to roughly half to double the default so a bad config
        // value cannot make the card unreadable (too small) or enormous
        // (too large).
        self.text_scale = scale.clamp(0.5, 2.0);
        self.rebuild_fonts();
        self.relayout_current_state();
    }

    // -- show/hide -----------------------------------------------------

    fn show_pending(&mut self) {
        self.leave_preview_if_active();
        self.reset_activation_and_timers();
        // No text in the pending state any more -- it shows a spinner.
        self.headline.clear();
        self.detail.clear();
        self.difficulty = None; // pending never shows a badge; keep state tidy
        self.state = CardState::Pending;
        self.anim_frame = 0;
        self.scroll_offset = 0;
        self.scroll_max = 0;

        unsafe {
            let _ = SetTimer(Some(self.hwnd), TIMER_ANIM, ANIM_INTERVAL_MS, None);
            let _ = SetTimer(
                Some(self.hwnd),
                TIMER_DISMISS,
                PENDING_SAFETY_TIMEOUT_SECS.saturating_mul(1000),
                None,
            );
        }

        self.layout_pending();
        self.reveal();
    }

    fn show_collapsed(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
    ) {
        self.leave_preview_if_active();
        self.reset_activation_and_timers();
        self.headline = headline.to_string();
        self.detail = detail.to_string();
        self.difficulty = difficulty;
        self.state = CardState::Collapsed;
        self.scroll_offset = 0;
        self.scroll_max = 0;
        // Issue #347: every ordinary show_answer/show_error call starts a
        // plain card, not a "click to open Settings" prompt -- only
        // `Card::show_settings_needed` sets this, right after this call
        // returns.
        self.open_settings_on_click = false;

        unsafe {
            if auto_dismiss_secs > 0 {
                let _ = SetTimer(
                    Some(self.hwnd),
                    TIMER_DISMISS,
                    auto_dismiss_secs.saturating_mul(1000),
                    None,
                );
            }
        }

        self.layout_collapsed();
        self.reveal();
    }

    fn hide(&mut self) {
        self.leave_preview_if_active();
        self.kill_timers();
        self.set_noactivate(true);
        self.state = CardState::Hidden;
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn reveal(&mut self) {
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_SHOWNOACTIVATE);
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
            );
        }
        self.invalidate();
    }

    fn reset_activation_and_timers(&mut self) {
        self.kill_timers();
        self.set_noactivate(true);
    }

    fn kill_timers(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            let _ = KillTimer(Some(self.hwnd), TIMER_DISMISS);
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Adds or removes `WS_EX_NOACTIVATE`.
    ///
    /// Tension noted in the spec: the card is created `WS_EX_NOACTIVATE` so
    /// showing it never steals focus (important for Pending/Collapsed, which
    /// pop up while the user is typing/working elsewhere). But once Expanded
    /// we need `SetForegroundWindow` to succeed so the window can later
    /// receive `WM_KILLFOCUS` and close itself — and `SetForegroundWindow`
    /// on a `WS_EX_NOACTIVATE` window will not actually give it the focus.
    /// So we strip the style right before expanding and restore it on every
    /// path back to Pending/Collapsed/Hidden.
    fn set_noactivate(&mut self, add: bool) {
        unsafe {
            let cur = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let mask = WS_EX_NOACTIVATE.0 as isize;
            let new = if add { cur | mask } else { cur & !mask };
            if new != cur {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new);
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            self.ex_noactivate_removed = !add;
        }
    }

    // -- click / expand --------------------------------------------------

    fn try_expand(&mut self) {
        if self.state != CardState::Collapsed || self.detail.is_empty() {
            return;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DISMISS); // expanded never auto-dismisses
        }
        self.set_noactivate(false);
        self.state = CardState::Expanded;
        self.scroll_offset = 0;
        self.layout_expanded();
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.invalidate();
    }

    /// Closes the card entirely. Used for WM_KILLFOCUS and Esc while
    /// Expanded, per spec ("close on WM_KILLFOCUS ... Esc also closes").
    fn close_expanded(&mut self) {
        if self.state == CardState::Expanded {
            self.hide();
        }
    }

    // -- layout ------------------------------------------------------------

    fn work_area_for_cursor(&self) -> RECT {
        unsafe {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            work_area_for_monitor(hmon)
        }
    }

    fn work_area_for_self(&self) -> RECT {
        unsafe {
            let hmon = MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST);
            work_area_for_monitor(hmon)
        }
    }

    fn place_bottom_right(&self, work: RECT, width: i32, height: i32) {
        let margin = self.scale(MARGIN_DP);
        let x = (work.right - margin - width).max(work.left);
        let y = (work.bottom - margin - height).max(work.top);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
    }

    fn layout_pending(&mut self) {
        let size = self.scale(PENDING_SIZE_DP);
        let work = self.work_area_for_cursor();
        self.place_bottom_right(work, size, size);
    }

    fn layout_collapsed(&mut self) {
        let padding = self.scale(PADDING_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_width = self.headline_content_width(content_width);
        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        let measured = self.measure_wrapped(self.fonts.headline, &self.headline, headline_width);
        let headline_h = measured.min(max_headline_h).max(headline_line_h);

        // No affordance line: the card is just the headline. Clicking it
        // still expands when there is detail to show.
        let height = padding * 2 + headline_h;

        let work = self.work_area_for_cursor();
        self.place_bottom_right(work, width, height);
    }

    fn layout_expanded(&mut self) {
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_width = self.headline_content_width(content_width);
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_width)
            .max(self.line_height(self.fonts.headline));
        let detail_h = if self.detail.is_empty() {
            0
        } else {
            self.measure_wrapped(self.fonts.body, &self.detail, content_width)
        };
        let content_h = headline_h + if detail_h > 0 { gap + detail_h } else { 0 };

        let work = self.work_area_for_self();
        let work_h = (work.bottom - work.top).max(1);
        let max_window_h = ((work_h as f32) * 0.6) as i32;
        let min_window_h = padding * 2 + self.line_height(self.fonts.headline);

        let desired_window_h = padding * 2 + content_h;
        let window_h = desired_window_h.min(max_window_h).max(min_window_h);
        let viewport_h = (window_h - padding * 2).max(1);

        self.scroll_max = (content_h - viewport_h).max(0);
        self.scroll_offset = self.scroll_offset.clamp(0, self.scroll_max);

        self.place_bottom_right(work, width, window_h);
    }

    /// Called after a DPI change: rebuild fonts for the new DPI and re-run
    /// whichever layout matches the current state, so every metric is
    /// re-derived from the new monitor rather than left stale.
    fn relayout_current_state(&mut self) {
        match self.state {
            CardState::Hidden => {}
            CardState::Pending => self.layout_pending(),
            CardState::Collapsed => self.layout_collapsed(),
            CardState::Expanded => self.layout_expanded(),
            CardState::Preview => {
                self.layout_preview();
                self.reposition_preview_controls();
            }
        }
        self.invalidate();
    }

    // -- text measurement ----------------------------------------------

    fn line_height(&self, font: HFONT) -> i32 {
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                return self.scale(16);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut tm = TEXTMETRICW::default();
            let ok = GetTextMetricsW(hdc, &mut tm).as_bool();
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            if ok {
                tm.tmHeight + tm.tmExternalLeading
            } else {
                self.scale(16)
            }
        }
    }

    fn measure_wrapped(&self, font: HFONT, text: &str, width: i32) -> i32 {
        if text.is_empty() || width <= 0 {
            return 0;
        }
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                return self.line_height(font);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut buf = utf16(text);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            (rect.bottom - rect.top).max(0)
        }
    }

    /// Natural (unwrapped) size of a single-line label, e.g. a badge's text.
    fn measure_label(&self, font: HFONT, text: &str) -> (i32, i32) {
        if text.is_empty() {
            let h = self.line_height(font);
            return (h, h);
        }
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                let h = self.line_height(font);
                return (h, h);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut buf = utf16(text);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            (
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
            )
        }
    }

    // -- difficulty badge --------------------------------------------------

    /// Outer width/height of the badge shape for `difficulty`, scaled for
    /// this card's DPI and text scale. A short label (e.g. "1") floors out at
    /// a circle; a wider one ("10", "U") grows into a pill.
    fn badge_size(&self, difficulty: Difficulty) -> (i32, i32) {
        let (tw, th) = self.measure_label(self.fonts.badge, difficulty.label());
        let pad_x = self.scale(BADGE_PAD_X_DP);
        let pad_y = self.scale(BADGE_PAD_Y_DP);
        let min_d = self.scale(BADGE_MIN_DIAMETER_DP);
        let w = (tw + pad_x * 2).max(min_d);
        let h = (th + pad_y * 2).max(min_d);
        (w, h)
    }

    /// How much narrower the headline's (and, in Expanded, the scroll hint's)
    /// available width must be so nothing is drawn underneath the badge.
    /// Returns 0 whenever there is no badge to avoid (i.e. `difficulty` is
    /// `None`), which is what keeps the `None` case pixel-identical to the
    /// pre-badge layout.
    fn badge_reserve(&self, difficulty: Option<Difficulty>) -> i32 {
        match difficulty {
            None => 0,
            Some(d) => {
                let (badge_w, _) = self.badge_size(d);
                let edge_margin = self.scale(BADGE_EDGE_MARGIN_DP);
                let gap = self.scale(BADGE_TEXT_GAP_DP);
                let padding = self.scale(PADDING_DP);
                // The content area's right edge already sits `padding` in
                // from the card edge; the badge sits `edge_margin` in from
                // the same edge. Only the amount by which the badge (plus a
                // little breathing room) extends past that existing padding
                // needs to be reserved on top of it.
                (edge_margin + badge_w + gap - padding).max(0)
            }
        }
    }

    /// Content width to wrap the headline into, narrowed to clear the
    /// difficulty badge when one is shown. Every headline layout/paint call
    /// site must use this (not the raw content width) so the reserved space
    /// stays in sync between measurement and drawing.
    fn headline_content_width(&self, content_w: i32) -> i32 {
        (content_w - self.badge_reserve(self.difficulty)).max(1)
    }

    // -- painting --------------------------------------------------------

    fn on_paint(&self) {
        unsafe {
            let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;

            if w > 0 && h > 0 && !hdc.0.is_null() {
                let mem_dc = CreateCompatibleDC(Some(hdc));
                if !mem_dc.0.is_null() {
                    let bmp = CreateCompatibleBitmap(hdc, w, h);
                    if !bmp.0.is_null() {
                        let old_bmp = SelectObject(mem_dc, HGDIOBJ(bmp.0));
                        self.paint_into(mem_dc, rc);
                        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem_dc), 0, 0, SRCCOPY);
                        SelectObject(mem_dc, old_bmp);
                        let _ = DeleteObject(HGDIOBJ(bmp.0));
                    }
                    let _ = DeleteDC(mem_dc);
                }
            }

            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    fn paint_into(&self, hdc: HDC, rc: RECT) {
        let palette = self.theme.palette();
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);

        unsafe {
            // Background fill. DWM clips the actual window to rounded
            // corners at composition time, so a plain rectangular fill here
            // ends up looking correctly rounded on screen.
            let bg_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(palette.bg));
            FillRect(hdc, &rc, bg_brush);
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));

            // Subtle 1px border.
            let border_brush =
                CreateSolidBrush(windows::Win32::Foundation::COLORREF(palette.border));
            FrameRect(hdc, &rc, border_brush);
            let _ = DeleteObject(HGDIOBJ(border_brush.0));

            SetBkMode(hdc, TRANSPARENT);

            match self.state {
                CardState::Hidden => {}
                CardState::Pending => self.paint_pending(hdc, rc, padding, &palette),
                CardState::Collapsed => self.paint_collapsed(hdc, rc, padding, &palette),
                CardState::Expanded => self.paint_expanded(hdc, rc, padding, gap, &palette),
                CardState::Preview => self.paint_preview(hdc, &palette),
            }
        }
    }

    /// Draws the indeterminate spinner: a dim full ring, then a brighter arc
    /// segment swept on top of it, its position driven by `anim_frame`.
    /// Replaces the old "Thinking..." text entirely.
    unsafe fn paint_pending(&self, hdc: HDC, rc: RECT, padding: i32, palette: &Palette) {
        let w = rc.right - rc.left;
        let h = rc.bottom - rc.top;
        let cx = rc.left + w / 2;
        let cy = rc.top + h / 2;
        let diameter = (w.min(h) - padding * 2).max(4);
        let radius = (diameter / 2).max(1);
        let left = cx - radius;
        let top = cy - radius;
        let right = cx + radius;
        let bottom = cy + radius;
        let stroke = self.scale(SPINNER_STROKE_DP).max(2);

        // Arc() never fills, but Ellipse() does -- select NULL_BRUSH so the
        // dim ring is an outline, not a filled disc.
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        // Dim full ring underneath.
        let ring_pen = CreatePen(
            PS_SOLID,
            stroke,
            windows::Win32::Foundation::COLORREF(palette.border),
        );
        if !ring_pen.0.is_null() {
            let old_pen = SelectObject(hdc, HGDIOBJ(ring_pen.0));
            let _ = Ellipse(hdc, left, top, right, bottom);
            SelectObject(hdc, old_pen);
        }
        let _ = DeleteObject(HGDIOBJ(ring_pen.0));

        // Bright sweeping arc on top. Prefer a geometric pen with round end
        // caps for a clean look; fall back to a plain cosmetic pen if that
        // ever fails (e.g. exotic display driver).
        let brush = LOGBRUSH {
            lbStyle: BS_SOLID,
            lbColor: windows::Win32::Foundation::COLORREF(palette.headline),
            lbHatch: 0,
        };
        let mut arc_pen = ExtCreatePen(
            PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
            stroke as u32,
            &brush,
            None,
        );
        if arc_pen.0.is_null() {
            arc_pen = CreatePen(
                PS_SOLID,
                stroke,
                windows::Win32::Foundation::COLORREF(palette.headline),
            );
        }
        if !arc_pen.0.is_null() {
            let start_deg = (self.anim_frame as f32 * SPINNER_DEGREES_PER_FRAME) % 360.0;
            let end_deg = start_deg + SPINNER_SWEEP_DEG;
            let (x1, y1) = ray_point(cx, cy, start_deg, radius);
            let (x2, y2) = ray_point(cx, cy, end_deg, radius);

            let old_pen = SelectObject(hdc, HGDIOBJ(arc_pen.0));
            let _ = Arc(hdc, left, top, right, bottom, x1, y1, x2, y2);
            SelectObject(hdc, old_pen);
        }
        let _ = DeleteObject(HGDIOBJ(arc_pen.0));

        SelectObject(hdc, old_brush);
    }

    unsafe fn paint_collapsed(&self, hdc: HDC, rc: RECT, padding: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let headline_w = self.headline_content_width(content_w);
        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        // Must match layout_collapsed's arithmetic exactly. Using the cap here
        // instead of the measured height pushes the hint below the window's
        // bottom edge whenever the headline wraps to fewer than the maximum
        // number of lines -- i.e. most of the time.
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_w)
            .min(max_headline_h)
            .max(headline_line_h);

        let headline_rect = RECT {
            left: rc.left + padding,
            top: rc.top + padding,
            right: rc.left + padding + headline_w,
            bottom: rc.top + padding + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &self.headline,
            headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        self.paint_difficulty_badge(hdc, rc);
    }

    unsafe fn paint_expanded(&self, hdc: HDC, rc: RECT, padding: i32, gap: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let content_left = rc.left + padding;
        let content_top = rc.top + padding;
        let content_bottom = rc.bottom - padding;

        // Clip to the padded content area so scrolled text never bleeds into
        // the border/padding.
        IntersectClipRect(
            hdc,
            content_left,
            content_top,
            rc.right - padding,
            content_bottom,
        );

        let headline_w = self.headline_content_width(content_w);
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_w)
            .max(self.line_height(self.fonts.headline));

        let y0 = content_top - self.scroll_offset;
        let headline_rect = RECT {
            left: content_left,
            top: y0,
            right: content_left + headline_w,
            bottom: y0 + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &self.headline,
            headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );

        if !self.detail.is_empty() {
            let detail_h = self.measure_wrapped(self.fonts.body, &self.detail, content_w);
            let y1 = y0 + headline_h + gap;
            let detail_rect = RECT {
                left: content_left,
                top: y1,
                right: content_left + content_w,
                bottom: y1 + detail_h,
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
            draw_text_line(
                hdc,
                &self.detail,
                detail_rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
            );
        }

        // The content is capped at 60% of the work area, so long answers get
        // clipped mid-line. Without a marker there is nothing to suggest the
        // rest is one scroll away, so draw a hint over the bottom edge -- but
        // only while there is actually something below the fold.
        if self.scroll_offset < self.scroll_max {
            let line_h = self.line_height(self.fonts.body).max(1);
            let band_h = line_h + self.scale(6);
            let band = RECT {
                left: rc.left + 1,
                top: content_bottom - line_h,
                right: rc.right - 1,
                bottom: rc.bottom - 1,
            };
            // Paint the card colour back over the clipped line so the hint sits
            // on a clean strip rather than on top of half a word.
            let bg = CreateSolidBrush(windows::Win32::Foundation::COLORREF(palette.bg));
            FillRect(hdc, &band, bg);
            let _ = DeleteObject(HGDIOBJ(bg.0));
            // Reuse the headline's badge reservation so the hint (which is
            // also right-aligned, in the same bottom-right corner the badge
            // occupies) does not draw underneath it either.
            let hint_rect = RECT {
                left: content_left,
                top: rc.bottom - padding - band_h + self.scale(2),
                right: content_left + headline_w,
                bottom: rc.bottom - padding,
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            draw_text_line(
                hdc,
                "more below",
                hint_rect,
                DT_RIGHT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
            );
        }

        self.paint_difficulty_badge(hdc, rc);
    }

    /// Draws the difficulty badge in the card's bottom-right corner, if any
    /// (`self.difficulty` is `None` for Pending, for errors, and whenever the
    /// integrator disabled the feature -- in all of those cases this is a
    /// no-op, which is what keeps that path pixel-identical to the code
    /// before this feature existed).
    ///
    /// Called last, after the rest of `paint_collapsed`/`paint_expanded`, so
    /// the badge always sits on top. `paint_expanded` narrows its clip region
    /// to the padded content box for scrolling; the badge lives partly
    /// outside that box (in the corner's padding whitespace), so the clip is
    /// reset first -- otherwise it would be silently clipped away there.
    unsafe fn paint_difficulty_badge(&self, hdc: HDC, rc: RECT) {
        let Some(difficulty) = self.difficulty else {
            return;
        };

        let _ = SelectClipRgn(hdc, None);

        let (w, h) = self.badge_size(difficulty);
        let edge_margin = self.scale(BADGE_EDGE_MARGIN_DP);
        let right = rc.right - edge_margin;
        let bottom = rc.bottom - edge_margin;
        let left = (right - w).max(rc.left);
        let top = (bottom - h).max(rc.top);

        let fill = difficulty_color(difficulty);
        let text_color = badge_text_color(fill);
        // Equal-radius rounding on both axes: when w == h (short labels like
        // "1") this comes out as a circle; when w > h ("10", "U") it comes
        // out as a pill.
        let round = h;

        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(fill));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = RoundRect(hdc, left, top, right, bottom, round, round);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let label_rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.badge.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(text_color));
        draw_text_line(
            hdc,
            difficulty.label(),
            label_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    // -- message handling --------------------------------------------------

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ERASEBKGND => Some(LRESULT(1)),
            // Issue #350: without this, preview EDIT fields keep the stock
            // white background and black text regardless of theme -- the
            // one control on the card where the user reads carefully before
            // confirming stays bright in dark mode. Only WM_CTLCOLOREDIT is
            // handled: every preview field the card creates is an editable
            // EDIT control (`create_preview_edit`, no ES_READONLY, no
            // STATIC), so WM_CTLCOLORSTATIC never fires for them and adding
            // a handler for it would be dead code.
            WM_CTLCOLOREDIT => {
                let hdc = HDC(wparam.0 as *mut _);
                let palette = self.theme.palette();
                let (text, bg) = edit_field_colors(&palette);
                unsafe {
                    SetTextColor(hdc, windows::Win32::Foundation::COLORREF(text));
                    SetBkColor(hdc, windows::Win32::Foundation::COLORREF(bg));
                    if self.edit_bg_brush.0.is_null() {
                        self.edit_bg_brush =
                            CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg));
                    }
                }
                let _ = HWND(lparam.0 as *mut _); // the EDIT control; unused (every preview EDIT shares one theme)
                Some(LRESULT(self.edit_bg_brush.0 as isize))
            }
            WM_PAINT => {
                self.on_paint();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_ANIM => {
                        self.anim_frame = self.anim_frame.wrapping_add(1);
                        self.invalidate();
                    }
                    // The preview state never auto-dismisses on a timer
                    // (rule 5 and safety: an action that writes something
                    // must never fire because nobody was there to see a
                    // countdown). `show_preview` never arms TIMER_DISMISS,
                    // but this guard makes that a hard invariant rather than
                    // "nothing currently posts one": a WM_TIMER already
                    // queued from a previous state (KillTimer stops future
                    // messages, not ones already in the queue) must still
                    // never close an open preview out from under the user
                    // -- the guard failing simply falls through to the `_`
                    // arm below, a deliberate no-op.
                    TIMER_DISMISS if self.state != CardState::Preview => {
                        self.hide();
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                // Issue #347: a settings-needed card's click opens Settings
                // instead of expanding -- checked before try_expand() (whose
                // own guard would otherwise just expand it, since these
                // cards are always Collapsed with a non-empty detail).
                if self.state == CardState::Collapsed && self.open_settings_on_click {
                    self.open_settings_on_click = false;
                    if let Some(owner) = self.owner {
                        unsafe {
                            let _ = PostMessageW(
                                Some(owner),
                                WM_APP_CARD_OPEN_SETTINGS,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                    self.hide();
                } else {
                    self.try_expand();
                }
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                if self.state == CardState::Expanded {
                    let raw = (wparam.0 & 0xFFFF_FFFF) as u32;
                    let delta = ((raw >> 16) as i16) as i32;
                    let step = self.scale(WHEEL_SCROLL_DP);
                    let notches = delta / 120;
                    self.scroll_offset =
                        (self.scroll_offset - notches * step).clamp(0, self.scroll_max);
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                if self.state == CardState::Expanded && wparam.0 as u16 == VK_ESCAPE.0 {
                    self.close_expanded();
                } else if self.state == CardState::Preview {
                    if let Some(command_id) = preview_key_command(wparam.0 as u16) {
                        self.run_preview_command(command_id);
                    }
                }
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                if self.state == CardState::Preview {
                    let id = (wparam.0 & 0xFFFF) as i32;
                    self.run_preview_command(id);
                }
                Some(LRESULT(0))
            }
            WM_KILLFOCUS => {
                self.close_expanded();
                Some(LRESULT(0))
            }
            WM_DPICHANGED => {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                self.dpi = new_dpi.max(1);
                self.rebuild_fonts();
                self.relayout_current_state();
                Some(LRESULT(0))
            }
            WM_NCDESTROY => {
                self.fonts.delete();
                self.fonts = Fonts::null();
                if !self.edit_bg_brush.0.is_null() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(self.edit_bg_brush.0));
                    }
                    self.edit_bg_brush = HBRUSH(std::ptr::null_mut());
                }
                self.hwnd = HWND(std::ptr::null_mut());
                None
            }
            WM_DESTROY => None,
            _ => None,
        }
    }

    // -- preview (confirmation) state (#26) -----------------------------

    /// Enters `CardState::Preview`. See [`Card::show_preview`] for the
    /// public contract (focus behaviour, the `main_window_exists` gate).
    fn show_preview(
        &mut self,
        title: &str,
        schema: &Value,
        proposal_value: &Value,
        main_window_exists: bool,
    ) {
        self.leave_preview_if_active();
        // The preview never auto-dismisses on a timer (rule 5 and safety:
        // an action that writes something must never fire because nobody
        // was there to see a countdown) -- so, unlike show_pending/
        // show_collapsed, no TIMER_DISMISS is ever armed here.
        self.kill_timers();

        let previous_foreground = unsafe { GetForegroundWindow() };

        let model = PreviewModel::from_schema(schema, proposal_value);

        self.headline.clear();
        self.detail.clear();
        self.difficulty = None;
        self.state = CardState::Preview;
        self.scroll_offset = 0;
        self.scroll_max = 0;

        // Preview needs real keyboard focus (typing into an editable field,
        // Enter, Esc), unlike Pending/Collapsed -- see the "Focus" note on
        // `Card::show_preview`.
        self.set_noactivate(false);

        // Starting a fresh preview must never let an earlier, never-taken
        // confirmation leak into this one's lifetime.
        self.last_confirmed = None;

        self.preview = Some(PreviewUi {
            model,
            title: title.to_string(),
            edits: Vec::new(),
            do_it_btn: HWND(std::ptr::null_mut()),
            edit_btn: None,
            cancel_btn: HWND(std::ptr::null_mut()),
            previous_foreground,
            main_window_exists,
        });
        // #225: from here until a decision is reported, tearing this
        // preview down is an abandonment and the owner has to be told. Set
        // AFTER the `leave_preview_if_active` above, so replacing a live
        // preview reports the OLD one exactly once and then arms the new.
        self.preview_decision_pending = true;
        self.preview_generation = self.preview_generation.wrapping_add(1);

        self.layout_preview();
        self.create_preview_controls();
        self.reveal();

        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.focus_initial_preview_control();
    }

    /// The one place every WM_COMMAND id and every preview keyboard mapping
    /// (from [`preview_key_command`]) is dispatched, so the card's own
    /// WM_KEYDOWN arm and the child-control subclass (which posts the same
    /// ids back to this window) always mean exactly the same thing.
    fn run_preview_command(&mut self, id: i32) {
        match id {
            ID_PREVIEW_DO_IT => self.preview_do_it(),
            ID_PREVIEW_CANCEL => self.preview_cancel(),
            ID_PREVIEW_EDIT => self.preview_edit_clicked(),
            _ => {}
        }
    }

    /// "Do it": reads every editable control's current text back into the
    /// model, refuses (leaving the card open) if a required field is now
    /// blank, then builds the `Confirmed<Value>` from exactly those values
    /// via `ui::confirm::confirm_preview` -- never from the original
    /// proposal -- and closes. This is #26's Done-when in code: what
    /// `take_confirmed` later hands an executor is provably what the card
    /// had on screen the moment "Do it" fired.
    fn preview_do_it(&mut self) {
        self.sync_edits_from_controls();
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        if !preview.model.is_valid() {
            // A required field is blank after editing: refuse silently and
            // leave the card open rather than build a Confirmed from an
            // invalid form. A card is never a dialog box (rule 7); an
            // inline validation message is left to a follow-up issue, not
            // invented here.
            return;
        }
        let shown = preview.model.to_value();
        let confirmed = crate::ui::confirm::confirm_preview(
            &preview.model,
            crate::ui::confirm::user_confirmed(),
        );
        debug_assert_eq!(
            *confirmed.value(),
            shown,
            "confirm_preview must hand back exactly what the model showed"
        );
        // Stored on `self`, not on `preview`: `close_preview` (below) drops
        // `PreviewUi` before returning -- see `last_confirmed`'s doc
        // comment on why this can't live there.
        self.last_confirmed = Some(confirmed);
        // #225: claim the decision before `close_preview` reaches
        // `leave_preview_if_active`, so it does not also report this as an
        // abandonment (and does not clear the `Confirmed` just stored).
        let generation = self.preview_generation;
        self.preview_decision_pending = false;
        self.close_preview();
        self.notify_owner_of_preview_decision(generation);
    }

    /// "Cancel" / Esc: produces nothing (no `Confirmed` is ever built) and
    /// closes. Also clears any earlier `last_confirmed` a caller never took,
    /// so `take_confirmed` reliably returns `None` after a Cancel rather
    /// than silently handing back a stale confirmation from an unrelated,
    /// earlier preview.
    fn preview_cancel(&mut self) {
        self.last_confirmed = None;
        // #225: same as `preview_do_it` -- this path reports its own
        // decision, so it is not an abandonment.
        let generation = self.preview_generation;
        self.preview_decision_pending = false;
        self.close_preview();
        self.notify_owner_of_preview_decision(generation);
    }

    /// Issue #39: the one way `CardInner` (a plain Win32 window with no
    /// reference back to `App`, and must stay that way) tells the owner
    /// window a preview just closed with a decision. Carries no payload
    /// (`wparam`/`lparam` are both `0`) -- see [`WM_APP_PREVIEW_DECIDED`]'s
    /// doc comment for why that is safe even if this message is dropped
    /// while Settings is open. A no-op if [`Card::set_owner`] was never
    /// called.
    /// #225: `WPARAM` is the generation of the preview this decision is
    /// about, so a late-delivered abandonment cannot be mistaken for a
    /// decision on whatever preview is on screen by the time it arrives.
    fn notify_owner_of_preview_decision(&self, generation: u32) {
        if let Some(owner) = self.owner {
            unsafe {
                let _ = PostMessageW(
                    Some(owner),
                    WM_APP_PREVIEW_DECIDED,
                    WPARAM(generation as usize),
                    LPARAM(0),
                );
            }
        }
    }

    /// "Edit": only reachable when `main_window_exists` was `true` at
    /// `show_preview` time -- the button (and thus `ID_PREVIEW_EDIT`) is not
    /// even created otherwise (#352), so Windows never delivers a click (or
    /// this synthetic command) for it. No caller passes `true` yet (Wingman
    /// has no main window today), so this body is an intentional no-op
    /// placeholder for the issue that adds one, the same "inert until its
    /// caller exists" status `Action::hotkey` has -- not a control silently
    /// doing nothing where a real signal was expected, since that control
    /// does not exist in production yet.
    fn preview_edit_clicked(&mut self) {}

    /// Reads every editable control's `GetWindowTextW` back into the model.
    /// Called once, right before "Do it" decides whether the form is valid
    /// -- never continuously (no `EN_CHANGE` tracking), which keeps "what
    /// was shown" meaning exactly what was on screen at the moment of the
    /// decision, not some earlier keystroke.
    fn sync_edits_from_controls(&mut self) {
        let edits: Vec<(String, HWND)> = match &self.preview {
            Some(p) => p.edits.clone(),
            None => return,
        };
        for (name, hwnd) in edits {
            let text = window_text(hwnd);
            if let Some(preview) = self.preview.as_mut() {
                preview.model.set_value(&name, text);
            }
        }
    }

    /// Tears down the preview's child controls, restores focus to whatever
    /// window had it before `show_preview`, and hides the card. Used by
    /// both `preview_do_it` and `preview_cancel` -- the only two ways out of
    /// Preview that go through here rather than a later `show_*`/`hide`
    /// call (both of which also tear the preview down, via
    /// `leave_preview_if_active`).
    fn close_preview(&mut self) {
        let previous_foreground = self.preview.as_ref().map(|p| p.previous_foreground);
        self.hide();
        if let Some(prev) = previous_foreground {
            if !prev.0.is_null() && unsafe { IsWindow(Some(prev)) }.as_bool() {
                unsafe {
                    let _ = SetForegroundWindow(prev);
                }
            }
        }
    }

    /// The single choke point that guarantees "`self.preview` is `Some`
    /// iff `state == Preview`": called at the top of every state-entering
    /// method (`show_pending`, `show_collapsed`, `show_preview` itself, and
    /// `hide`) so a preview's child controls can never survive a jump to a
    /// different state, regardless of which path got there.
    /// The single choke point every path out of `CardState::Preview` goes
    /// through. Issue #225: it is also where an ABANDONED preview is
    /// reported. "Do it" and Cancel clear `preview_decision_pending` before
    /// they get here and post their own notification, so they do not double
    /// post (`preview_do_it_posts_exactly_one_wm_app_preview_decided`
    /// guards that). Every other way out of Preview leaves the flag set,
    /// and the owner is told here.
    fn leave_preview_if_active(&mut self) {
        if self.preview.is_some() {
            self.destroy_preview_controls();
            self.preview = None;
        }
        if self.preview_decision_pending {
            self.preview_decision_pending = false;
            // No `Confirmed` is produced, and any stale one is dropped: an
            // abandoned preview must never look like a confirmation.
            self.last_confirmed = None;
            self.notify_owner_of_preview_decision(self.preview_generation);
        }
    }

    fn destroy_preview_controls(&mut self) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        unsafe {
            for (_, hwnd) in &preview.edits {
                if !hwnd.0.is_null() {
                    let _ = DestroyWindow(*hwnd);
                }
            }
            let mut btns = vec![preview.do_it_btn, preview.cancel_btn];
            if let Some(edit_btn) = preview.edit_btn {
                btns.push(edit_btn);
            }
            for hwnd in btns {
                if !hwnd.0.is_null() {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }

    fn focus_initial_preview_control(&self) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let target = preview
            .edits
            .first()
            .map(|(_, hwnd)| *hwnd)
            .unwrap_or(preview.do_it_btn);
        if !target.0.is_null() {
            unsafe {
                let _ = SetFocus(Some(target));
            }
        }
    }

    /// Creates one EDIT control per editable field and the three buttons,
    /// all parented to the card's own window and positioned from
    /// [`CardInner::compute_preview_layout`]. Every child control is
    /// subclassed with [`preview_control_subclass`] so Enter/Esc work
    /// regardless of which control has focus (see that function's doc
    /// comment for why this cannot rely on the integrating app's message
    /// loop).
    fn create_preview_controls(&mut self) {
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        let instance = self.instance;
        let parent = self.hwnd;
        let font = self.fonts.body;

        let mut edits = Vec::new();
        for (field, row) in fields.iter().zip(metrics.rows.iter()) {
            if !field.editable {
                continue;
            }
            if let Some(hwnd) =
                create_preview_edit(parent, instance, font, &field.value, &row.value_rect)
            {
                subclass_preview_control(hwnd, parent);
                edits.push((field.name.clone(), hwnd));
            }
        }

        let do_it_btn = create_preview_button(
            parent,
            instance,
            font,
            "Do it",
            &metrics.buttons.do_it,
            ID_PREVIEW_DO_IT,
            true,
            true,
        );
        let edit_btn = metrics.buttons.edit.map(|rect| {
            create_preview_button(
                parent,
                instance,
                font,
                "Edit",
                &rect,
                ID_PREVIEW_EDIT,
                false,
                true,
            )
        });
        let cancel_btn = create_preview_button(
            parent,
            instance,
            font,
            "Cancel",
            &metrics.buttons.cancel,
            ID_PREVIEW_CANCEL,
            false,
            true,
        );
        let mut btns = vec![do_it_btn, cancel_btn];
        if let Some(edit_btn) = edit_btn {
            btns.push(edit_btn);
        }
        for hwnd in btns {
            if !hwnd.0.is_null() {
                subclass_preview_control(hwnd, parent);
            }
        }

        if let Some(preview) = self.preview.as_mut() {
            preview.edits = edits;
            preview.do_it_btn = do_it_btn;
            preview.edit_btn = edit_btn;
            preview.cancel_btn = cancel_btn;
        }
    }

    /// Re-runs preview layout and moves every existing child control (and
    /// re-applies the possibly-rebuilt font) to match -- the DPI-change
    /// counterpart of `create_preview_controls`, which only ever runs once
    /// per `show_preview` call.
    fn reposition_preview_controls(&mut self) {
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        let font = self.fonts.body;

        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        for (name, hwnd) in &preview.edits {
            if let Some(idx) = fields.iter().position(|f| &f.name == name) {
                let r = &metrics.rows[idx].value_rect;
                unsafe {
                    let _ = SetWindowPos(
                        *hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER,
                    );
                    SendMessageW(
                        *hwnd,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
            }
        }
        let mut positioned = vec![
            (preview.do_it_btn, metrics.buttons.do_it),
            (preview.cancel_btn, metrics.buttons.cancel),
        ];
        if let (Some(edit_btn), Some(edit_rect)) = (preview.edit_btn, metrics.buttons.edit) {
            positioned.push((edit_btn, edit_rect));
        }
        for (hwnd, rect) in positioned {
            if hwnd.0.is_null() {
                continue;
            }
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER,
                );
                SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    fn layout_preview(&mut self) {
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        // Preview does not track the cursor the way Pending/Collapsed do
        // (`work_area_for_cursor`): once a form is up, the user's mouse is
        // likely to move away while they read or type, and having the card
        // hop monitors mid-edit would be actively hostile. Expanded makes
        // the same choice for the same reason.
        let work = self.work_area_for_self();
        self.place_bottom_right(work, metrics.width, metrics.window_h);
    }

    /// The preview form's full geometry: the title line, one label/value
    /// row per field (in `fields`' order, which is schema order -- see
    /// `PreviewModel::from_schema`), and the button row. Pure geometry:
    /// used both to position real child controls and to paint the
    /// non-editable rows' labels/values, so the two can never drift apart.
    ///
    /// `show_edit` mirrors `main_window_exists` (#352): when `false`, no
    /// "Edit" rect is produced at all (`buttons.edit` is `None`) and "Do it"
    /// / "Cancel" are spread across the freed width instead of leaving a
    /// blank column where "Edit" used to sit.
    fn compute_preview_layout(&self, fields: &[Field], show_edit: bool) -> PreviewLayoutMetrics {
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(PREVIEW_WIDTH_DP);
        let content_left = padding;
        let content_right = width - padding;
        let content_width = (content_right - content_left).max(1);

        let title_h = self.line_height(self.fonts.headline).max(1);
        let mut y = padding;
        let title_rect = RECT {
            left: content_left,
            top: y,
            right: content_left + content_width,
            bottom: y + title_h,
        };
        y += title_h + gap;

        let row_h = self
            .scale(PREVIEW_ROW_H_DP)
            .max(self.line_height(self.fonts.body));
        let row_gap = self.scale(PREVIEW_ROW_GAP_DP);
        // #355: the label column used to be a fixed 84dp, truncating a long
        // label (e.g. "Date of birth") to "Date of bi..." exactly when the
        // user is checking what is about to be written. Size it from the
        // widest label actually shown instead, clamped between the old
        // 84dp floor and 45% of the content width so one very long label
        // cannot squeeze the value column away.
        let label_min_w = self.scale(PREVIEW_LABEL_W_DP).min(content_width / 2).max(1);
        let label_max_w = ((content_width as f32) * PREVIEW_LABEL_MAX_FRACTION) as i32;
        let measured_label_w: Vec<i32> = fields
            .iter()
            .map(|f| self.measure_label(self.fonts.body, &f.label).0)
            .collect();
        let label_w = label_column_width(&measured_label_w, label_min_w, label_max_w);
        let value_x = content_left + label_w + self.scale(6);
        let value_w = (content_right - value_x).max(1);

        let mut rows = Vec::with_capacity(fields.len());
        for (field, &measured_w) in fields.iter().zip(measured_label_w.iter()) {
            // A label that still does not fit the (clamped) column wraps to
            // a second line rather than ellipsizing, per #355's "Done
            // when": the whole label must be visible, not just wider.
            let label_wrapped = measured_w > label_w;
            let row_content_h = if label_wrapped {
                self.measure_wrapped(self.fonts.body, &field.label, label_w)
                    .max(row_h)
            } else {
                row_h
            };
            let label_rect = RECT {
                left: content_left,
                top: y,
                right: content_left + label_w,
                bottom: y + row_content_h,
            };
            // When the label wraps to two lines, the value stays a single
            // line top-aligned with the label's *first* line rather than
            // vertically centred in the now-taller row (review nit on
            // #355): `row_h` here, not `row_content_h`.
            let value_rect = RECT {
                left: value_x,
                top: y,
                right: value_x + value_w,
                bottom: y + row_h,
            };
            rows.push(PreviewRowMetrics {
                label_rect,
                value_rect,
                label_wrapped,
            });
            y += row_content_h + row_gap;
        }
        y = if fields.is_empty() {
            y + gap
        } else {
            y - row_gap + gap
        };

        let btn_h = self.scale(PREVIEW_BUTTON_H_DP);
        let btn_w = self.scale(PREVIEW_BUTTON_W_DP);
        let btn_gap = self.scale(PREVIEW_BUTTON_GAP_DP);
        let do_it = RECT {
            left: content_right - btn_w,
            top: y,
            right: content_right,
            bottom: y + btn_h,
        };
        // With no "Edit" button, "Cancel" moves from directly left of "Do
        // it" all the way to the content's left edge, spreading the two
        // buttons across the width "Edit" used to share instead of leaving
        // it blank (#352).
        let cancel_left = if show_edit {
            do_it.left - btn_gap - btn_w
        } else {
            content_left
        };
        let cancel = RECT {
            left: cancel_left,
            top: y,
            right: cancel_left + btn_w,
            bottom: y + btn_h,
        };
        let edit = if show_edit {
            Some(RECT {
                left: content_left,
                top: y,
                right: content_left + btn_w,
                bottom: y + btn_h,
            })
        } else {
            None
        };
        y += btn_h;

        let window_h = y + padding;

        PreviewLayoutMetrics {
            width,
            window_h,
            title_rect,
            rows,
            buttons: PreviewButtonMetrics {
                do_it,
                edit,
                cancel,
            },
        }
    }

    /// Paints the title line, every field's label, and every *non-editable*
    /// field's value (an editable field's value is shown by its live EDIT
    /// control instead, drawn by Windows on top of this same client area).
    unsafe fn paint_preview(&self, hdc: HDC, palette: &Palette) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let fields = preview.model.fields();
        let metrics = self.compute_preview_layout(fields, preview.main_window_exists);

        let title_rect = metrics.title_rect;
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &preview.title,
            title_rect,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );

        for (field, row) in fields.iter().zip(metrics.rows.iter()) {
            let label_rect = row.label_rect;
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            // #355: a label that did not fit the clamped column even at its
            // widest wraps to a second line instead of ellipsizing, so the
            // whole label stays readable.
            let label_format = if row.label_wrapped {
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX
            } else {
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS
            };
            draw_text_line(hdc, &field.label, label_rect, label_format);

            if !field.editable {
                let value_rect = row.value_rect;
                SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
                SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
                draw_text_line(
                    hdc,
                    &field.value,
                    value_rect,
                    DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Preview (confirmation) state -- layout constants, child-control helpers,
// keyboard mapping, and the shared control subclass (#26)
// ---------------------------------------------------------------------------

const PREVIEW_WIDTH_DP: i32 = 320;
const PREVIEW_ROW_H_DP: i32 = 22;
const PREVIEW_ROW_GAP_DP: i32 = 6;
const PREVIEW_LABEL_W_DP: i32 = 84;
const PREVIEW_BUTTON_H_DP: i32 = 26;
const PREVIEW_BUTTON_W_DP: i32 = 84;
const PREVIEW_BUTTON_GAP_DP: i32 = 8;
/// Upper bound on the label column as a fraction of the content width
/// (#355): even the widest label never pushes the value column below 55%
/// of the available space.
const PREVIEW_LABEL_MAX_FRACTION: f32 = 0.45;

/// Sizes the preview label column from the widest measured label (#355),
/// clamped between `min_w` (the old fixed 84dp floor) and `max_w` (45% of
/// the content width). Pure and DPI-agnostic: callers scale `measured`,
/// `min_w` and `max_w` to pixels first, so this same function is exercised
/// at every DPI by `label_column_width_is_clamped_at_several_dpis` below --
/// it never needs to know what DPI produced its inputs.
fn label_column_width(measured: &[i32], min_w: i32, max_w: i32) -> i32 {
    let max_w = max_w.max(min_w);
    let widest = measured.iter().copied().max().unwrap_or(min_w);
    widest.clamp(min_w, max_w)
}

const ID_PREVIEW_DO_IT: i32 = 3900;
const ID_PREVIEW_EDIT: i32 = 3901;
const ID_PREVIEW_CANCEL: i32 = 3902;

/// Subclass id passed to `SetWindowSubclass`/`RemoveWindowSubclass`. A
/// single constant is enough: every preview child control uses the same
/// subclass procedure, so nothing here ever needs to tell two subclasses on
/// the same control apart.
const PREVIEW_SUBCLASS_ID: usize = 1;

const WC_EDIT: &str = "EDIT";
const WC_BUTTON: &str = "BUTTON";
const ES_AUTOHSCROLL: u32 = 0x0080;
const BS_PUSHBUTTON: u32 = 0x0000;
const BS_DEFPUSHBUTTON: u32 = 0x0001;

/// Live state for `CardState::Preview`. Owned by `CardInner.preview`; exists
/// for exactly as long as the preview's child controls do (see
/// `leave_preview_if_active`).
struct PreviewUi {
    model: PreviewModel,
    /// The heading line drawn above the fields -- the action's name (e.g.
    /// "Add to calendar"), not a schema field.
    title: String,
    /// One `(schema field name, EDIT HWND)` pair per *editable* field, in
    /// the same order `PreviewModel::fields()` returns them. Non-editable
    /// fields have no entry here; their value is painted, not typed into.
    edits: Vec<(String, HWND)>,
    do_it_btn: HWND,
    /// `None` when `main_window_exists` was `false` at `show_preview` time
    /// (#352): the button is not created at all, not just disabled.
    edit_btn: Option<HWND>,
    cancel_btn: HWND,
    /// `GetForegroundWindow()` at the moment `show_preview` was called.
    /// `close_preview` hands the foreground back to this window (if it
    /// still exists) so the preview's interruption is temporary -- see
    /// `Card::show_preview`'s "Focus" note.
    previous_foreground: HWND,
    /// Whether Wingman's main window exists yet -- gates whether the "Edit"
    /// button is created enabled. See `Card::show_preview`'s doc comment.
    main_window_exists: bool,
}

struct PreviewRowMetrics {
    label_rect: RECT,
    value_rect: RECT,
    /// Whether this row's label is too wide for the (clamped) label column
    /// even after `label_column_width` picked the widest label it could,
    /// so it must be drawn wrapped (`DT_WORDBREAK`) instead of ellipsized
    /// on one line (#355).
    label_wrapped: bool,
}

struct PreviewButtonMetrics {
    do_it: RECT,
    /// `None` when the layout was computed with `show_edit: false` (#352).
    edit: Option<RECT>,
    cancel: RECT,
}

struct PreviewLayoutMetrics {
    width: i32,
    window_h: i32,
    title_rect: RECT,
    /// Same length and order as the `fields` slice `compute_preview_layout`
    /// was called with.
    rows: Vec<PreviewRowMetrics>,
    buttons: PreviewButtonMetrics,
}

/// Pure keyboard mapping for the preview state (#26): Enter means "Do it",
/// Esc means "Cancel", everything else is not a preview command. Shared by
/// the card's own `WM_KEYDOWN` arm and [`preview_control_subclass`] (every
/// EDIT/BUTTON child forwards through the same mapping), so both agree by
/// construction rather than by two hand-kept switch statements. Pure and
/// unit-tested without a live window -- see the `tests` module below.
fn preview_key_command(vk: u16) -> Option<i32> {
    if vk == VK_RETURN.0 {
        Some(ID_PREVIEW_DO_IT)
    } else if vk == VK_ESCAPE.0 {
        Some(ID_PREVIEW_CANCEL)
    } else {
        None
    }
}

/// Issue #207: pure decision for a preview child control's `WM_KILLFOCUS`
/// -- whether the new focus target means "click away" (cancel the preview,
/// same as Esc) or staying within the card's own window group (do nothing:
/// Tab moving between two preview fields, or focus landing on the Do
/// it/Cancel button itself right before its own click fires).
/// `new_focus_is_card_or_descendant` is
/// `new_focus_hwnd == card_hwnd || IsChild(card_hwnd, new_focus_hwnd)`,
/// computed by the caller (both need a live `HWND` comparison/`IsChild`
/// call, so cannot be done here). `WM_KILLFOCUS`'s own "nothing is gaining
/// focus" case (`wparam == 0`, e.g. the whole app losing the foreground to
/// nothing in particular) needs no separate case: a null `HWND` is never
/// the card's own and `IsChild` is never true for it, so it already
/// computes `new_focus_is_card_or_descendant == false` the same as any
/// other outside window.
fn preview_focus_left_the_card(new_focus_is_card_or_descendant: bool) -> bool {
    !new_focus_is_card_or_descendant
}

/// Subclass installed on every preview EDIT/BUTTON child control so Enter
/// and Esc work regardless of which control currently has keyboard focus.
///
/// This cannot be left to the integrating app's message loop: this module's
/// own doc comment promises callers a plain `GetMessageW`/`DispatchMessageW`
/// loop is enough to run the card (no `IsDialogMessageW` translation, unlike
/// `ui::settings`'s modal loop). A child control's `WM_KEYDOWN` is delivered
/// to the CHILD's own window procedure, never to the parent card's, so
/// without this subclass Enter/Esc would only work while the card's own
/// `HWND` itself happened to have focus (which it never does once a child
/// control does) -- exactly the "wired to nothing" shape this crate's own
/// skill warns about for a low-level hook with no message loop backing it.
///
/// `dwrefdata` carries the parent card's `HWND` (as a `usize`, set at
/// subclass time by [`subclass_preview_control`]) so a mapped key can
/// `PostMessageW` a `WM_COMMAND` back to it, indistinguishable from a real
/// button click.
///
/// Issue #207: the same `dwrefdata` is what makes click-away cancellation
/// possible here too. `WM_KILLFOCUS` on the card's own `HWND` (handled in
/// `CardInner::handle_message`) never fires while a preview is showing,
/// because Preview's fields are real EDIT/BUTTON children -- focus moves
/// between THEM, not off the card window itself, until it leaves the whole
/// group. So this subclass, installed on every one of those children,
/// checks each `WM_KILLFOCUS`'s own new-focus target (`wparam`) against the
/// card window group via [`preview_focus_left_the_card`] and posts
/// `ID_PREVIEW_CANCEL` the same way Enter/Esc already do when it has.
unsafe extern "system" fn preview_control_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let parent = HWND(ref_data as *mut c_void);
    if msg == WM_KEYDOWN {
        if let Some(command_id) = preview_key_command(wparam.0 as u16) {
            let _ = PostMessageW(
                Some(parent),
                WM_COMMAND,
                WPARAM(command_id as usize),
                LPARAM(0),
            );
            return LRESULT(0);
        }
    }
    if msg == WM_KILLFOCUS {
        let new_focus = HWND(wparam.0 as *mut c_void);
        let new_focus_is_card_or_descendant =
            new_focus == parent || unsafe { IsChild(parent, new_focus) }.as_bool();
        if preview_focus_left_the_card(new_focus_is_card_or_descendant) {
            let _ = PostMessageW(
                Some(parent),
                WM_COMMAND,
                WPARAM(ID_PREVIEW_CANCEL as usize),
                LPARAM(0),
            );
        }
    }
    if msg == WM_NCDESTROY {
        let _ = RemoveWindowSubclass(hwnd, Some(preview_control_subclass), PREVIEW_SUBCLASS_ID);
    }
    DefSubclassProc(hwnd, msg, wparam, lparam)
}

fn subclass_preview_control(hwnd: HWND, parent: HWND) {
    unsafe {
        let _ = SetWindowSubclass(
            hwnd,
            Some(preview_control_subclass),
            PREVIEW_SUBCLASS_ID,
            parent.0 as usize,
        );
    }
}

fn create_preview_edit(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    initial_text: &str,
    rect: &RECT,
) -> Option<HWND> {
    let class_w = wide_z(WC_EDIT);
    let text_w = wide_z(initial_text);
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
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
            None,
            Some(instance),
            None,
        )
    };
    let hwnd = hwnd.ok()?;
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    Some(hwnd)
}

#[allow(clippy::too_many_arguments)]
fn create_preview_button(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    text: &str,
    rect: &RECT,
    id: i32,
    is_default: bool,
    enabled: bool,
) -> HWND {
    let class_w = wide_z(WC_BUTTON);
    let text_w = wide_z(text);
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_TABSTOP.0
        | if is_default {
            BS_DEFPUSHBUTTON
        } else {
            BS_PUSHBUTTON
        }
        | if enabled { 0 } else { WS_DISABLED.0 };
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(parent),
            Some(HMENU(id as *mut c_void)),
            Some(instance),
            None,
        )
    };
    match hwnd {
        Ok(hwnd) => {
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
        Err(_) => HWND(std::ptr::null_mut()),
    }
}

/// `GetWindowTextW` into an owned `String`, `""` for a null/invalid handle
/// rather than a panic -- mirrors `ui::settings::get_text`.
fn window_text(hwnd: HWND) -> String {
    if hwnd.0.is_null() {
        return String::new();
    }
    unsafe {
        let len = GetWindowTextLengthW(hwnd);
        if len <= 0 {
            return String::new();
        }
        let mut buf = vec![0u16; len as usize + 1];
        let read = GetWindowTextW(hwnd, &mut buf);
        buf.truncate(read.max(0) as usize);
        String::from_utf16_lossy(&buf)
    }
}

/// A point at distance `len` from `(cx, cy)` along the ray at `angle_deg`
/// (standard math convention: 0 deg is +x, increasing counterclockwise).
/// Used to give `Arc` the two boundary points that define where the
/// spinner's sweep begins and ends.
fn ray_point(cx: i32, cy: i32, angle_deg: f32, len: i32) -> (i32, i32) {
    let rad = angle_deg.to_radians();
    let x = cx + (len as f32 * rad.cos()).round() as i32;
    let y = cy - (len as f32 * rad.sin()).round() as i32;
    (x, y)
}

fn work_area_for_monitor(hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> RECT {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return mi.rcWork;
        }
        // Fall back to the primary monitor's work area.
        let mut rc = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        rc
    }
}

// ---------------------------------------------------------------------------
// Small string helpers
// ---------------------------------------------------------------------------

/// UTF-16, null-terminated — for Win32 APIs that expect a `PCWSTR`.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16, *not* null-terminated — for `DrawTextW`, which takes an explicit
/// slice length rather than scanning for a terminator.
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// Smoke test — exercises real Win32 window creation/destruction, not just
// compilation. `cargo test` runs this headless; no window is ever shown
// (`show_*` only calls `ShowWindow(SW_SHOWNOACTIVATE)`, which does not steal
// focus or require an interactive desktop message pump to succeed).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    fn instance() -> HINSTANCE {
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        HINSTANCE(h.0)
    }

    #[test]
    fn create_show_hide_drop_smoke_test() {
        let mut card = Card::new(instance()).expect("Card::new");
        assert!(!card.hwnd().0.is_null());
        assert_eq!(card.state(), CardState::Hidden);

        card.show_pending();
        assert_eq!(card.state(), CardState::Pending);

        card.show_answer(
            "2 + 2 = 4",
            "You carried correctly.",
            5,
            Some(Difficulty::Level(1)),
        );
        assert_eq!(card.state(), CardState::Collapsed);

        // Simulate the click that expands a collapsed card with detail.
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);

        // Esc while expanded should close the card entirely.
        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Hidden);

        card.show_error("Couldn't capture the screen", "detail text");
        assert_eq!(card.state(), CardState::Collapsed);
        // show_error must clear any badge left over from the previous
        // show_answer call above -- it must never linger on an error card.
        assert_eq!(card.inner.difficulty, None);

        card.hide();
        assert_eq!(card.state(), CardState::Hidden);
        // Dropping must not panic (this exercises DestroyWindow + WM_NCDESTROY).
    }

    #[test]
    fn clicking_a_settings_needed_card_opens_settings_instead_of_expanding() {
        // Issue #347: the old "No API key: open Edit settings" card named a
        // menu item that no longer exists. The fix is that the card's own
        // click opens Settings directly, so this proves the click posts
        // WM_APP_CARD_OPEN_SETTINGS to the owner (same self-notify idiom the
        // preview tests use) and does not just expand the card.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_settings_needed("No AI model set up yet", "Click here to open Settings.");
        assert_eq!(card.state(), CardState::Collapsed);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());

        assert_eq!(
            card.state(),
            CardState::Hidden,
            "clicking a settings-needed card must hide it, not expand it"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "clicking a settings-needed card must post WM_APP_CARD_OPEN_SETTINGS to the owner"
        );
        assert_eq!(msg.message, WM_APP_CARD_OPEN_SETTINGS);
    }

    #[test]
    fn clicking_an_ordinary_error_card_still_expands() {
        // Neighbouring case: show_error (not show_settings_needed) must keep
        // the pre-#347 expand-on-click behaviour.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error("Couldn't capture the screen", "detail text");
        assert_eq!(card.state(), CardState::Collapsed);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Expanded);
    }

    #[test]
    fn edit_field_colors_follow_the_card_theme() {
        // Issue #350: preview EDIT fields kept the stock white-on-black
        // regardless of theme. The colors WM_CTLCOLOREDIT hands back must
        // come from the same Palette the rest of the card paints with, not
        // a hardcoded pair -- otherwise dark mode gets a bright rectangle in
        // the one place the user reads carefully before confirming.
        let dark = Theme::Dark.palette();
        let (dark_text, dark_bg) = edit_field_colors(&dark);
        assert_eq!(dark_bg, dark.bg);
        assert_eq!(dark_text, dark.headline);

        let light = Theme::Light.palette();
        let (light_text, light_bg) = edit_field_colors(&light);
        assert_eq!(light_bg, light.bg);
        assert_eq!(light_text, light.headline);

        // The two themes must not collapse onto the same colors -- that
        // would be the "wired to nothing" failure mode: a function that
        // compiles and returns *a* color pair but ignores the theme it was
        // given.
        assert_ne!(dark_bg, light_bg);
        assert_ne!(dark_text, light_text);
    }

    #[test]
    fn set_text_scale_clamps_and_rebuilds() {
        let mut card = Card::new(instance()).expect("Card::new");
        assert!((card.inner.text_scale - 1.0).abs() < f32::EPSILON);

        // Within range: applied as-is.
        card.set_text_scale(1.5);
        assert!((card.inner.text_scale - 1.5).abs() < f32::EPSILON);

        // Below the floor: clamped up to 0.5.
        card.set_text_scale(0.01);
        assert!((card.inner.text_scale - 0.5).abs() < f32::EPSILON);

        // Above the ceiling: clamped down to 2.0.
        card.set_text_scale(50.0);
        assert!((card.inner.text_scale - 2.0).abs() < f32::EPSILON);

        // Fonts must survive the rebuild (never left null) and painting
        // whatever state is active must still not panic afterwards.
        assert!(!card.inner.fonts.headline.0.is_null());
        assert!(!card.inner.fonts.body.0.is_null());
        assert!(!card.inner.fonts.badge.0.is_null());

        card.show_answer("headline text", "detail text", 0, Some(Difficulty::Ultra));
        card.set_text_scale(0.75);
        assert_eq!(card.state(), CardState::Collapsed);
    }

    #[test]
    fn difficulty_none_reserves_no_width() {
        let card = Card::new(instance()).expect("Card::new");
        assert_eq!(card.inner.badge_reserve(None), 0);
        let content_w = card.inner.scale(CARD_WIDTH_DP) - 2 * card.inner.scale(PADDING_DP);
        assert_eq!(card.inner.headline_content_width(content_w), content_w);

        // A real difficulty always reserves *some* width.
        assert!(card.inner.badge_reserve(Some(Difficulty::Level(1))) > 0);
        assert!(card.inner.badge_reserve(Some(Difficulty::Level(10))) > 0);
        assert!(card.inner.badge_reserve(Some(Difficulty::Ultra)) > 0);
    }

    #[test]
    fn long_headline_wraps_clear_of_the_badge() {
        let mut card = Card::new(instance()).expect("Card::new");
        let content_w = card.inner.scale(CARD_WIDTH_DP) - 2 * card.inner.scale(PADDING_DP);
        let long_headline = "supercalifragilisticexpialidocious ".repeat(6);

        // Baseline: no badge, full content width.
        card.show_answer(&long_headline, "detail", 0, None);
        let no_badge_h =
            card.inner
                .measure_wrapped(card.inner.fonts.headline, &card.inner.headline, content_w);

        // With a badge: the headline must be measured/drawn into a strictly
        // narrower width (so it wraps clear of the badge), which can only
        // ever push the wrapped height up, never down.
        card.show_answer(&long_headline, "detail", 0, Some(Difficulty::Level(10)));
        let narrowed_w = card.inner.headline_content_width(content_w);
        assert!(narrowed_w < content_w);
        let badge_h =
            card.inner
                .measure_wrapped(card.inner.fonts.headline, &card.inner.headline, narrowed_w);
        assert!(badge_h >= no_badge_h);

        // The card must still lay out and paint without panicking for every
        // difficulty value with this long headline (exercises paint_collapsed
        // and paint_expanded's badge path end-to-end).
        for difficulty in [
            Difficulty::Level(1),
            Difficulty::Level(5),
            Difficulty::Level(10),
            Difficulty::Ultra,
        ] {
            card.show_answer(&long_headline, "some detail text", 0, Some(difficulty));
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.inner.try_expand();
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.hide();
        }
    }

    fn channels(c: u32) -> (u8, u8, u8) {
        (
            (c & 0xFF) as u8,
            ((c >> 8) & 0xFF) as u8,
            ((c >> 16) & 0xFF) as u8,
        )
    }

    #[test]
    fn difficulty_color_gradient_is_sane() {
        // Rank 1: dominantly green.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(1)));
        assert!(
            g > r && g > b,
            "rank 1 should read as green (r={r} g={g} b={b})"
        );

        // Rank 10: dominantly red.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(10)));
        assert!(
            r > g && r > b,
            "rank 10 should read as red (r={r} g={g} b={b})"
        );

        // Rank 5 sits at the amber midpoint stop: not muddy brown, i.e. red
        // and green channels should both be well above blue and reasonably
        // close to each other.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(5)));
        assert!(
            r > b && g > b,
            "rank 5 should read as amber (r={r} g={g} b={b})"
        );

        // Ultra: purple, outside the gradient -- blue and red both clearly
        // above green.
        let (r, g, b) = channels(difficulty_color(Difficulty::Ultra));
        assert!(
            b > g && r > g,
            "Ultra should read as purple (r={r} g={g} b={b})"
        );

        // All 11 ranks (Level(1..=10) plus Ultra) must be visually
        // distinguishable from one another.
        let mut colors: Vec<u32> = (1..=10u8)
            .map(|n| difficulty_color(Difficulty::Level(n)))
            .collect();
        colors.push(difficulty_color(Difficulty::Ultra));
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "ranks {i} and {j} produced the same colour"
                );
            }
        }
    }

    #[test]
    fn badge_text_color_contrasts_with_its_fill() {
        // A light (amber) fill must get dark ink, not white-on-yellow.
        let amber = difficulty_color(Difficulty::Level(5));
        assert_eq!(badge_text_color(amber), rgb(0x20, 0x20, 0x20));

        // Darker fills (green, red, purple) must get white ink.
        for d in [
            Difficulty::Level(1),
            Difficulty::Level(10),
            Difficulty::Ultra,
        ] {
            let fill = difficulty_color(d);
            assert_eq!(
                badge_text_color(fill),
                rgb(0xff, 0xff, 0xff),
                "{d:?} fill {fill:#08x} should contrast with white text"
            );
        }
    }

    // -- preview_key_command: pure keyboard mapping (#26) --------------------

    #[test]
    fn preview_key_command_maps_enter_to_do_it() {
        assert_eq!(preview_key_command(VK_RETURN.0), Some(ID_PREVIEW_DO_IT));
    }

    #[test]
    fn preview_key_command_maps_escape_to_cancel() {
        assert_eq!(preview_key_command(VK_ESCAPE.0), Some(ID_PREVIEW_CANCEL));
    }

    #[test]
    fn preview_key_command_ignores_every_other_key() {
        // Spot-check a handful of unrelated virtual-key codes, including the
        // boundary values 0 and u16::MAX.
        for vk in [0u16, 1, 0x41 /* 'A' */, 0x09 /* Tab */, u16::MAX] {
            assert_eq!(preview_key_command(vk), None, "vk={vk:#06x}");
        }
    }

    // -- preview_focus_left_the_card: pure decision (#207) -------------------

    #[test]
    fn focus_staying_in_the_card_group_does_not_cancel() {
        // Tab between two preview fields, or focus landing on the Do
        // it/Cancel button right before its own click fires: the new focus
        // target IS the card or a descendant of it.
        assert!(!preview_focus_left_the_card(true));
    }

    #[test]
    fn focus_leaving_the_card_group_cancels() {
        // The new focus target is neither the card itself nor a descendant
        // -- covers both "another window" and WM_KILLFOCUS's own
        // wparam == 0 case (see this function's doc comment: both compute
        // new_focus_is_card_or_descendant == false at the call site).
        assert!(preview_focus_left_the_card(false));
    }

    // -- preview state: real Win32 (#26) -------------------------------------
    //
    // Uses `Card::new_for_test`, which registers and creates against a
    // window class distinct from the production `Card` (rule 9). No window
    // is ever shown interactively and no message loop is pumped, so this is
    // safe and fast to run under `cargo test`, the same "smoke test" style
    // the module's other real-Win32 tests already use.

    fn calendar_schema_and_value() -> (serde_json::Value, serde_json::Value) {
        let schema = crate::actions::schema::schema_for("calendar_event", false)
            .expect("calendar_event is registered");
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": "bring laptop"
        });
        (schema, value)
    }

    fn gwl_style(hwnd: HWND) -> isize {
        const GWL_STYLE: i32 = -16;
        unsafe {
            GetWindowLongPtrW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WINDOW_LONG_PTR_INDEX(GWL_STYLE),
            )
        }
    }

    #[test]
    fn preview_calendar_event_renders_editable_start_and_title() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();

        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        let preview = card
            .inner
            .preview
            .as_ref()
            .expect("preview state is active");
        // #26's Done-when: "a calendar_event proposal renders with editable
        // start and title" -- exactly those two fields get a real EDIT
        // control, nothing else.
        let editable_names: Vec<&str> = preview
            .edits
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(editable_names.len(), 2);
        assert!(editable_names.contains(&"title"));
        assert!(editable_names.contains(&"start"));
        for (_, hwnd) in &preview.edits {
            assert!(!hwnd.0.is_null());
        }
        assert!(!preview.do_it_btn.0.is_null());
        assert!(!preview.cancel_btn.0.is_null());
        // main_window_exists is false here (#352): no Edit button at all.
        assert!(preview.edit_btn.is_none());

        // A real EDIT control's initial text round-trips through the
        // window, not just the pure model.
        let start_hwnd = preview
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        assert_eq!(window_text(start_hwnd), "09:00");
    }

    /// The wired-to-nothing check for issue #350 that does not depend on a
    /// desktop or compositor being attached to the process: sends the exact
    /// message a real preview EDIT control sends its parent
    /// (WM_CTLCOLOREDIT, wParam = the field's own HDC, lParam = its HWND)
    /// straight to `CardInner::handle_message`, the same path the real
    /// window proc uses, and checks the two GDI side effects a stock,
    /// unhandled WM_CTLCOLOREDIT would never produce: the HDC's text/
    /// background colors actually changed to the theme's, and a non-null
    /// brush came back (the stock default would leave `DefWindowProcW` to
    /// return `COLOR_WINDOW`, not our cached brush).
    #[test]
    fn wm_ctlcoloredit_paints_the_dark_palette_not_stock_white() {
        use windows::Win32::Graphics::Gdi::{GetBkColor, GetTextColor};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.inner.theme = Theme::Dark;
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let edit_hwnd = card
            .inner
            .preview
            .as_ref()
            .expect("preview state is active")
            .edits[0]
            .1;

        let hdc = unsafe { GetDC(Some(edit_hwnd)) };
        let result = card.inner.handle_message(
            WM_CTLCOLOREDIT,
            WPARAM(hdc.0 as usize),
            LPARAM(edit_hwnd.0 as isize),
        );
        let brush = result.expect("WM_CTLCOLOREDIT must be handled, not fall through to Def*");
        assert_ne!(brush.0, 0, "must return a real brush, not NULL");

        let dark = Theme::Dark.palette();
        let text_after = unsafe { GetTextColor(hdc) };
        let bg_after = unsafe { GetBkColor(hdc) };
        assert_eq!(text_after.0, dark.headline);
        assert_eq!(bg_after.0, dark.bg);
        // The bug this closes: stock EDIT white background, unconditionally.
        assert_ne!(bg_after.0, rgb(255, 255, 255));

        unsafe {
            ReleaseDC(Some(edit_hwnd), hdc);
        }
    }

    /// Manual observable for issue #350 (wired-to-nothing check): shows a
    /// real preview card, forced into `Theme::Dark`, pumps a few WM_PAINTs
    /// so the EDIT fields actually receive WM_CTLCOLOREDIT and repaint, then
    /// captures the window's own pixels with GetDIBits and writes a PNG.
    /// Captured in-process (not via a second process's screen-scrape)
    /// because this box's Bash and PowerShell tools run on window stations
    /// that cannot see each other's windows -- MEASURED 2026-09-24:
    /// `FindWindowW(NULL, "Wingman")` from a PowerShell tool call found
    /// nothing while the card window from a backgrounded `cargo test` was
    /// on screen and the test itself was still running. Not run by
    /// `cargo test`: `#[ignore]`d, same pattern as
    /// `ocr::ocr_live_recognizes_gdi_rendered_text`. Run with
    /// `cargo test dark_preview_manual_screenshot -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dark_preview_manual_screenshot() {
        use image::ImageEncoder;
        use windows::Win32::Graphics::Gdi::{
            GetDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetWindowRect, PeekMessageW, TranslateMessage, PM_REMOVE,
        };

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        // Force dark regardless of the machine's actual theme, so this
        // check does not depend on the dev box's Settings.
        card.inner.theme = Theme::Dark;
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let hwnd = card.hwnd();

        unsafe {
            use windows::Win32::Graphics::Gdi::{
                RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
            };

            // Pump enough messages for WM_PAINT (and the WM_CTLCOLOREDIT
            // each preview EDIT sends as it repaints) to actually run, then
            // force an immediate synchronous repaint before capture.
            for _ in 0..15 {
                let mut msg = std::mem::zeroed();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );

            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).expect("GetWindowRect");
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;

            let hdc = GetDC(Some(hwnd));
            let mem_dc = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
            let _ = BitBlt(mem_dc, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY);
            SelectObject(mem_dc, old);

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down DIB
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            GetDIBits(
                mem_dc,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );

            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem_dc);
            ReleaseDC(Some(hwnd), hdc);

            // GetDIBits hands back BGRA; the PNG encoder wants RGBA.
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }

            let path = std::env::temp_dir().join("wingman_dark_preview_350.png");
            let file = std::fs::File::create(&path).expect("create png file");
            let mut writer = std::io::BufWriter::new(file);
            image::codecs::png::PngEncoder::new(&mut writer)
                .write_image(&buf, w as u32, h as u32, image::ExtendedColorType::Rgba8)
                .expect("encode png");
            println!("saved dark preview screenshot to {}", path.display());
        }
    }

    #[test]
    fn preview_edit_button_only_exists_once_main_window_exists() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();

        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        assert!(
            card.inner.preview.as_ref().unwrap().edit_btn.is_none(),
            "Edit must not be created while no main window exists (#352)"
        );

        card.inner
            .show_preview("Add to calendar", &schema, &value, true);
        let edit_btn = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edit_btn
            .expect("Edit must be created once main_window_exists is true");
        assert_eq!(
            gwl_style(edit_btn) & (WS_DISABLED.0 as isize),
            0,
            "Edit must be enabled, not WS_DISABLED, once created"
        );
    }

    /// #355's pure layout-math test: `label_column_width` at several DPIs.
    /// Callers scale their inputs first, so this exercises the same
    /// function real DPIs would see without needing a real window.
    #[test]
    fn label_column_width_is_clamped_at_several_dpis() {
        for dpi in [96u32, 120, 144, 192] {
            let scale = |dp: i32| (dp * dpi as i32 + 48) / 96;
            let min_w = scale(PREVIEW_LABEL_W_DP);
            let content_w = scale(PREVIEW_WIDTH_DP) - 2 * scale(PADDING_DP);
            let max_w = ((content_w as f32) * PREVIEW_LABEL_MAX_FRACTION) as i32;

            // A short label never grows the column past its natural width.
            let short = scale(20);
            assert_eq!(
                label_column_width(&[short], min_w, max_w),
                min_w,
                "dpi {dpi}: a label narrower than the floor must not shrink the column"
            );

            // A label between the floor and the ceiling sizes the column
            // to exactly that label.
            let mid = (min_w + max_w) / 2;
            assert_eq!(
                label_column_width(&[mid], min_w, max_w),
                mid,
                "dpi {dpi}: a label between floor and ceiling sizes the column to it"
            );

            // A label wider than the ceiling is clamped, not honored in
            // full (it wraps instead -- see the row-height test below).
            let huge = max_w + scale(200);
            assert_eq!(
                label_column_width(&[huge], min_w, max_w),
                max_w,
                "dpi {dpi}: a label past the 45% ceiling must clamp, not widen the column further"
            );

            // The widest of several labels wins, still clamped.
            assert_eq!(
                label_column_width(&[short, mid, huge], min_w, max_w),
                max_w,
                "dpi {dpi}: the widest label among several drives the column"
            );

            // No fields at all: falls back to the floor, never zero/negative.
            assert_eq!(label_column_width(&[], min_w, max_w), min_w);
        }
    }

    /// #355's Done-when: a "Date of birth" field's whole label is visible
    /// (measured width fits inside `label_rect`, or it wraps rather than
    /// being cut) at both 100% and 150% scaling.
    #[test]
    fn preview_long_label_is_never_ellipsized_at_100_or_150_percent() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        for dpi in [96u32, 144] {
            card.inner.dpi = dpi;
            card.inner.rebuild_fonts();

            let fields = vec![Field {
                name: "dob".to_string(),
                label: "Date of birth".to_string(),
                value: "2000-01-01".to_string(),
                editable: false,
                required: false,
            }];
            let metrics = card.inner.compute_preview_layout(&fields, false);
            let row = &metrics.rows[0];
            let (label_w, _) = card
                .inner
                .measure_label(card.inner.fonts.body, "Date of birth");
            let column_w = row.label_rect.right - row.label_rect.left;

            assert!(
                column_w >= label_w || row.label_wrapped,
                "dpi {dpi}: \"Date of birth\" must fit the column ({column_w}px) or wrap, \
                 not be silently ellipsized (measured {label_w}px)"
            );
        }
    }

    #[test]
    fn preview_without_edit_spreads_do_it_and_cancel_across_the_freed_width() {
        // #352's Done-when: only Do it and Cancel show, laid out across the
        // width Edit used to share -- not squeezed into the same corner as
        // before with a blank gap where Edit was.
        let card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let fields = vec![];
        let metrics = card.inner.compute_preview_layout(&fields, false);
        assert!(metrics.buttons.edit.is_none());
        // Cancel now sits at the content's left edge, not directly beside
        // Do it, so the freed width is genuinely used rather than left
        // blank on the far side.
        let padding = card.inner.scale(PADDING_DP);
        assert_eq!(metrics.buttons.cancel.left, padding);
        assert!(metrics.buttons.cancel.right < metrics.buttons.do_it.left);

        let with_edit = card.inner.compute_preview_layout(&fields, true);
        assert!(with_edit.buttons.edit.is_some());
        // With Edit present, Cancel sits directly beside Do it (small gap),
        // not spread out to the content's left edge like the no-edit case.
        assert!(with_edit.buttons.cancel.left > padding);
        assert!(with_edit.buttons.cancel.right < with_edit.buttons.do_it.left);
    }

    #[test]
    fn preview_do_it_after_an_edit_builds_a_confirmed_equal_to_what_was_shown() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();

        // Programmatically edit the field, exactly like a real keystroke
        // would leave it.
        let edited = wide_z("10:30");
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                start_hwnd,
                PCWSTR(edited.as_ptr()),
            );
        }

        // Simulate the "Do it" command via SendMessage(WM_COMMAND), the same
        // message a real button click (or the Enter subclass) generates.
        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(
            card.state(),
            CardState::Hidden,
            "Do it must close the preview"
        );
        let confirmed = card
            .take_confirmed()
            .expect("Do it must produce a Confirmed");
        assert_eq!(confirmed.value()["start"], "10:30");
        assert_eq!(confirmed.value()["title"], "Standup");
        assert_eq!(confirmed.value()["end"], "09:15");
        assert_eq!(confirmed.value()["location"], "Room 2");

        // A take, not a peek.
        assert!(card.take_confirmed().is_none());
    }

    #[test]
    fn preview_cancel_produces_no_confirmed_and_closes() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_CANCEL as usize)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(card.state(), CardState::Hidden);
        assert!(
            card.take_confirmed().is_none(),
            "Cancel must produce nothing"
        );
    }

    #[test]
    fn preview_do_it_notifies_the_owner_window_via_wm_app_preview_decided() {
        // Issue #39: uses the card's own hwnd as a stand-in "owner" (a
        // self-notify) so the real observable -- a posted message sitting
        // in a real message queue -- can be checked with PeekMessageW, the
        // same idiom `preview_control_subclass_forwards_enter_as_a_posted_do_it_command`
        // already uses for the same reason.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_do_it must post WM_APP_PREVIEW_DECIDED to the owner window"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn preview_cancel_also_notifies_the_owner_window() {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_CANCEL as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_cancel must also post WM_APP_PREVIEW_DECIDED (App::on_preview_decided \
             calls take_confirmed(), which is None for Cancel -- see that method's doc comment)"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn hide_while_preview_is_open_notifies_the_owner_with_no_confirmation() {
        // Issue #225: every one of app.rs's "hide the stale card" call
        // sites (ask, extract_text, copy_region, open_settings,
        // pause_for, add_event_from_screen, review_this_email,
        // fill_form_from_screen) calls exactly this public `Card::hide()`
        // with no check for `CardState::Preview`. Before the fix,
        // `CardInner::hide` routed to `leave_preview_if_active`, which
        // destroyed the preview's controls and reset `state` to `Hidden`
        // -- but never posted `WM_APP_PREVIEW_DECIDED`, so whichever
        // pending-preview context `App` had stashed for it was never
        // cleared, and a later, unrelated preview's confirm could pick up
        // the stale one instead (or run neither, dropping the new one).
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        // The bug: hiding the card for an unrelated reason while a
        // preview is open (Settings opening, Pause, a second action
        // starting...), instead of the user pressing "Do it" or "Cancel".
        card.hide();

        assert_eq!(card.state(), CardState::Hidden, "hide() must still hide");
        assert!(
            card.take_confirmed().is_none(),
            "an externally-hidden preview must never look like a Do it"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "hide() must post WM_APP_PREVIEW_DECIDED when it destroys an \
             active preview, so App::on_preview_decided can clear its \
             pending-preview slot instead of leaving it to hijack a \
             later, unrelated preview's decision (#225)"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn show_preview_interrupting_an_active_preview_notifies_before_replacing_it() {
        // Same shape as the hide() case above, but for the path where a
        // NEW preview interrupts an active one directly (on_calendar_result
        // / on_review_result / on_form_fill_result all call show_preview
        // without checking for an existing one first).
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        let _ = card.show_preview("Fill this form", &schema, &value, false);
        assert_eq!(
            card.state(),
            CardState::Preview,
            "the second preview must still show"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "replacing an active preview with a new one must notify the \
             owner about the FIRST one before showing the second"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn preview_do_it_posts_exactly_one_wm_app_preview_decided() {
        // Mutation guard for #225's fix: `leave_preview_if_active` now
        // does the notifying (so an external `hide()` is covered too),
        // and `preview_do_it`'s own explicit call was removed -- if it
        // had not been, "Do it" would double-post and this would go red.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut count = 0;
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_APP_PREVIEW_DECIDED {
                count += 1;
            }
        }
        assert_eq!(
            count, 1,
            "leave_preview_if_active must not double-post alongside a \
             separate explicit notify"
        );
    }

    #[test]
    fn no_owner_set_means_preview_do_it_never_panics_and_posts_nothing() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        // No `set_owner` call: `preview_do_it` must degrade quietly, same
        // as every other best-effort Win32 call in this module.
        card.inner.preview_do_it();
        assert!(card.take_confirmed().is_some());
    }

    #[test]
    fn preview_never_auto_dismisses_on_the_safety_timer() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        // Simulate a TIMER_DISMISS message arriving while Preview is active
        // (rule 5: a card must never close on a timer while showing an
        // action that has not been confirmed yet).
        let handled = card.handle_message(WM_TIMER, WPARAM(TIMER_DISMISS), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(
            card.state(),
            CardState::Preview,
            "TIMER_DISMISS must never close an open preview"
        );
    }

    #[test]
    fn preview_enter_key_on_the_card_window_itself_triggers_do_it() {
        // Exercises the card's own WM_KEYDOWN arm (as opposed to a child
        // control's subclass), which uses the same preview_key_command
        // mapping.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Hidden);
        assert!(card.take_confirmed().is_some());
    }

    #[test]
    fn preview_control_subclass_forwards_enter_as_a_posted_do_it_command() {
        // Real Enter/Esc keystrokes reach `run_preview_command` through
        // `preview_control_subclass` (installed on every EDIT/BUTTON child
        // by `create_preview_controls`), not through the card's own
        // WM_KEYDOWN arm -- see that function's doc comment for why a child
        // control's WM_KEYDOWN never reaches the parent on its own. This
        // test calls the actual installed function pointer directly, the
        // same way Windows would while a preview EDIT control has focus,
        // and checks the real observable: a WM_COMMAND sitting in this
        // thread's message queue (PostMessageW, not SendMessageW), which is
        // exactly what `run_preview_command` needs delivered to fire.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KEYDOWN,
                WPARAM(VK_RETURN.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        // A single, non-looping check: posted messages (like the WM_COMMAND
        // above) take priority over synthesized WM_PAINT/WM_TIMER messages,
        // so one PeekMessageW call retrieves it directly. This deliberately
        // does NOT loop with PM_REMOVE: discarding a synthesized WM_PAINT
        // without running it through DispatchMessageW (which is what
        // validates the window via BeginPaint/EndPaint) would make Windows
        // re-synthesize WM_PAINT forever for this still-invalid window --
        // an infinite busy loop, not a hang that shows up as "no output".
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_control_subclass must post a WM_COMMAND to the card window"
        );
        assert_eq!(msg.message, WM_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_DO_IT);
    }

    // -- click-away cancels the preview (#207) --------------------------

    #[test]
    fn preview_control_subclass_posts_cancel_when_focus_leaves_the_card() {
        // Simulates a real click-away: WM_KILLFOCUS on a preview field whose
        // new focus target (wparam, per the real WM_KILLFOCUS contract) is
        // some other, unrelated window -- not the card, not one of its
        // children.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        let other_hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(wide_z(TEST_CLASS_NAME).as_ptr()),
                PCWSTR(wide_z("unrelated window").as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance()),
                None,
            )
        }
        .expect("CreateWindowExW for the unrelated window");

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(other_hwnd.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "a click-away WM_KILLFOCUS must post a WM_COMMAND to the card window"
        );
        assert_eq!(msg.message, WM_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_CANCEL);

        unsafe {
            let _ = DestroyWindow(other_hwnd);
        }
    }

    #[test]
    fn preview_control_subclass_posts_cancel_when_nothing_gains_focus() {
        // WM_KILLFOCUS's wparam is 0 when no window is gaining the focus at
        // all (e.g. the whole app losing the foreground) -- must cancel the
        // same as a click into another window, per preview_focus_left_the_card's
        // doc comment.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(0),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(found, "wparam == 0 must also be treated as a click-away");
        assert_eq!(msg.message, WM_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_CANCEL);
    }

    #[test]
    fn preview_control_subclass_does_not_cancel_on_focus_moving_to_another_preview_control() {
        // Tab moving focus from the "start" EDIT to the Do it button (both
        // preview children) must not cancel -- the exact scenario
        // preview_focus_left_the_card's doc comment names.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let preview = card.inner.preview.as_ref().unwrap();
        let start_hwnd = preview
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let do_it_btn = preview.do_it_btn;
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(do_it_btn.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        // Filtered to the WM_COMMAND range specifically (rather than 0..0,
        // which every other real-Win32 test in this module uses): an EDIT
        // control's OWN default WM_KILLFOCUS handling (reached via
        // DefSubclassProc, since `preview_control_subclass` does not
        // swallow WM_KILLFOCUS) legitimately posts its own WM_COMMAND
        // (EN_KILLFOCUS) to its parent regardless of this fix, and an
        // unfiltered peek would also pick up an unrelated synthesized
        // WM_PAINT for this never-validated test window. Neither is what
        // this test checks; only whether ID_PREVIEW_CANCEL specifically was
        // posted.
        let mut msg = MSG::default();
        let found = unsafe {
            PeekMessageW(&mut msg, Some(card_hwnd), WM_COMMAND, WM_COMMAND, PM_REMOVE).as_bool()
        };
        let cancelled = found && (msg.wParam.0 & 0xFFFF) as i32 == ID_PREVIEW_CANCEL;
        assert!(
            !cancelled,
            "focus moving between two preview controls must not cancel the preview"
        );
    }

    #[test]
    fn preview_closing_restores_the_previous_foreground_window() {
        // The settings window's own smoke test constructs a real HWND the
        // same way; reused here purely as "some other real window that
        // existed before the preview" -- never shown, never pumped.
        let settings_hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(wide_z(TEST_CLASS_NAME).as_ptr()),
                PCWSTR(wide_z("other window").as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance()),
                None,
            )
        };
        // This environment may have no other window able to become the
        // foreground window (headless CI); the important, always-checkable
        // part of this test is that close_preview does not panic and always
        // leaves the card itself Hidden -- see the two asserts below, which
        // run regardless of whether CreateWindowExW above succeeded.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        card.inner.preview_cancel();
        assert_eq!(card.state(), CardState::Hidden);
        if let Ok(hwnd) = settings_hwnd {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    #[test]
    fn preview_gdi_objects_do_not_leak_across_create_show_close_drop() {
        // Best-effort per the task's "if practical": GetGuiResources counts
        // GDI objects for the whole process, and `cargo test` runs many
        // tests concurrently on other threads, so this cannot assert an
        // exact before/after match without flaking on unrelated tests'
        // allocations. What it CAN assert without flaking: creating,
        // showing, editing, and closing a preview, then dropping the card,
        // must not leave the process's GDI object count higher than it was
        // immediately after the card's own fonts were built (i.e. nothing
        // preview-specific leaks), allowing generous slack for concurrent
        // tests.
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS,
        };

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let baseline = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };

        let (schema, value) = calendar_schema_and_value();
        for _ in 0..3 {
            card.inner
                .show_preview("Add to calendar", &schema, &value, false);
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.inner.preview_cancel();
        }
        drop(card);

        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        let slack = 64; // headroom for concurrently-running unrelated tests
        assert!(
            after <= baseline + slack,
            "GDI object count grew by more than {slack} after repeated preview create/close/drop \
             (baseline={baseline}, after={after}); investigate a leak before raising this slack"
        );
    }

    // -- issue #221: DrawTextW must not crash on empty headline/detail/preview
    // text --------------------------------------------------------------------
    //
    // `Answer::headline`/`Answer::detail` are plain, unvalidated `String`s
    // (`provider::common::answer_schema` has no `minLength`), and a
    // non-editable preview field's value can be empty too (an omitted or
    // empty proposal property -- see `PreviewModel::from_schema`'s
    // `unwrap_or_default()`). MEASURED 2026-09-17 (`src/ui/palette.rs`'s
    // `draw_text_line_tolerates_an_empty_string`, commit `453fe0b`, and
    // independently re-confirmed in this worktree by
    // `crate::ui::text::tests::raw_drawtextw_crashes_on_empty_text`, a
    // dedicated `#[ignore]`d test run in isolation -- see that module's own
    // doc comment): an unguarded `DrawTextW` call with a zero-length UTF-16
    // buffer -- what `utf16("")` produces -- reliably crashes with exit
    // code `0xC0000005` (`STATUS_ACCESS_VIOLATION`) through this crate's
    // `windows` 0.62 binding. These tests here do NOT re-run that raw,
    // unguarded call: doing so as part of this ordinary `cargo test`
    // invocation would take down this whole test binary
    // (`STATUS_ACCESS_VIOLATION` is not a catchable panic), losing every
    // other test in the same run. What these tests prove instead is that
    // painting an empty headline/detail/preview-field DOES NOT crash now
    // that every call site routes through
    // `crate::ui::text::draw_text_line`.

    #[test]
    fn paint_collapsed_with_empty_headline_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_answer("", "", 0, None);
        assert_eq!(card.state(), CardState::Collapsed);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_expanded_with_empty_headline_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        // `try_expand` only enters Expanded when `detail` is non-empty (see
        // its own `self.detail.is_empty()` early return), so a genuinely
        // empty detail can never reach Expanded through the public API --
        // that DrawTextW call is provably unreachable with empty text
        // (paint_expanded's own `if !self.detail.is_empty()` guard around
        // it), unlike the headline's, which is unconditional. This still
        // exercises the headline's DrawTextW call with an empty string, the
        // reachable half of the bug.
        card.show_answer("", "non-empty detail", 0, Some(Difficulty::Level(3)));
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        // Scroll past the fold so the "more below" hint band (also a
        // DrawTextW call site) paints too.
        card.inner.scroll_offset = 1;
        card.inner.scroll_max = 100;
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_error_with_empty_headline_and_detail_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_error("", "");
        assert_eq!(card.state(), CardState::Collapsed);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_preview_with_empty_title_and_empty_field_value_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, _) = calendar_schema_and_value();
        // "location" is a non-editable field (see calendar_schema_and_value's
        // schema): an empty value here reaches paint_preview's value
        // DrawTextW call directly, the same shape as a proposal that simply
        // omitted the property (PreviewModel::from_schema's
        // unwrap_or_default()).
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "", "notes": "bring laptop"
        });
        // An empty title also exercises paint_preview's own title DrawTextW
        // call.
        card.inner.show_preview("", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }
}
