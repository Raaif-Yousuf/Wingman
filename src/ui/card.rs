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
    SelectObject, SetBkMode, SetTextColor, BS_SOLID, DEFAULT_GUI_FONT, DT_CALCRECT, DT_CENTER,
    DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_RIGHT, DT_SINGLELINE, DT_TOP,
    DT_VCENTER, DT_WORDBREAK, FW_NORMAL, HBRUSH, HDC, HFONT, HGDIOBJ, LOGBRUSH, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, NULL_BRUSH, NULL_PEN, PS_ENDCAP_ROUND, PS_GEOMETRIC, PS_JOIN_ROUND,
    PS_SOLID, SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
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

use crate::provider::Difficulty;

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
            difficulty: None,
            dpi: 96,
            theme,
            fonts: Fonts::null(),
            text_scale: 1.0,
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
        self.reset_activation_and_timers();
        self.headline = headline.to_string();
        self.detail = detail.to_string();
        self.difficulty = difficulty;
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
            let mut rect = RECT { left: 0, top: 0, right: 0, bottom: 0 };
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            ((rect.right - rect.left).max(1), (rect.bottom - rect.top).max(1))
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

        let mut headline_buf = utf16(&self.headline);
        let mut headline_rect = RECT {
            left: rc.left + padding,
            top: rc.top + padding,
            right: rc.left + padding + headline_w,
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

        self.paint_difficulty_badge(hdc, rc);
    }

    unsafe fn paint_expanded(&self, hdc: HDC, rc: RECT, padding: i32, gap: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let content_left = rc.left + padding;
        let content_top = rc.top + padding;
        let content_bottom = rc.bottom - padding;

        // Clip to the padded content area so scrolled text never bleeds into
        // the border/padding.
        IntersectClipRect(hdc, content_left, content_top, rc.right - padding, content_bottom);

        let headline_w = self.headline_content_width(content_w);
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_w)
            .max(self.line_height(self.fonts.headline));

        let y0 = content_top - self.scroll_offset;
        let mut headline_buf = utf16(&self.headline);
        let mut headline_rect = RECT {
            left: content_left,
            top: y0,
            right: content_left + headline_w,
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
            // Reuse the headline's badge reservation so the hint (which is
            // also right-aligned, in the same bottom-right corner the badge
            // occupies) does not draw underneath it either.
            let mut hint_rect = RECT {
                left: content_left,
                top: rc.bottom - padding - band_h + self.scale(2),
                right: content_left + headline_w,
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

        let mut label = utf16(difficulty.label());
        let mut label_rect = RECT { left, top, right, bottom };
        SelectObject(hdc, HGDIOBJ(self.fonts.badge.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(text_color));
        DrawTextW(
            hdc,
            &mut label,
            &mut label_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
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
        let badge_h = card.inner.measure_wrapped(
            card.inner.fonts.headline,
            &card.inner.headline,
            narrowed_w,
        );
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
        assert!(g > r && g > b, "rank 1 should read as green (r={r} g={g} b={b})");

        // Rank 10: dominantly red.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(10)));
        assert!(r > g && r > b, "rank 10 should read as red (r={r} g={g} b={b})");

        // Rank 5 sits at the amber midpoint stop: not muddy brown, i.e. red
        // and green channels should both be well above blue and reasonably
        // close to each other.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(5)));
        assert!(r > b && g > b, "rank 5 should read as amber (r={r} g={g} b={b})");

        // Ultra: purple, outside the gradient -- blue and red both clearly
        // above green.
        let (r, g, b) = channels(difficulty_color(Difficulty::Ultra));
        assert!(b > g && r > g, "Ultra should read as purple (r={r} g={g} b={b})");

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
        for d in [Difficulty::Level(1), Difficulty::Level(10), Difficulty::Ultra] {
            let fill = difficulty_color(d);
            assert_eq!(
                badge_text_color(fill),
                rgb(0xff, 0xff, 0xff),
                "{d:?} fill {fill:#08x} should contrast with white text"
            );
        }
    }
}
