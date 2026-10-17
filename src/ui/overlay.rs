//! The in-place overlay's window (spec
//! `docs/superpowers/specs/2026-10-16-overlay-design.md`, "The overlay
//! window"): one layered, click-through, topmost, never-focused window that
//! holds the labels and underline marks the pure `crate::overlay` core placed,
//! and the Win32 steps (event hooks, mouse and key hooks, the age timer) that
//! make it go away when the screen under it changes.
//!
//! Everything decided is in `crate::overlay` and unit-tested there. This file
//! only does what needs Win32: measuring and drawing into a DIB, pushing it
//! with `UpdateLayeredWindow`, excluding the window from capture, installing
//! and removing the hooks, and looking up the windows the main thread's
//! decision needs. Hook callbacks here only post `WM_APP_OVERLAY_INPUT`; they
//! never look anything up (`GetForegroundWindow`, `WindowFromPoint`), because
//! Windows unhooks a callback that is slow.
//!
//! Fail safe: a window that cannot be excluded from capture, a hook or timer
//! that cannot be installed, or a bitmap that cannot be drawn means no overlay
//! at all; the caller shows the summary card alone.

use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Once;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, SIZE, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, CreateFontIndirectW, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, GdiFlush, GetDC, GetTextExtentPoint32W, MonitorFromRect, ReleaseDC,
    SelectObject, SetBkMode, SetTextColor, AC_SRC_ALPHA, AC_SRC_OVER, BITMAPINFO, BITMAPINFOHEADER,
    BI_RGB, BLENDFUNCTION, CLEARTYPE_QUALITY, DIB_RGB_COLORS, DT_CENTER, DT_NOPREFIX,
    DT_SINGLELINE, DT_VCENTER, FW_NORMAL, HBITMAP, HDC, HFONT, HGDIOBJ, LOGFONTW,
    MONITOR_DEFAULTTONEAREST, TRANSPARENT,
};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetAncestor, GetForegroundWindow,
    GetWindowThreadProcessId, KillTimer, PostMessageW, RegisterClassExW, SetTimer,
    SetWindowDisplayAffinity, SetWindowPos, ShowWindow, UpdateLayeredWindow, WindowFromPoint,
    CS_HREDRAW, CS_VREDRAW, EVENT_OBJECT_DESTROY, EVENT_OBJECT_LOCATIONCHANGE,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZESTART, GA_ROOT, HTTRANSPARENT, HWND_TOPMOST,
    MA_NOACTIVATE, OBJID_WINDOW, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_SHOWNOACTIVATE,
    ULW_ALPHA, WDA_EXCLUDEFROMCAPTURE, WDA_NONE, WINDOW_DISPLAY_AFFINITY, WINEVENT_OUTOFCONTEXT,
    WINEVENT_SKIPOWNPROCESS, WM_APP, WM_ERASEBKGND, WM_MOUSEACTIVATE, WM_NCHITTEST, WNDCLASSEXW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

use crate::capture::ScreenRect;
use crate::overlay::alpha::{finish_pills, Pill};
use crate::overlay::annotation::Annotation;
use crate::overlay::scene::{plan, to_local, Metrics, Scene};
use crate::overlay::session::Steps;
use crate::overlay::state::{claim_expiry, HookKind, Install};
use crate::overlay::wire::{Kind, Windows};
use crate::ui::theme::{
    detect_theme, scale_for_dpi, type_height_px, SysColors, Theme, Tokens, CONTROL_RADIUS_DP,
    PREFERRED_FONT_FACE, SPACE_SM_DP, TYPE_BODY_PX,
};

/// Posted to the main window by every overlay hook callback. `wparam` is a
/// [`Kind`], `lparam` one number (see `crate::overlay::wire`). The handler
/// decodes it, looks up the windows it needs on the main thread, and asks the
/// state machine what it means. `WM_APP + 30`: `+1` through `+22` are taken;
/// add any new `WM_APP_*` constant to `app.rs`'s `tests::ALL_WM_APP_IDS` too
/// (issue #163).
pub const WM_APP_OVERLAY_INPUT: u32 = WM_APP + 30;

/// `SetTimer`'s id for the overlay's one-shot age timer. It is killed the
/// moment it fires and by every hide path, so it never repeats (rule 5).
pub const OVERLAY_TIMER_ID: usize = 7;

const CLASS_NAME: &str = "Wingman.Overlay.5b1d9c42";
#[cfg(test)]
const TEST_CLASS_NAME: &str = "Wingman.Overlay.5b1d9c42.Test";

/// Horizontal and vertical padding inside a label, in design pixels.
const LABEL_PAD_X_DP: i32 = SPACE_SM_DP;
const LABEL_PAD_Y_DP: i32 = 4;
/// The underline mark's thickness, in design pixels (3 px in high contrast).
const MARK_DP: i32 = 2;
const MARK_HC_PX: i32 = 3;
/// The label outline: 1 px, 2 px in high contrast.
const BORDER_PX: i32 = 1;
const BORDER_HC_PX: i32 = 2;

/// A monotonic millisecond tick, the clock the Esc claim's expiry uses.
pub fn tick_ms() -> u64 {
    unsafe { GetTickCount64() }
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------

/// How the overlay is drawn on this monitor.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Look {
    pub tokens: Tokens,
    pub high_contrast: bool,
    pub dpi: u32,
    pub text_scale: f32,
}

impl Look {
    /// The live theme, at `dpi` and `text_scale`.
    pub(crate) fn current(dpi: u32, text_scale: f32) -> Look {
        let theme = detect_theme();
        Look::for_theme(theme, &SysColors::read(), dpi, text_scale)
    }

    pub(crate) fn for_theme(theme: Theme, sys: &SysColors, dpi: u32, text_scale: f32) -> Look {
        Look {
            tokens: Tokens::for_theme(theme, sys),
            high_contrast: theme == Theme::HighContrast,
            dpi: dpi.max(48),
            text_scale: text_scale.clamp(0.5, 2.0),
        }
    }

    fn px(&self, dp: i32) -> i32 {
        scale_for_dpi(dp, self.dpi)
    }
}

/// One drawn overlay: a premultiplied 32-bit DIB the size of the bounding box
/// of the marks, selected into a memory DC for `UpdateLayeredWindow`. The
/// memory is released when it drops, which is right after it is presented.
pub(crate) struct Frame {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    /// Where the window goes, in screen pixels.
    pub rect: ScreenRect,
    /// Where each placed label is, in screen pixels (placement order). Its
    /// length is how many annotations got a place.
    pub labels: Vec<ScreenRect>,
}

impl Frame {
    pub(crate) fn width(&self) -> i32 {
        self.rect.right - self.rect.left
    }

    pub(crate) fn height(&self) -> i32 {
        self.rect.bottom - self.rect.top
    }

    /// The BGRA bytes, rows top to bottom.
    pub(crate) fn pixels(&self) -> &[u8] {
        let len = (self.width().max(0) as usize) * (self.height().max(0) as usize) * 4;
        if self.bits.is_null() || len == 0 {
            return &[];
        }
        // SAFETY: `bits` points at the DIB section's pixel memory, which is
        // exactly width * height * 4 bytes and lives until `Drop`.
        unsafe { std::slice::from_raw_parts(self.bits, len) }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            let _ = DeleteObject(self.bitmap.into());
            let _ = DeleteDC(self.dc);
        }
    }
}

fn label_font(look: &Look) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: type_height_px(TYPE_BODY_PX, look.dpi, look.text_scale),
        lfWeight: FW_NORMAL.0 as i32,
        lfQuality: CLEARTYPE_QUALITY,
        ..Default::default()
    };
    for (i, unit) in PREFERRED_FONT_FACE
        .encode_utf16()
        .take(lf.lfFaceName.len() - 1)
        .enumerate()
    {
        lf.lfFaceName[i] = unit;
    }
    unsafe { CreateFontIndirectW(&lf) }
}

fn text_size(dc: HDC, text: &str) -> (i32, i32) {
    let wide: Vec<u16> = text.encode_utf16().collect();
    if wide.is_empty() {
        return (0, 0);
    }
    let mut size = SIZE::default();
    // SAFETY: `wide` is a live buffer and `size` a live out value.
    let ok = unsafe { GetTextExtentPoint32W(dc, &wide, &mut size) }.as_bool();
    if ok {
        (size.cx, size.cy)
    } else {
        (0, 0)
    }
}

fn rect_of(r: ScreenRect) -> RECT {
    RECT {
        left: r.left,
        top: r.top,
        right: r.right,
        bottom: r.bottom,
    }
}

/// Draws `annotations` for the monitor `monitor`: measures every label, asks
/// the pure planner where everything goes (keeping labels off `obstacles`, the
/// boxes of the page's other text), draws it into a DIB, and finishes
/// the alpha. `None` when nothing found a place or any step fails (the caller
/// then shows the summary card alone).
pub(crate) fn render(
    annotations: &[Annotation],
    obstacles: &[ScreenRect],
    monitor: ScreenRect,
    look: &Look,
) -> Option<Frame> {
    let border = if look.high_contrast {
        BORDER_HC_PX
    } else {
        BORDER_PX
    };
    let pad_x = look.px(LABEL_PAD_X_DP);
    let pad_y = look.px(LABEL_PAD_Y_DP);
    let font = label_font(look);
    // SAFETY: every GDI object created below is released on every path.
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        ReleaseDC(None, screen);
        if dc.is_invalid() {
            if !font.is_invalid() {
                let _ = DeleteObject(font.into());
            }
            return None;
        }
        let old_font = SelectObject(dc, font.into());
        let sizes: Vec<(i32, i32)> = annotations
            .iter()
            .map(|a| {
                let (w, h) = text_size(dc, &a.label);
                if w <= 0 || h <= 0 {
                    (0, 0)
                } else {
                    (
                        w.saturating_add(2 * (pad_x + border)),
                        h.saturating_add(2 * (pad_y + border)),
                    )
                }
            })
            .collect();
        let metrics = Metrics {
            gap: look.px(crate::overlay::layout::GAP_DESIGN_PX),
            mark: if look.high_contrast {
                MARK_HC_PX
            } else {
                look.px(MARK_DP).max(1)
            },
            halo: if look.high_contrast { 0 } else { 1 },
        };
        let result = plan(annotations, &sizes, obstacles, monitor, &metrics)
            .and_then(|scene| draw(&scene, annotations, look, border));
        // The measuring DC is done either way; the DIB has its own.
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
        let _ = DeleteDC(dc);
        result
    }
}

/// Draws `scene` into a fresh DIB, with the same font the labels were
/// measured in.
unsafe fn draw(
    scene: &Scene,
    annotations: &[Annotation],
    look: &Look,
    border: i32,
) -> Option<Frame> {
    let width = scene.window.right.saturating_sub(scene.window.left);
    let height = scene.window.bottom.saturating_sub(scene.window.top);
    if width <= 0 || height <= 0 || width > 16_384 || height > 16_384 {
        return None;
    }
    let screen = GetDC(None);
    let dc = CreateCompatibleDC(Some(screen));
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
    let bitmap = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
    ReleaseDC(None, screen);
    let bitmap = match bitmap {
        Ok(b) if !b.is_invalid() && !bits.is_null() && !dc.is_invalid() => b,
        other => {
            // A bitmap that was made but cannot be used (no bits, or no dc to
            // select it into) is still ours to free.
            if let Ok(b) = other {
                if !b.is_invalid() {
                    let _ = DeleteObject(b.into());
                }
            }
            if !dc.is_invalid() {
                let _ = DeleteDC(dc);
            }
            return None;
        }
    };
    let old = SelectObject(dc, bitmap.into());

    let label_font = label_font(look);
    let old_font = SelectObject(dc, label_font.into());
    SetBkMode(dc, TRANSPARENT);
    SetTextColor(dc, COLORREF(look.tokens.text));

    let surface = CreateSolidBrush(COLORREF(look.tokens.surface));
    let accent = CreateSolidBrush(COLORREF(look.tokens.mark_note));
    let local = |r: ScreenRect| to_local(r, scene.window);
    let mut pills: Vec<Pill> = Vec::new();

    for mark in &scene.marks {
        let halo = rect_of(local(mark.halo));
        let line = rect_of(local(mark.line));
        FillRect(dc, &halo, surface);
        FillRect(dc, &line, accent);
        pills.push(Pill {
            rect: local(mark.halo),
            radius: 0,
        });
    }
    let radius = if look.high_contrast {
        0
    } else {
        look.px(CONTROL_RADIUS_DP)
    };
    for placed in &scene.labels {
        let Some(a) = annotations.get(placed.index) else {
            continue;
        };
        let outer = local(placed.rect);
        let inner = ScreenRect {
            left: outer.left + border,
            top: outer.top + border,
            right: outer.right - border,
            bottom: outer.bottom - border,
        };
        FillRect(dc, &rect_of(outer), accent);
        FillRect(dc, &rect_of(inner), surface);
        crate::ui::text::draw_text_line(
            dc,
            &a.label,
            rect_of(inner),
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
        pills.push(Pill {
            rect: outer,
            radius,
        });
    }

    SelectObject(dc, old_font);
    let _ = DeleteObject(label_font.into());
    let _ = DeleteObject(surface.into());
    let _ = DeleteObject(accent.into());
    let _ = GdiFlush();

    let len = (width as usize) * (height as usize) * 4;
    // SAFETY: the DIB section is `width * height * 4` bytes at `bits`.
    let buf = std::slice::from_raw_parts_mut(bits.cast::<u8>(), len);
    if !finish_pills(buf, width as u32, height as u32, &pills) {
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        return None;
    }
    Some(Frame {
        dc,
        bitmap,
        old,
        bits: bits.cast::<u8>(),
        rect: scene.window,
        labels: scene.labels.iter().map(|l| l.rect).collect(),
    })
}

// ---------------------------------------------------------------------------
// The window
// ---------------------------------------------------------------------------

static CLASS_INIT: Once = Once::new();
static CLASS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
#[cfg(test)]
static TEST_CLASS_INIT: Once = Once::new();
#[cfg(test)]
static TEST_CLASS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_class(instance: HINSTANCE, name: &'static str) -> bool {
    let (init, ok) = {
        #[cfg(test)]
        {
            if name == TEST_CLASS_NAME {
                (&TEST_CLASS_INIT, &TEST_CLASS_OK)
            } else {
                (&CLASS_INIT, &CLASS_OK)
            }
        }
        #[cfg(not(test))]
        {
            (&CLASS_INIT, &CLASS_OK)
        }
    };
    init.call_once(|| {
        let class_name = wide_z(name);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(wndproc),
            hInstance: instance,
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let registered = unsafe { RegisterClassExW(&wc) } != 0;
        let _ = ok.set(registered);
    });
    ok.get().copied().unwrap_or(false)
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Never takes focus, so typing keeps going to the app underneath.
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        // Read-only: every click goes to the app underneath.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),
        // Painted by `UpdateLayeredWindow`, never by a message.
        WM_ERASEBKGND => LRESULT(1),
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Excludes `hwnd` from every capture (Wingman's own, a screen share, a
/// recorder): `WDA_EXCLUDEFROMCAPTURE`, MEASURED to hold for `BitBlt`, the
/// Windows Graphics Capture path and `PrintWindow` (spec "Excluding the
/// overlay from capture"). `false` when the call fails.
fn exclude_from_capture(hwnd: HWND) -> bool {
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE) }.is_ok()
}

fn leave_capturable(_hwnd: HWND) -> bool {
    true
}

/// The one overlay window. Created hidden, shown by [`OverlayWindow::present`],
/// destroyed on drop.
pub(crate) struct OverlayWindow {
    hwnd: HWND,
}

impl OverlayWindow {
    /// Creates the window, hidden and excluded from capture. `capturable`
    /// (the `[overlay] capturable` demo setting) skips the exclusion.
    /// `Err` leaves no window behind: if the exclusion cannot be arranged,
    /// there is no overlay.
    pub(crate) fn create(
        instance: HINSTANCE,
        rect: ScreenRect,
        capturable: bool,
    ) -> Result<Self, &'static str> {
        let apply: fn(HWND) -> bool = if capturable {
            leave_capturable
        } else {
            exclude_from_capture
        };
        Self::create_with(instance, CLASS_NAME, rect, apply)
    }

    /// [`Self::create`] under the test-only class name (Hard Rule 9: tests
    /// never register a production name).
    #[cfg(test)]
    fn create_for_test(
        instance: HINSTANCE,
        rect: ScreenRect,
        capturable: bool,
    ) -> Result<Self, &'static str> {
        let apply: fn(HWND) -> bool = if capturable {
            leave_capturable
        } else {
            exclude_from_capture
        };
        Self::create_with(instance, TEST_CLASS_NAME, rect, apply)
    }

    fn create_with(
        instance: HINSTANCE,
        class: &'static str,
        rect: ScreenRect,
        apply_affinity: fn(HWND) -> bool,
    ) -> Result<Self, &'static str> {
        if !ensure_class(instance, class) {
            return Err("could not register the overlay window class");
        }
        let class_name = wide_z(class);
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR::null(),
                WS_POPUP,
                rect.left,
                rect.top,
                rect.right.saturating_sub(rect.left).max(1),
                rect.bottom.saturating_sub(rect.top).max(1),
                None,
                None,
                Some(instance),
                None,
            )
        };
        let Ok(hwnd) = hwnd else {
            return Err("could not create the overlay window");
        };
        if !apply_affinity(hwnd) {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            return Err("could not keep the overlay out of screen captures");
        }
        Ok(Self { hwnd })
    }

    pub(crate) fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Pushes `frame` to the screen and shows the window without activating
    /// it. `false`, with the window still hidden, if Windows refuses.
    pub(crate) fn present(&self, frame: &Frame) -> bool {
        if frame.pixels().is_empty() {
            return false;
        }
        let dst = POINT {
            x: frame.rect.left,
            y: frame.rect.top,
        };
        let size = SIZE {
            cx: frame.width(),
            cy: frame.height(),
        };
        let src = POINT { x: 0, y: 0 };
        let blend = BLENDFUNCTION {
            BlendOp: AC_SRC_OVER as u8,
            BlendFlags: 0,
            SourceConstantAlpha: 255,
            AlphaFormat: AC_SRC_ALPHA as u8,
        };
        unsafe {
            let screen = GetDC(None);
            let ok = UpdateLayeredWindow(
                self.hwnd,
                Some(screen),
                Some(&dst),
                Some(&size),
                Some(frame.dc),
                Some(&src),
                COLORREF(0),
                Some(&blend),
                ULW_ALPHA,
            )
            .is_ok();
            ReleaseDC(None, screen);
            if !ok {
                return false;
            }
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
        true
    }
}

impl Drop for OverlayWindow {
    fn drop(&mut self) {
        // Never panic on the main thread: nothing useful to do on failure.
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The display affinity of `hwnd`, for the check that the exclusion held.
#[cfg(test)]
fn affinity_of(hwnd: HWND) -> Option<WINDOW_DISPLAY_AFFINITY> {
    let mut a = 0u32;
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowDisplayAffinity(hwnd, &mut a) }
        .ok()?;
    Some(WINDOW_DISPLAY_AFFINITY(a))
}

// ---------------------------------------------------------------------------
// Hooks and timer
// ---------------------------------------------------------------------------

/// The main window every callback posts to. Set while any step is installed.
static OWNER: AtomicIsize = AtomicIsize::new(0);

/// The `Kind` a `SetWinEventHook` event is reported as. Pure, so the mapping
/// is tested.
pub(crate) fn kind_for_event(event: u32) -> Option<Kind> {
    match event {
        EVENT_OBJECT_LOCATIONCHANGE => Some(Kind::Moved),
        EVENT_OBJECT_DESTROY => Some(Kind::Destroyed),
        EVENT_SYSTEM_MINIMIZESTART => Some(Kind::Minimized),
        EVENT_SYSTEM_FOREGROUND => Some(Kind::Foreground),
        _ => None,
    }
}

/// The out-of-context `SetWinEventHook` callback. It filters to window
/// objects and posts; it decides nothing and looks nothing up.
unsafe extern "system" fn win_event_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    _id_child: i32,
    _thread: u32,
    _time: u32,
) {
    if id_object != OBJID_WINDOW.0 || hwnd.0.is_null() {
        return;
    }
    let Some(kind) = kind_for_event(event) else {
        return;
    };
    let owner = OWNER.load(Ordering::Relaxed);
    if owner == 0 {
        return;
    }
    let _ = PostMessageW(
        Some(HWND(owner as *mut _)),
        WM_APP_OVERLAY_INPUT,
        WPARAM(kind as usize),
        LPARAM(hwnd.0 as isize),
    );
}

/// The real hooks and timer behind [`Steps`].
pub(crate) struct Win32Steps {
    owner: HWND,
    age_ms: u64,
    /// Process ids of the target windows: one location hook each.
    target_pids: Vec<u32>,
    location: Vec<HWINEVENTHOOK>,
    destroy: Option<HWINEVENTHOOK>,
    minimize: Option<HWINEVENTHOOK>,
    foreground: Option<HWINEVENTHOOK>,
}

impl Win32Steps {
    pub(crate) fn new(owner: HWND, age_ms: u64, targets: &[isize]) -> Self {
        let mut target_pids = Vec::new();
        for &t in targets {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(HWND(t as *mut _), Some(&mut pid)) };
            if pid != 0 && !target_pids.contains(&pid) {
                target_pids.push(pid);
            }
        }
        Self {
            owner,
            age_ms,
            target_pids,
            location: Vec::new(),
            destroy: None,
            minimize: None,
            foreground: None,
        }
    }

    fn hook_event(event: u32, pid: u32) -> Option<HWINEVENTHOOK> {
        let h = unsafe {
            SetWinEventHook(
                event,
                event,
                None,
                Some(win_event_proc),
                pid,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            )
        };
        (!h.is_invalid()).then_some(h)
    }

    fn unhook(h: HWINEVENTHOOK) {
        let _ = unsafe { UnhookWinEvent(h) };
    }
}

impl Steps for Win32Steps {
    fn install(&mut self, step: Install) -> bool {
        OWNER.store(self.owner.0 as isize, Ordering::Relaxed);
        match step {
            Install::Hook(HookKind::LocationChange) => {
                if self.target_pids.is_empty() {
                    return false;
                }
                for &pid in &self.target_pids.clone() {
                    match Self::hook_event(EVENT_OBJECT_LOCATIONCHANGE, pid) {
                        Some(h) => self.location.push(h),
                        None => {
                            // Leave nothing of this step behind.
                            for h in self.location.drain(..) {
                                Self::unhook(h);
                            }
                            return false;
                        }
                    }
                }
                true
            }
            Install::Hook(HookKind::Destroy) => {
                self.destroy = Self::hook_event(EVENT_OBJECT_DESTROY, 0);
                self.destroy.is_some()
            }
            Install::Hook(HookKind::MinimizeStart) => {
                self.minimize = Self::hook_event(EVENT_SYSTEM_MINIMIZESTART, 0);
                self.minimize.is_some()
            }
            Install::Hook(HookKind::Foreground) => {
                self.foreground = Self::hook_event(EVENT_SYSTEM_FOREGROUND, 0);
                self.foreground.is_some()
            }
            Install::Hook(HookKind::Mouse) => crate::dismiss::arm_overlay(self.owner),
            Install::Hook(HookKind::Keyboard) => {
                crate::hotkey::arm_overlay_keys(claim_expiry(tick_ms(), self.age_ms))
            }
            Install::AgeTimer => {
                let ms = u32::try_from(self.age_ms).unwrap_or(u32::MAX);
                unsafe { SetTimer(Some(self.owner), OVERLAY_TIMER_ID, ms, None) != 0 }
            }
        }
    }

    fn remove(&mut self, step: Install) {
        match step {
            Install::Hook(HookKind::LocationChange) => {
                for h in self.location.drain(..) {
                    Self::unhook(h);
                }
            }
            Install::Hook(HookKind::Destroy) => {
                if let Some(h) = self.destroy.take() {
                    Self::unhook(h);
                }
            }
            Install::Hook(HookKind::MinimizeStart) => {
                if let Some(h) = self.minimize.take() {
                    Self::unhook(h);
                }
            }
            Install::Hook(HookKind::Foreground) => {
                if let Some(h) = self.foreground.take() {
                    Self::unhook(h);
                }
            }
            Install::Hook(HookKind::Mouse) => crate::dismiss::disarm_overlay(),
            Install::Hook(HookKind::Keyboard) => crate::hotkey::disarm_overlay_keys(),
            Install::AgeTimer => unsafe {
                let _ = KillTimer(Some(self.owner), OVERLAY_TIMER_ID);
            },
        }
    }
}

impl Drop for Win32Steps {
    fn drop(&mut self) {
        // A step set that is dropped still installed (a path that forgot to
        // hide) must not leave a hook behind.
        for h in self.location.drain(..) {
            Self::unhook(h);
        }
        for h in [
            self.destroy.take(),
            self.minimize.take(),
            self.foreground.take(),
        ]
        .into_iter()
        .flatten()
        {
            Self::unhook(h);
        }
        // The mouse and key claims and the age timer are not handles this
        // struct owns, but a path that forgot to hide must not leave them
        // armed either. All three are idempotent and cheap.
        crate::dismiss::disarm_overlay();
        crate::hotkey::disarm_overlay_keys();
        unsafe {
            let _ = KillTimer(Some(self.owner), OVERLAY_TIMER_ID);
        }
        OWNER.store(0, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Window lookups (main thread only)
// ---------------------------------------------------------------------------

/// The real [`Windows`] lookups. Only the main thread calls these, never a
/// hook callback.
pub(crate) struct WinLookup;

fn root_of_window(h: HWND) -> isize {
    if h.0.is_null() {
        return 0;
    }
    let root = unsafe { GetAncestor(h, GA_ROOT) };
    if root.0.is_null() {
        h.0 as isize
    } else {
        root.0 as isize
    }
}

impl Windows for WinLookup {
    fn root_at(&self, x: i32, y: i32) -> isize {
        root_of_window(unsafe { WindowFromPoint(POINT { x, y }) })
    }

    fn foreground(&self) -> isize {
        root_of_window(unsafe { GetForegroundWindow() })
    }

    fn root_of(&self, h: isize) -> isize {
        root_of_window(HWND(h as *mut _))
    }
}

/// One on-screen top-level window, as `EnumWindows` hands it back.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Stacked {
    pub hwnd: isize,
    pub rect: ScreenRect,
    pub pid: u32,
    /// `WS_EX_TRANSPARENT`: mouse input falls through it.
    pub click_through: bool,
}

/// The window a mark at `(x, y)` was drawn over: the first window in `stack`
/// (topmost first) that contains the point, skipping Wingman's own windows
/// (a card or palette in front) and click-through windows. `None` when
/// nothing qualifies. A point probe (`WindowFromPoint`) cannot do this: it
/// answers with Wingman's own card and the target is lost.
pub(crate) fn target_at(stack: &[Stacked], x: i32, y: i32, own_pid: u32) -> Option<isize> {
    stack
        .iter()
        .find(|w| {
            w.pid != own_pid
                && !w.click_through
                && x >= w.rect.left
                && x < w.rect.right
                && y >= w.rect.top
                && y < w.rect.bottom
        })
        .map(|w| w.hwnd)
}

/// The desktop and taskbar windows. They cover large areas but are never what
/// a mark was drawn over.
fn is_shell_class(class: &str) -> bool {
    matches!(
        class,
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
    )
}

/// A layered window set to alpha 0 (`LWA_ALPHA`, value 0): drawn as nothing.
fn is_invisible_layered(flags: u32, alpha: u8) -> bool {
    const LWA_ALPHA: u32 = 2;
    flags & LWA_ALPHA != 0 && alpha == 0
}

/// The visible, un-cloaked, un-minimised top-level windows, topmost first.
/// `None` when the enumeration fails: the caller treats that as unknown.
fn window_stack() -> Option<Vec<Stacked>> {
    use windows::Win32::Graphics::Dwm::{
        DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetClassNameW, GetLayeredWindowAttributes, GetWindowLongW, GetWindowRect,
        IsIconic, IsWindowVisible, GWL_EXSTYLE, WS_EX_LAYERED,
    };
    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> windows::core::BOOL {
        // SAFETY: `lparam` is the `&mut Vec<Stacked>` passed below, which
        // outlives `EnumWindows`.
        let out = unsafe { &mut *(lparam.0 as *mut Vec<Stacked>) };
        let mut cloaked = 0u32;
        let _ = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_CLOAKED,
                &mut cloaked as *mut u32 as *mut core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        // The visible frame, without the invisible ~7 px DWM shadow border;
        // the plain window rect when DWM cannot say.
        let mut r = RECT::default();
        let dwm_ok = unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut core::ffi::c_void,
                std::mem::size_of::<RECT>() as u32,
            )
        }
        .is_ok();
        let got = dwm_ok || unsafe { GetWindowRect(hwnd, &mut r) }.is_ok();
        let ex = unsafe { GetWindowLongW(hwnd, GWL_EXSTYLE) } as u32;
        let mut buf = [0u16; 64];
        let n = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
        let shell = is_shell_class(&String::from_utf16_lossy(&buf[..n.min(buf.len())]));
        let mut invisible = false;
        if ex & WS_EX_LAYERED.0 != 0 {
            let (mut alpha, mut flags) = (0u8, Default::default());
            if unsafe { GetLayeredWindowAttributes(hwnd, None, Some(&mut alpha), Some(&mut flags)) }
                .is_ok()
            {
                invisible = is_invisible_layered(flags.0, alpha);
            }
        }
        if got
            && cloaked == 0
            && !shell
            && !invisible
            && unsafe { IsWindowVisible(hwnd) }.as_bool()
            && !unsafe { IsIconic(hwnd) }.as_bool()
        {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            out.push(Stacked {
                hwnd: hwnd.0 as isize,
                rect: ScreenRect {
                    left: r.left,
                    top: r.top,
                    right: r.right,
                    bottom: r.bottom,
                },
                pid,
                click_through: ex & WS_EX_TRANSPARENT.0 != 0,
            });
        }
        true.into()
    }
    let mut out: Vec<Stacked> = Vec::new();
    // SAFETY: `out` outlives the call; the callback is its only user.
    unsafe { EnumWindows(Some(visit), LPARAM(&mut out as *mut Vec<Stacked> as isize)) }.ok()?;
    Some(out)
}

/// The top-level windows under the marks, one per distinct window: what
/// the overlay watches for change. Found at each annotation rect's centre
/// from the z-ordered window list. Empty when the list cannot be read, which
/// the caller treats as "do not show" (no way to go stale is the outcome to
/// avoid).
pub(crate) fn targets_under(annotations: &[Annotation]) -> Vec<isize> {
    let Some(stack) = window_stack() else {
        return Vec::new();
    };
    targets_in(&stack, annotations, unsafe {
        windows::Win32::System::Threading::GetCurrentProcessId()
    })
}

fn targets_in(stack: &[Stacked], annotations: &[Annotation], own_pid: u32) -> Vec<isize> {
    let mut out: Vec<isize> = Vec::new();
    for rect in annotations.iter().flat_map(|a| a.rects.iter()) {
        let x = rect.left + (rect.right.saturating_sub(rect.left)) / 2;
        let y = rect.top + (rect.bottom.saturating_sub(rect.top)) / 2;
        if let Some(w) = target_at(stack, x, y, own_pid) {
            if !out.contains(&w) {
                out.push(w);
            }
        }
    }
    out
}

/// The effective DPI of the monitor nearest `rect`, 96 if Windows will not say.
pub(crate) fn dpi_near(rect: ScreenRect) -> u32 {
    let r = rect_of(rect);
    let monitor = unsafe { MonitorFromRect(&r, MONITOR_DEFAULTTONEAREST) };
    let (mut x, mut y) = (0u32, 0u32);
    match unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) } {
        Ok(()) if x >= 48 => x,
        _ => 96,
    }
}

/// Whether the overlay's own `WDA_NONE` guard value is what a normal window
/// reports: kept so a future caller can assert the exclusion really holds.
#[allow(dead_code)]
const NOT_EXCLUDED: WINDOW_DISPLAY_AFFINITY = WDA_NONE;

// ---------------------------------------------------------------------------
// The UI gallery's picture (debug builds only)
// ---------------------------------------------------------------------------

/// The gallery's `overlay-notes` picture: a GDI-drawn fixture page, three
/// marks over it (a label beside a phrase at the top edge, a two-word anchor,
/// an anchor that spans two lines), composited in
/// software and saved as a PNG. It does not use a window: `PrintWindow` on a
/// layered window renders black (#626), and the pixels the real window shows
/// are exactly the frame composited here.
#[cfg(debug_assertions)]
pub(crate) fn gallery_png(
    theme: Option<Theme>,
    dpi: u32,
    path: &std::path::Path,
) -> anyhow::Result<()> {
    use crate::ui::theme::rgb;
    let theme = theme.unwrap_or_else(detect_theme);
    let sys = if theme == Theme::HighContrast {
        crate::ui::theme::HC_BLACK
    } else {
        SysColors::read()
    };
    let look = Look::for_theme(theme, &sys, dpi, 1.0);
    // A page is the user's, not Wingman's: light for light, dark for the rest.
    let (page_bg, page_ink) = match theme {
        Theme::Light => (rgb(255, 255, 255), rgb(32, 32, 32)),
        Theme::Dark => (rgb(24, 24, 24), rgb(225, 225, 225)),
        Theme::HighContrast => (0, rgb(255, 255, 255)),
    };
    let (w, h) = (look.px(640), look.px(138));
    let (mut page, words) = fixture_page(&look, w, h, page_bg, page_ink)?;
    let word = |line: usize, index: usize| -> ScreenRect { words[line][index] };
    let join = |a: ScreenRect, b: ScreenRect| ScreenRect {
        left: a.left.min(b.left),
        top: a.top.min(b.top),
        right: a.right.max(b.right),
        bottom: a.bottom.max(b.bottom),
    };
    let marks = [
        // "Thursday at 9:00 PT.", on the top line: the label goes beside it.
        Annotation::note(vec![join(word(0, 3), word(0, 6))], "Thu 17:00 your time"),
        // "quarterly report": two words, one anchor.
        Annotation::note(
            vec![join(word(1, 3), word(1, 4))],
            "Longest word: quarterly",
        ),
        // "before" then "the end": the same mark across a line break.
        Annotation::note(
            vec![word(1, 7), join(word(2, 0), word(2, 2))],
            "Deadline, end of day",
        ),
    ];
    let annotations: Vec<Annotation> = marks.into_iter().flatten().collect();
    let monitor = ScreenRect {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    let obstacles: Vec<ScreenRect> = words.iter().flatten().copied().collect();
    let frame = render(&annotations, &obstacles, monitor, &look)
        .ok_or_else(|| anyhow::anyhow!("nothing placed"))?;
    anyhow::ensure!(
        frame.labels.len() == annotations.len(),
        "{} of {} marks placed",
        frame.labels.len(),
        annotations.len()
    );
    // Composite the premultiplied frame over the opaque page.
    let src = frame.pixels();
    for y in 0..frame.height() {
        for x in 0..frame.width() {
            let (px, py) = (frame.rect.left + x, frame.rect.top + y);
            let si = ((y * frame.width() + x) * 4) as usize;
            let di = ((py * w + px) * 4) as usize;
            let a = u32::from(src[si + 3]);
            for k in 0..3 {
                let over =
                    u32::from(src[si + k]) + (u32::from(page[di + k]) * (255 - a) + 127) / 255;
                page[di + k] = over.min(255) as u8;
            }
        }
    }
    // BGRA -> RGBA, opaque.
    for px in page.chunks_exact_mut(4) {
        px.swap(0, 2);
        px[3] = 255;
    }
    let img: image::RgbaImage = image::ImageBuffer::from_raw(w as u32, h as u32, page)
        .ok_or_else(|| anyhow::anyhow!("bad overlay gallery buffer"))?;
    img.save(path)
        .map_err(|e| anyhow::anyhow!("failed to save {path:?}: {e}"))?;
    Ok(())
}

/// The fixture page: three lines of text on `bg`, as BGRA bytes, and each
/// word's rectangle (line, then word index), so the gallery can anchor to words
/// the way the matcher would.
#[cfg(debug_assertions)]
fn fixture_page(
    look: &Look,
    w: i32,
    h: i32,
    bg: u32,
    ink: u32,
) -> anyhow::Result<(Vec<u8>, Vec<Vec<ScreenRect>>)> {
    const LINES: [&str; 3] = [
        "Meeting moved to Thursday at 9:00 PT.",
        "Please send the quarterly report to Priya before",
        "the end of the day. Thank you for your patience.",
    ];
    // SAFETY: every GDI object is released before returning.
    unsafe {
        let screen = GetDC(None);
        let dc = CreateCompatibleDC(Some(screen));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut std::ffi::c_void = std::ptr::null_mut();
        let bitmap = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0);
        ReleaseDC(None, screen);
        let bitmap = bitmap.map_err(|e| anyhow::anyhow!("CreateDIBSection: {e}"))?;
        anyhow::ensure!(!bits.is_null() && !dc.is_invalid(), "no DIB for the page");
        let old = SelectObject(dc, bitmap.into());
        let mut lf = LOGFONTW {
            lfHeight: -look.px(16),
            lfWeight: FW_NORMAL.0 as i32,
            lfQuality: CLEARTYPE_QUALITY,
            ..Default::default()
        };
        for (i, unit) in PREFERRED_FONT_FACE.encode_utf16().take(31).enumerate() {
            lf.lfFaceName[i] = unit;
        }
        let font = CreateFontIndirectW(&lf);
        let old_font = SelectObject(dc, font.into());
        let brush = CreateSolidBrush(COLORREF(bg));
        FillRect(
            dc,
            &RECT {
                left: 0,
                top: 0,
                right: w,
                bottom: h,
            },
            brush,
        );
        let _ = DeleteObject(brush.into());
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(ink));
        let tops = [look.px(6), look.px(70), look.px(94)];
        let mut words: Vec<Vec<ScreenRect>> = Vec::new();
        for (line, top) in LINES.iter().zip(tops) {
            let left = look.px(16);
            let mut row = Vec::new();
            let mut start = 0usize;
            for word in line.split(' ') {
                let before = text_size(dc, &line[..start]).0;
                let (ww, wh) = text_size(dc, word);
                row.push(ScreenRect {
                    left: left + before,
                    top,
                    right: left + before + ww,
                    bottom: top + wh,
                });
                start += word.len() + 1;
            }
            crate::ui::text::draw_text_line(
                dc,
                line,
                RECT {
                    left,
                    top,
                    right: w,
                    bottom: top + look.px(24),
                },
                DT_NOPREFIX | DT_SINGLELINE,
            );
            words.push(row);
        }
        let _ = GdiFlush();
        let len = (w as usize) * (h as usize) * 4;
        let page = std::slice::from_raw_parts(bits.cast::<u8>(), len).to_vec();
        SelectObject(dc, old_font);
        let _ = DeleteObject(font.into());
        SelectObject(dc, old);
        let _ = DeleteObject(bitmap.into());
        let _ = DeleteDC(dc);
        Ok((page, words))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `render` with no other text on the page.
    fn render(annotations: &[Annotation], monitor: ScreenRect, look: &Look) -> Option<Frame> {
        super::render(annotations, &[], monitor, look)
    }
    use crate::overlay::state::DEFAULT_AGE_MS;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn stacked(hwnd: isize, rect: ScreenRect, pid: u32, click_through: bool) -> Stacked {
        Stacked {
            hwnd,
            rect,
            pid,
            click_through,
        }
    }

    #[test]
    fn target_at_skips_own_and_click_through_windows_in_front() {
        const OWN: u32 = 7;
        let stack = [
            // Wingman's card, in front of everything.
            stacked(1, r(0, 0, 500, 500), OWN, false),
            // A click-through overlay from another app.
            stacked(2, r(0, 0, 500, 500), 9, true),
            // The window the mark was drawn over.
            stacked(3, r(100, 100, 400, 400), 9, false),
            stacked(4, r(0, 0, 1000, 1000), 9, false),
        ];
        assert_eq!(target_at(&stack, 250, 250, OWN), Some(3));
        // Outside the third window the one behind it is the answer.
        assert_eq!(target_at(&stack, 450, 450, OWN), Some(4));
        // Nothing there, or nothing but Wingman: unknown, not a guess.
        assert_eq!(target_at(&stack, 2000, 2000, OWN), None);
        assert_eq!(target_at(&stack[..1], 250, 250, OWN), None);
        assert_eq!(target_at(&[], 250, 250, OWN), None);
    }

    #[test]
    fn targets_in_gives_one_window_per_distinct_target() {
        let stack = [
            stacked(3, r(0, 0, 400, 400), 9, false),
            stacked(4, r(400, 0, 800, 400), 9, false),
        ];
        let marks: Vec<Annotation> = [r(10, 10, 50, 30), r(60, 10, 90, 30), r(500, 10, 540, 30)]
            .into_iter()
            .map(|rc| Annotation::note(vec![rc], "x").unwrap())
            .collect();
        assert_eq!(targets_in(&stack, &marks, 7), vec![3, 4]);
        // A mark over nothing adds nothing; no windows means no targets.
        assert!(targets_in(&[], &marks, 7).is_empty());
    }

    /// Real windows of a system class (no Wingman name is registered): a
    /// target, and topmost windows over its centre, one normal and one
    /// click-through. `target_at` names the window under the cover once the
    /// cover is Wingman's own; a point probe names whatever is on top.
    #[test]
    fn target_at_over_real_windows_with_a_topmost_cover() {
        use windows::Win32::UI::WindowsAndMessaging::{WS_EX_LAYERED, WS_VISIBLE};
        let _dpi = DpiAware::enter();
        let make = |ex, rc: ScreenRect| unsafe {
            CreateWindowExW(
                ex,
                windows::core::w!("STATIC"),
                PCWSTR::null(),
                WS_POPUP | WS_VISIBLE,
                rc.left,
                rc.top,
                rc.right - rc.left,
                rc.bottom - rc.top,
                None,
                None,
                None,
                None,
            )
            .unwrap()
        };
        let target = make(WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE, r(300, 300, 700, 600));
        let cover = make(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            r(400, 400, 600, 500),
        );
        let ghost = make(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE | WS_EX_LAYERED | WS_EX_TRANSPARENT,
            r(400, 400, 600, 500),
        );
        let _cleanup = Destroy(vec![ghost, cover, target]);
        let (t, c, g) = (target.0 as isize, cover.0 as isize, ghost.0 as isize);
        let (x, y) = (500, 450);
        let own = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };

        let stack = window_stack().expect("EnumWindows works");
        let order = |h: isize| stack.iter().position(|w| w.hwnd == h);
        let (ot, oc) = (order(t), order(c));
        assert!(order(g).is_some() && ot.is_some() && oc.is_some());
        assert!(oc < ot, "a topmost window is above a normal one");

        // Foreign view: the cover is another app's visible window, so it is
        // what is under the mark; the click-through one is never an answer.
        // Only the three test windows: whatever else this machine has on top
        // is not under test.
        let foreign: Vec<Stacked> = stack
            .iter()
            .filter(|w| [t, c, g].contains(&w.hwnd))
            .map(|w| Stacked { pid: 1, ..*w })
            .collect();
        assert_eq!(target_at(&foreign, x, y, own), Some(c));
        // The cover is Wingman's own: the target underneath is found.
        let mut mixed = foreign.clone();
        for w in mixed.iter_mut().filter(|w| w.hwnd == c) {
            w.pid = own;
        }
        assert_eq!(target_at(&mixed, x, y, own), Some(t));
    }

    /// Destroys its windows on drop, so a failed assert cannot leak topmost
    /// test windows onto the desktop.
    struct Destroy(Vec<HWND>);
    impl Drop for Destroy {
        fn drop(&mut self) {
            for h in &self.0 {
                let _ = unsafe { DestroyWindow(*h) };
            }
        }
    }

    /// Per-monitor-v2 on this thread, as the app runs, restored on drop. DWM
    /// frame bounds are physical pixels; a DPI-unaware test thread would see
    /// scaled `GetWindowRect` values and disagree with them.
    struct DpiAware(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);
    impl DpiAware {
        fn enter() -> Self {
            use windows::Win32::UI::HiDpi::{
                SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            };
            Self(unsafe {
                SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)
            })
        }
    }
    impl Drop for DpiAware {
        fn drop(&mut self) {
            unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0) };
        }
    }

    #[test]
    fn shell_windows_are_never_a_target() {
        for c in [
            "Progman",
            "WorkerW",
            "Shell_TrayWnd",
            "Shell_SecondaryTrayWnd",
        ] {
            assert!(is_shell_class(c), "{c}");
        }
        for c in ["Notepad", "Chrome_WidgetWin_1", "STATIC", ""] {
            assert!(!is_shell_class(c), "{c}");
        }
    }

    #[test]
    fn fully_transparent_layered_windows_are_skipped() {
        const LWA_ALPHA: u32 = 2;
        const LWA_COLORKEY: u32 = 1;
        assert!(is_invisible_layered(LWA_ALPHA, 0));
        assert!(!is_invisible_layered(LWA_ALPHA, 1));
        assert!(!is_invisible_layered(LWA_ALPHA, 255));
        // Alpha is not in play (colour key only): the alpha byte means nothing.
        assert!(!is_invisible_layered(LWA_COLORKEY, 0));
        assert!(!is_invisible_layered(0, 0));
    }

    fn instance() -> HINSTANCE {
        HINSTANCE(
            unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
                .unwrap()
                .0,
        )
    }

    fn look(theme: Theme, dpi: u32) -> Look {
        let sys = crate::ui::theme::HC_BLACK;
        Look::for_theme(theme, &sys, dpi, 1.0)
    }

    fn note(rect: ScreenRect, label: &str) -> Annotation {
        Annotation::note(vec![rect], label).unwrap()
    }

    fn alpha_at(f: &Frame, x: i32, y: i32) -> u8 {
        let local_x = x - f.rect.left;
        let local_y = y - f.rect.top;
        f.pixels()[((local_y * f.width() + local_x) * 4 + 3) as usize]
    }

    #[test]
    fn a_label_renders_inside_the_monitor_with_opaque_pill_pixels() {
        let monitor = r(0, 0, 800, 600);
        let anchor = r(200, 300, 260, 316);
        for theme in [Theme::Light, Theme::Dark, Theme::HighContrast] {
            let frame = render(
                &[note(anchor, "Longest word: administration")],
                monitor,
                &look(theme, 96),
            )
            .expect("a label with room renders");
            assert_eq!(frame.labels.len(), 1);
            let w = frame.rect;
            assert!(w.left >= 0 && w.top >= 0 && w.right <= 800 && w.bottom <= 600);
            assert_eq!(
                frame.pixels().len() as i32,
                frame.width() * frame.height() * 4
            );
            let label = frame.labels[0];
            // Every pixel strictly inside the label is opaque (the alpha
            // trap: GDI text would otherwise leave alpha 0 and vanish).
            for y in (label.top + 2)..(label.bottom - 2) {
                for x in (label.left + 6)..(label.right - 6) {
                    assert_eq!(alpha_at(&frame, x, y), 255, "{theme:?} ({x},{y})");
                }
            }
            // The label sits above its text and never over it.
            assert!(label.bottom <= anchor.top, "{theme:?}");
        }
    }

    #[test]
    fn the_text_is_really_drawn_not_just_a_blank_pill() {
        let frame = render(
            &[note(r(200, 300, 260, 316), "Hello")],
            r(0, 0, 800, 600),
            &look(Theme::Light, 96),
        )
        .unwrap();
        let label = frame.labels[0];
        let mut distinct = std::collections::HashSet::new();
        for y in (label.top + 2)..(label.bottom - 2) {
            for x in (label.left + 2)..(label.right - 2) {
                let i =
                    (((y - frame.rect.top) * frame.width() + (x - frame.rect.left)) * 4) as usize;
                distinct.insert([
                    frame.pixels()[i],
                    frame.pixels()[i + 1],
                    frame.pixels()[i + 2],
                ]);
            }
        }
        assert!(
            distinct.len() > 2,
            "a filled pill with text has more than its fill and ink: {}",
            distinct.len()
        );
    }

    #[test]
    fn pixels_outside_the_labels_and_marks_stay_transparent() {
        let frame = render(
            &[note(r(200, 300, 260, 316), "Hi")],
            r(0, 0, 800, 600),
            &look(Theme::Light, 96),
        )
        .unwrap();
        // The window is the bounding box, so its far corner is empty unless a
        // mark reaches it: the box corner between label and mark is clear.
        let w = frame.rect;
        let corner_clear = (w.left..w.right)
            .flat_map(|x| (w.top..w.bottom).map(move |y| (x, y)))
            .any(|(x, y)| alpha_at(&frame, x, y) == 0);
        assert!(corner_clear, "some of the box is transparent");
    }

    #[test]
    fn the_overlay_is_bigger_at_a_higher_dpi() {
        let small = render(
            &[note(r(200, 300, 260, 316), "Hello")],
            r(0, 0, 1600, 1200),
            &look(Theme::Light, 96),
        )
        .unwrap();
        let big = render(
            &[note(r(200, 300, 260, 316), "Hello")],
            r(0, 0, 1600, 1200),
            &look(Theme::Light, 192),
        )
        .unwrap();
        let lw = |f: &Frame| f.labels[0].right - f.labels[0].left;
        assert!(
            lw(&big) > lw(&small) * 3 / 2,
            "{} vs {}",
            lw(&big),
            lw(&small)
        );
    }

    #[test]
    fn nothing_that_fits_means_no_frame() {
        let tiny = r(0, 0, 20, 20);
        assert!(render(
            &[note(
                r(5, 5, 15, 15),
                "A label far too wide for this monitor"
            )],
            tiny,
            &look(Theme::Light, 96)
        )
        .is_none());
        assert!(render(&[], r(0, 0, 800, 600), &look(Theme::Light, 96)).is_none());
    }

    #[test]
    fn a_window_that_cannot_be_excluded_from_capture_is_not_created() {
        static SEEN: AtomicIsize = AtomicIsize::new(0);
        fn refuse(h: HWND) -> bool {
            SEEN.store(h.0 as isize, Ordering::Relaxed);
            false
        }
        let made = OverlayWindow::create_with(instance(), TEST_CLASS_NAME, r(0, 0, 50, 20), refuse);
        assert!(made.is_err());
        // The window that was created to be tested is gone again.
        let seen = SEEN.load(Ordering::Relaxed);
        assert_ne!(seen, 0, "the affinity step ran on a real window");
        let alive = unsafe {
            windows::Win32::UI::WindowsAndMessaging::IsWindow(Some(HWND(seen as *mut _)))
        };
        assert!(!alive.as_bool(), "no overlay window is left behind");
    }

    #[test]
    fn the_window_is_layered_click_through_and_never_activated() {
        fn accept(_: HWND) -> bool {
            true
        }
        let w = OverlayWindow::create_with(instance(), TEST_CLASS_NAME, r(0, 0, 50, 20), accept)
            .expect("window");
        let ex = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                w.hwnd(),
                windows::Win32::UI::WindowsAndMessaging::GWL_EXSTYLE,
            )
        } as u32;
        for flag in [
            WS_EX_LAYERED,
            WS_EX_TRANSPARENT,
            WS_EX_TOPMOST,
            WS_EX_NOACTIVATE,
            WS_EX_TOOLWINDOW,
        ] {
            assert_ne!(ex & flag.0, 0, "missing {flag:?}");
        }
        let visible = unsafe { windows::Win32::UI::WindowsAndMessaging::IsWindowVisible(w.hwnd()) };
        assert!(!visible.as_bool(), "created hidden");
    }

    /// Live: the real exclusion on a real window. Run with
    /// `cargo test overlay_window_is_excluded_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn overlay_window_is_excluded_live() {
        let w = OverlayWindow::create_for_test(instance(), r(0, 0, 50, 20), false).expect("window");
        assert_eq!(affinity_of(w.hwnd()), Some(WDA_EXCLUDEFROMCAPTURE));
    }

    #[test]
    fn every_overlay_event_maps_to_one_kind_and_others_to_none() {
        assert_eq!(
            kind_for_event(EVENT_OBJECT_LOCATIONCHANGE),
            Some(Kind::Moved)
        );
        assert_eq!(kind_for_event(EVENT_OBJECT_DESTROY), Some(Kind::Destroyed));
        assert_eq!(
            kind_for_event(EVENT_SYSTEM_MINIMIZESTART),
            Some(Kind::Minimized)
        );
        assert_eq!(
            kind_for_event(EVENT_SYSTEM_FOREGROUND),
            Some(Kind::Foreground)
        );
        assert_eq!(kind_for_event(0), None);
        assert_eq!(kind_for_event(0x8002), None);
    }

    /// The hook callbacks only post; the lookups that could time a hook out
    /// stay on the main thread (spec "Where each decision is made").
    #[test]
    fn no_hook_callback_looks_up_windows() {
        let overlay = include_str!("overlay.rs").replace('\r', "");
        let hotkey = include_str!("../hotkey.rs").replace('\r', "");
        let dismiss = include_str!("../dismiss.rs").replace('\r', "");
        let callbacks = [
            ("win_event_proc", &overlay),
            ("hook_proc", &hotkey),
            ("hook_proc", &dismiss),
        ];
        let mut checked = 0;
        for (name, src) in callbacks {
            let start = src
                .find(&format!("unsafe extern \"system\" fn {name}("))
                .unwrap_or_else(|| panic!("{name} not found"));
            let body = &src[start..];
            let end = body.find("\n}\n").expect("end of fn");
            let body = &body[..end];
            for banned in ["GetForegroundWindow", "WindowFromPoint", "GetAncestor"] {
                assert!(!body.contains(banned), "{name} calls {banned}");
            }
            checked += 1;
        }
        assert_eq!(checked, 3, "the scan must look at every callback");
    }

    /// Rule 5: the one timer is set once and killed on first fire and on every
    /// hide path. Nothing in this file arms a second one.
    #[test]
    fn exactly_one_set_timer_and_it_is_killed() {
        let all = include_str!("overlay.rs").replace('\r', "");
        // Only the code above the test module: the scan's own text names it.
        let src = all.split("#[cfg(test)]\nmod tests {").next().unwrap_or("");
        let code: Vec<&str> = src
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect();
        let sets = code.iter().filter(|l| l.contains("SetTimer(")).count();
        let kills = code.iter().filter(|l| l.contains("KillTimer(")).count();
        assert!(sets >= 1, "the candidate set must not be empty");
        assert_eq!(sets, 1, "one SetTimer, for the age cap");
        assert!(kills >= 1, "and a KillTimer for it");
    }

    #[test]
    fn age_is_the_only_interval_and_it_is_the_config_default() {
        assert_eq!(DEFAULT_AGE_MS, 30_000);
    }

    /// Live (Manual check 1's machine half): the same capture path the real
    /// app uses must not contain an overlay window that was excluded from
    /// capture, and must contain one that was not (the control, which shows
    /// the test can see the labels at all). Needs a desktop session and
    /// leaves a mark on screen for about half a second. Run with
    /// `cargo test overlay_is_absent_from_capture_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn overlay_is_absent_from_capture_live() {
        use crate::capture::{grab_raw_for, LookupWindow};
        // The app runs per-monitor-v2; so must the test, or window and
        // capture coordinates disagree on a scaled display.
        unsafe {
            let _ = windows::Win32::UI::HiDpi::SetProcessDpiAwarenessContext(
                windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
            );
        }
        let look = Look::current(96, 1.0);
        let accent = look.tokens.mark_note;
        let (ar, ag, ab) = (
            (accent & 0xFF) as u8,
            ((accent >> 8) & 0xFF) as u8,
            ((accent >> 16) & 0xFF) as u8,
        );
        // The monitor the capture will take, in screen pixels.
        let probe = grab_raw_for("active", u32::MAX, u64::MAX, LookupWindow::None, &[]).unwrap();
        let map = probe.screen.as_ref().expect("a screen capture has a map");
        let monitor = r(
            map.origin_x as i32,
            map.origin_y as i32,
            map.origin_x as i32 + probe.width as i32,
            map.origin_y as i32 + probe.height as i32,
        );
        // A long anchor, so the solid underline is hundreds of pixels.
        let anchor = r(
            monitor.left + 200,
            monitor.top + 300,
            monitor.left + 600,
            monitor.top + 320,
        );
        let frame = render(&[note(anchor, "Overlay capture check")], monitor, &look).unwrap();
        let count_in_capture = |capturable: bool| -> usize {
            let w =
                OverlayWindow::create_for_test(instance(), frame.rect, capturable).expect("window");
            assert!(w.present(&frame));
            std::thread::sleep(std::time::Duration::from_millis(400));
            let shot = grab_raw_for("active", u32::MAX, u64::MAX, LookupWindow::None, &[]).unwrap();
            drop(w);
            let mut n = 0;
            for y in anchor.bottom..(anchor.bottom + 2) {
                for x in anchor.left..anchor.right {
                    let (ix, iy) = ((x - monitor.left) as usize, (y - monitor.top) as usize);
                    let i = (iy * shot.width as usize + ix) * 4;
                    if shot.rgba[i..i + 3] == [ar, ag, ab] {
                        n += 1;
                    }
                }
            }
            n
        };
        let excluded = count_in_capture(false);
        let visible = count_in_capture(true);
        println!("mark pixels in the capture: excluded {excluded}, capturable {visible}");
        assert!(visible > 300, "the control must see the mark: {visible}");
        assert_eq!(excluded, 0, "an excluded overlay is not in the capture");
    }

    /// Live: the real OCR finds a word on a GDI-drawn page and the debug
    /// detector marks it where the word really is. Run with
    /// `cargo test overlay_detects_on_real_ocr_live -- --ignored --nocapture`.
    #[cfg(debug_assertions)]
    #[test]
    #[ignore]
    fn overlay_detects_on_real_ocr_live() {
        use crate::capture::{RawShot, ScreenMap};
        let look = Look::for_theme(Theme::Light, &SysColors::read(), 96, 1.0);
        let (w, h) = (look.px(640), look.px(138));
        let (mut bgra, words) =
            fixture_page(&look, w, h, crate::ui::theme::rgb(255, 255, 255), 0).unwrap();
        for px in bgra.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        // The page sits at (100, 50) on a pretend screen, and is shown to OCR
        // at its own size: scale 1.
        let map = ScreenMap::for_capture(100, 50, w as u32, h as u32, w as u32, h as u32, None);
        let raw = RawShot {
            rgba: bgra,
            width: w as u32,
            height: h as u32,
            screen: Some(map),
        };
        let found = crate::overlay::run::detect_on_shot(
            &raw,
            crate::overlay::detect::MARK_WORD,
            20,
            &|r| {
                crate::actions::extract_text::TextRecognizer::recognize(
                    &crate::actions::extract_text::WindowsOcr,
                    r,
                )
            },
        )
        .unwrap();
        println!("{:?}", found.annotations);
        assert_eq!(found.annotations.len(), 1, "one word marked");
        // "quarterly" is the longest word of the fixture, on line 2.
        let want = words[1][3];
        let got = found.annotations[0].rects[0];
        let near = |a: i32, b: i32| (a - b).abs() <= look.px(6);
        assert!(found.annotations[0].label.ends_with("quarterly"));
        assert!(
            near(got.left, want.left + 100),
            "left {} vs {}",
            got.left,
            want.left + 100
        );
        assert!(
            near(got.top, want.top + 50),
            "top {} vs {}",
            got.top,
            want.top + 50
        );
    }
}
