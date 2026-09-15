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
//!     pub fn show_answer(&mut self, headline: &str, detail: &str, auto_dismiss_secs: u32);
//!     pub fn show_error(&mut self, headline: &str, detail: &str);
//!     pub fn hide(&mut self);
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
//! - `show_pending` likewise has no timeout parameter, but pending state still
//!   carries an internal safety-net auto-dismiss (`PENDING_SAFETY_TIMEOUT_SECS`)
//!   so a card is never stuck forever if the worker thread never reports back.
//! - Nothing in this module panics. Win32 calls that can fail are always
//!   handled by falling back to a degraded-but-functional default (square
//!   corners, stock font, light theme, primary-monitor work area, etc.).

use std::ffi::c_void;
use std::sync::{Once, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, EndPaint, FillRect, FrameRect,
    GetDC, GetMonitorInfoW, GetStockObject, GetTextMetricsW, IntersectClipRect, MonitorFromPoint,
    MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode, SetTextColor, DEFAULT_GUI_FONT,
    DT_CALCRECT, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP,
    DT_VCENTER,
    DT_WORDBREAK, FW_SEMIBOLD, HBRUSH, HDC, HFONT, HGDIOBJ, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetWindowLongPtrW, KillTimer, LoadCursorW, RegisterClassExW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow, SystemParametersInfoW,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, GWL_EXSTYLE, HWND_TOPMOST, IDC_ARROW,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA,
    SW_HIDE, SW_SHOWNOACTIVATE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SWP_NOZORDER, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_DESTROY, WM_DPICHANGED,
    WM_ERASEBKGND, WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEWHEEL, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_TIMER, WNDCLASSEXW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    WS_EX_TOPMOST, WS_POPUP,
};

// ---------------------------------------------------------------------------
// Public state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Hidden,
    Pending,
    Collapsed,
    Expanded,
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
            anyhow::bail!("copilot-ask: failed to register the card window class");
        }

        let theme = detect_theme();
        // A 96 DPI guess used only to build a first, throwaway set of fonts
        // before we have a real HWND to ask GetDpiForWindow about. It is
        // replaced immediately below once the window exists.
        let inner = Box::new(CardInner {
            hwnd: HWND(std::ptr::null_mut()),
            state: CardState::Hidden,
            headline: String::new(),
            detail: String::new(),
            dpi: 96,
            theme,
            fonts: Fonts::null(),
            anim_frame: 0,
            scroll_offset: 0,
            scroll_max: 0,
            ex_noactivate_removed: false,
        });
        let raw = Box::into_raw(inner);

        let class_name = wide_z(CLASS_NAME);
        let title = wide_z("copilot-ask");
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
                return Err(anyhow::anyhow!("copilot-ask: CreateWindowExW failed: {e}"));
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

    pub fn show_pending(&mut self) {
        self.inner.show_pending();
    }

    pub fn show_answer(&mut self, headline: &str, detail: &str, auto_dismiss_secs: u32) {
        self.inner.show_collapsed(headline, detail, auto_dismiss_secs);
    }

    pub fn show_error(&mut self, headline: &str, detail: &str) {
        // No timeout parameter is given for errors: they persist until the
        // user dismisses them (click to expand, then Esc / focus-loss), or
        // until a later show_* call replaces them. See module docs.
        self.inner.show_collapsed(headline, detail, 0);
    }

    pub fn hide(&mut self) {
        self.inner.hide();
    }

    pub fn state(&self) -> CardState {
        self.inner.state
    }

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

const CLASS_NAME: &str = "CopilotAsk.Card.Window.7f3c1a9e";

static CLASS_INIT: Once = Once::new();
static CLASS_OK: OnceLock<bool> = OnceLock::new();

fn ensure_class_registered(instance: HINSTANCE) -> bool {
    CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance) };
        let _ = CLASS_OK.set(ok);
    });
    CLASS_OK.get().copied().unwrap_or(false)
}

unsafe fn register_class(instance: HINSTANCE) -> bool {
    let class_name = wide_z(CLASS_NAME);
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
unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
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
}

impl Fonts {
    fn null() -> Self {
        Fonts {
            headline: HFONT(std::ptr::null_mut()),
            body: HFONT(std::ptr::null_mut()),
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
        }
    }
}

/// Builds the headline/body fonts for `dpi`, deriving from the shell's
/// message font (`SPI_GETNONCLIENTMETRICS`) so the card matches the system.
/// Infallible: any Win32 failure degrades to a stock GUI font rather than
/// panicking or propagating an error.
fn build_fonts(dpi: u32) -> Fonts {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
        let got = SystemParametersInfoW(
            SPI_GETNONCLIENTMETRICS,
            ncm.cbSize,
            Some(&mut ncm as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
        .is_ok();

        let base = if got { ncm.lfMessageFont } else { fallback_logfont() };
        // `lfMessageFont` comes back at the system's baseline (96 DPI)
        // scale; every actual pixel size used on screen must be scaled from
        // that baseline by the *current monitor's* DPI, not hardcoded.
        let scale = dpi as f32 / 96.0;

        let mut headline_lf = base;
        headline_lf.lfWeight = FW_SEMIBOLD.0 as i32;
        headline_lf.lfHeight = scaled_height(base.lfHeight, scale * 1.16);

        let mut body_lf = base;
        body_lf.lfHeight = scaled_height(base.lfHeight, scale);

        Fonts {
            headline: font_or_stock(&headline_lf),
            body: font_or_stock(&body_lf),
        }
    }
}

fn scaled_height(base_height: i32, factor: f32) -> i32 {
    let v = (base_height as f32 * factor).round() as i32;
    if v == 0 {
        if base_height < 0 { -1 } else { 1 }
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

const MARGIN_DP: i32 = 20;
const PADDING_DP: i32 = 18;
const CARD_WIDTH_DP: i32 = 400;
/// Collapsed headline cap. The prompt budgets 90 characters, and at a high
/// DPI scale that needs four lines at this width -- three silently ellipsised
/// real answers mid-sentence.
const HEADLINE_MAX_LINES: i32 = 4;
const PENDING_WIDTH_DP: i32 = 240;
const GAP_DP: i32 = 8;
const WHEEL_SCROLL_DP: i32 = 48;

const TIMER_ANIM: usize = 1;
const TIMER_DISMISS: usize = 2;
const ANIM_INTERVAL_MS: u32 = 450;
const PENDING_SAFETY_TIMEOUT_SECS: u32 = 30;

// ---------------------------------------------------------------------------
// CardInner: the real state; addressed by raw pointer from GWLP_USERDATA
// ---------------------------------------------------------------------------

struct CardInner {
    hwnd: HWND,
    state: CardState,
    headline: String,
    detail: String,
    dpi: u32,
    theme: Theme,
    fonts: Fonts,
    anim_frame: u32,
    scroll_offset: i32,
    scroll_max: i32,
    /// True while WS_EX_NOACTIVATE has been removed (i.e. while Expanded).
    ex_noactivate_removed: bool,
}

impl CardInner {
    fn scale(&self, dp: i32) -> i32 {
        (dp * self.dpi as i32 + 48) / 96
    }

    fn rebuild_fonts(&mut self) {
        let fresh = build_fonts(self.dpi);
        let old = std::mem::replace(&mut self.fonts, fresh);
        old.delete();
    }

    // -- show/hide -----------------------------------------------------

    fn show_pending(&mut self) {
        self.reset_activation_and_timers();
        self.headline = "Thinking".to_string();
        self.detail.clear();
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

    fn show_collapsed(&mut self, headline: &str, detail: &str, auto_dismiss_secs: u32) {
        self.reset_activation_and_timers();
        self.headline = headline.to_string();
        self.detail = detail.to_string();
        self.state = CardState::Collapsed;
        self.scroll_offset = 0;
        self.scroll_max = 0;

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
        let padding = self.scale(PADDING_DP);
        let width = self.scale(PENDING_WIDTH_DP);
        let line_h = self.line_height(self.fonts.headline).max(1);
        let height = padding * 2 + line_h;
        let work = self.work_area_for_cursor();
        self.place_bottom_right(work, width, height);
    }

    fn layout_collapsed(&mut self) {
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        let measured = self.measure_wrapped(self.fonts.headline, &self.headline, content_width);
        let headline_h = measured.min(max_headline_h).max(headline_line_h);

        let mut height = padding * 2 + headline_h;
        if !self.detail.is_empty() {
            let hint_h = self.line_height(self.fonts.body).max(1);
            height += gap + hint_h;
        }

        let work = self.work_area_for_cursor();
        self.place_bottom_right(work, width, height);
    }

    fn layout_expanded(&mut self) {
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, content_width)
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
                CardState::Collapsed => self.paint_collapsed(hdc, rc, padding, gap, &palette),
                CardState::Expanded => self.paint_expanded(hdc, rc, padding, gap, &palette),
            }
        }
    }

    unsafe fn paint_pending(&self, hdc: HDC, rc: RECT, padding: i32, palette: &Palette) {
        let dots = ".".repeat(1 + (self.anim_frame as usize % 3));
        let text = format!("Thinking{dots}");
        let mut buf = utf16(&text);
        let mut rect = RECT {
            left: rc.left + padding,
            top: rc.top,
            right: rc.right - padding,
            bottom: rc.bottom,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        DrawTextW(
            hdc,
            &mut buf,
            &mut rect,
            DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    unsafe fn paint_collapsed(&self, hdc: HDC, rc: RECT, padding: i32, gap: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        // Must match layout_collapsed's arithmetic exactly. Using the cap here
        // instead of the measured height pushes the hint below the window's
        // bottom edge whenever the headline wraps to fewer than the maximum
        // number of lines -- i.e. most of the time.
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, content_w)
            .min(max_headline_h)
            .max(headline_line_h);

        let mut headline_buf = utf16(&self.headline);
        let mut headline_rect = RECT {
            left: rc.left + padding,
            top: rc.top + padding,
            right: rc.left + padding + content_w,
            bottom: rc.top + padding + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        DrawTextW(
            hdc,
            &mut headline_buf,
            &mut headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        if !self.detail.is_empty() {
            let hint_top = rc.top + padding + headline_h + gap;
            let mut hint_buf = utf16("click for working");
            let mut hint_rect = RECT {
                left: rc.left + padding,
                top: hint_top,
                right: rc.left + padding + content_w,
                bottom: hint_top + self.line_height(self.fonts.body).max(1),
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            DrawTextW(
                hdc,
                &mut hint_buf,
                &mut hint_rect,
                DT_LEFT | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }
    }

    unsafe fn paint_expanded(&self, hdc: HDC, rc: RECT, padding: i32, gap: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let content_left = rc.left + padding;
        let content_top = rc.top + padding;
        let content_bottom = rc.bottom - padding;

        // Clip to the padded content area so scrolled text never bleeds into
        // the border/padding.
        IntersectClipRect(hdc, content_left, content_top, rc.right - padding, content_bottom);

        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, content_w)
            .max(self.line_height(self.fonts.headline));

        let y0 = content_top - self.scroll_offset;
        let mut headline_buf = utf16(&self.headline);
        let mut headline_rect = RECT {
            left: content_left,
            top: y0,
            right: content_left + content_w,
            bottom: y0 + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        DrawTextW(
            hdc,
            &mut headline_buf,
            &mut headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );

        if !self.detail.is_empty() {
            let detail_h = self.measure_wrapped(self.fonts.body, &self.detail, content_w);
            let y1 = y0 + headline_h + gap;
            let mut detail_buf = utf16(&self.detail);
            let mut detail_rect = RECT {
                left: content_left,
                top: y1,
                right: content_left + content_w,
                bottom: y1 + detail_h,
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
            DrawTextW(
                hdc,
                &mut detail_buf,
                &mut detail_rect,
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
            let mut hint = utf16("more below");
            let mut hint_rect = RECT {
                left: content_left,
                top: rc.bottom - padding - band_h + self.scale(2),
                right: rc.right - padding,
                bottom: rc.bottom - padding,
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            DrawTextW(
                hdc,
                &mut hint,
                &mut hint_rect,
                DT_RIGHT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
            );
        }
    }

    // -- message handling --------------------------------------------------

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, _lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ERASEBKGND => Some(LRESULT(1)),
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
                    TIMER_DISMISS => {
                        self.hide();
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                self.try_expand();
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
                self.hwnd = HWND(std::ptr::null_mut());
                None
            }
            WM_DESTROY => None,
            _ => None,
        }
    }
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

        card.show_answer("2 + 2 = 4", "You carried correctly.", 5);
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

        card.hide();
        assert_eq!(card.state(), CardState::Hidden);
        // Dropping must not panic (this exercises DestroyWindow + WM_NCDESTROY).
    }
}
