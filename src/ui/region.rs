//! Region and window capture (#29): a full-virtual-desktop, topmost overlay
//! window that shows a frozen screenshot dimmed, a crosshair cursor, and a
//! live selection rectangle with a pixel-size label. Drag selects a region;
//! click-without-drag on a window selects that window's bounds (DWM
//! extended frame bounds, resolved from a window snapshot taken before the
//! overlay itself existed -- see [`SnapshotEntry`], #271). Esc or
//! right-click cancels; Enter confirms the current rectangle -- a drag or a
//! click only STAGES a
//! rectangle (drawn, not yet returned); Enter is the one thing that turns a
//! staged rectangle into the overlay's result. See [`select_region`] for the
//! public entry point.
//!
//! Two layers, same split as `ui::card` and for the same reason (CLAUDE.md
//! rule 8):
//!
//! - **Pure geometry** (top of this file): [`Rect`] and every function that
//!   normalizes a drag, clamps to the desktop, enforces a minimum size,
//!   resolves which rect a click-on-a-window should offer (including
//!   [`window_at_point`], the pure lookup against a [`SnapshotEntry`]
//!   snapshot), and converts a virtual-desktop-space rect into the
//!   buffer-local [`crate::capture::RectPx`] `capture::crop_rgba` needs.
//!   None of this touches a `windows` type, so it runs against plain Rust
//!   values with no real window, monitor or DPI call involved.
//! - **Win32** ([`Overlay`], [`win32`]): the real window (its own class,
//!   `WM_LBUTTONDOWN`/`WM_MOUSEMOVE`/`WM_LBUTTONUP`/`WM_KEYDOWN`/
//!   `WM_RBUTTONDOWN`/`WM_PAINT` handling) and
//!   `win32::capture_window_snapshot` (`EnumWindows` ->
//!   `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)`, falling back to
//!   `GetWindowRect`, for every visible top-level window, captured ONCE
//!   before the overlay's own window is created) for the window-selection
//!   click path.
//!
//! # Coordinate spaces
//!
//! Three, and every function's doc comment says which one it uses:
//!
//! 1. **Virtual-desktop space** (`Rect` as returned by [`select_region`]'s
//!    final result, and as [`capture::virtual_desktop_rect`] describes it):
//!    physical pixels, `left`/`top` can be negative. Since `App::run` sets
//!    `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` once, process-wide,
//!    before any window exists (see `ui::card`'s module doc for the same
//!    note), Win32 hands this crate un-scaled physical coordinates
//!    regardless of any one monitor's own DPI scale factor -- there is no
//!    second DPI-to-pixel conversion to perform anywhere in this module.
//!    The only "mixed DPI" arithmetic this module actually does is
//!    translating between this space and the next one.
//! 2. **Overlay-local space**: the same physical pixels, with the origin
//!    moved to the overlay window's own top-left corner (i.e.
//!    virtual-desktop coordinates minus the desktop rect's `left`/`top`).
//!    Every Win32 mouse message ([`OverlayInner`]'s handlers) works
//!    entirely in this space, since that is what `WM_MOUSEMOVE` etc.
//!    already hand back in `lParam` -- no conversion needed there either.
//! 3. **Buffer-local space** ([`crate::capture::RectPx`]): non-negative,
//!    what [`crate::capture::crop_rgba`] takes. Numerically identical to
//!    overlay-local space (both have their origin at the desktop rect's own
//!    top-left) -- [`to_buffer_rect`] exists mainly to change the type, and
//!    to be the one function this module's mixed-DPI tests exercise
//!    directly with virtual-desktop-space (space 1) inputs.

use std::ffi::c_void;
use std::sync::{Once, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, BitBlt, CreateCompatibleDC, CreateDIBSection, CreatePen, DeleteDC, DeleteObject,
    EndPaint, GetStockObject, Rectangle, SelectObject, SetBkMode, SetTextColor, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, DT_NOPREFIX, DT_SINGLELINE, HBITMAP, HDC, HGDIOBJ,
    NULL_BRUSH, PAINTSTRUCT, PS_SOLID, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetClientRect, GetMessageW,
    GetWindowLongPtrW, LoadCursorW, RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, ShowWindow, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, HWND_TOPMOST,
    IDC_CROSS, MSG, SWP_NOACTIVATE, SW_SHOW, WM_DESTROY, WM_ERASEBKGND, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_NCCREATE, WM_NCDESTROY, WM_PAINT, WM_RBUTTONDOWN, WNDCLASSEXW,
    WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP,
};

use crate::capture::{self, RawShot, RectPx};

// ---------------------------------------------------------------------------
// Pure geometry
// ---------------------------------------------------------------------------

/// A rectangle in virtual-desktop physical-pixel coordinates (see the module
/// doc comment's "Coordinate spaces", space 1) -- `left`/`top` can be
/// negative. Deliberately a plain Rust struct, not
/// `windows::Win32::Foundation::RECT`: every function below runs against
/// ordinary values with no `windows` dependency at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    pub fn width(&self) -> i32 {
        (self.right - self.left).max(0)
    }

    pub fn height(&self) -> i32 {
        (self.bottom - self.top).max(0)
    }

    pub fn is_empty(&self) -> bool {
        self.width() <= 0 || self.height() <= 0
    }
}

impl From<RECT> for Rect {
    fn from(r: RECT) -> Self {
        Rect {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

impl From<Rect> for RECT {
    fn from(r: Rect) -> Self {
        RECT {
            left: r.left,
            top: r.top,
            right: r.right,
            bottom: r.bottom,
        }
    }
}

/// The minimum drag distance (in either axis) that counts as an intentional
/// region drag rather than a click. 4px matches the conventional Win32 drag
/// threshold (`SM_CXDRAG`/`SM_CYDRAG` default to 4 at 100% scaling); a plain
/// crate constant rather than reading those metrics keeps this decision
/// unit-testable with no live Win32 call involved.
pub const DRAG_THRESHOLD_PX: i32 = 4;

/// The smallest a confirmed selection may be, in either axis -- a 0- or
/// 1-pixel crop is never useful to a caller downstream.
pub const MIN_REGION_SIZE_PX: i32 = 8;

/// Normalizes a drag from `a` to `b` (dragged in any of the four diagonal
/// directions) into a rect with `left <= right` and `top <= bottom`.
pub fn normalize_drag(a: (i32, i32), b: (i32, i32)) -> Rect {
    Rect {
        left: a.0.min(b.0),
        top: a.1.min(b.1),
        right: a.0.max(b.0),
        bottom: a.1.max(b.1),
    }
}

/// Clamps `rect` to lie entirely within `bounds`. A `rect` entirely outside
/// `bounds` clamps to an empty rect at the nearest edge -- callers that need
/// to distinguish that case check [`Rect::is_empty`].
pub fn clamp_to_desktop(rect: Rect, bounds: Rect) -> Rect {
    let left = rect.left.clamp(bounds.left, bounds.right);
    let top = rect.top.clamp(bounds.top, bounds.bottom);
    let right = rect.right.clamp(bounds.left, bounds.right);
    let bottom = rect.bottom.clamp(bounds.top, bounds.bottom);
    Rect {
        left: left.min(right),
        top: top.min(bottom),
        right: left.max(right),
        bottom: top.max(bottom),
    }
}

/// Whether a mouse-down at `start` released at `end` counts as a drag (a
/// region selection) rather than a click (a window selection).
pub fn is_drag(start: (i32, i32), end: (i32, i32), threshold: i32) -> bool {
    (end.0 - start.0).abs() >= threshold || (end.1 - start.1).abs() >= threshold
}

/// Grows `rect` (about its own center) to at least `min` in both
/// dimensions, then re-clamps to `bounds` so growth never pushes it back
/// outside the desktop (a `bounds` narrower than `min` still wins -- this
/// can never select more than the desktop itself has to offer).
pub fn enforce_min_size(rect: Rect, min: i32, bounds: Rect) -> Rect {
    let grow_x = (min - rect.width()).max(0);
    let grow_y = (min - rect.height()).max(0);
    let grown = Rect {
        left: rect.left - grow_x / 2,
        right: rect.right + (grow_x - grow_x / 2),
        top: rect.top - grow_y / 2,
        bottom: rect.bottom + (grow_y - grow_y / 2),
    };
    clamp_to_desktop(grown, bounds)
}

/// Resolves the rect to stage for a click-not-drag release at `point`:
/// prefers `dwm_rect` (the DWM extended frame bounds of the top-level window
/// under the cursor, see [`window_at_point`]), falls back to `fallback_rect`
/// (`GetWindowRect`) when DWM has nothing usable, and finally falls back to
/// a single point-sized rect (grown to [`MIN_REGION_SIZE_PX`] below) when
/// neither is available -- e.g. the point is over the desktop background
/// itself, where neither rect means anything.
pub fn resolve_window_selection(
    dwm_rect: Option<Rect>,
    fallback_rect: Option<Rect>,
    point: (i32, i32),
    bounds: Rect,
) -> Rect {
    let chosen = dwm_rect
        .filter(|r| !r.is_empty())
        .or_else(|| fallback_rect.filter(|r| !r.is_empty()))
        .unwrap_or(Rect {
            left: point.0,
            top: point.1,
            right: point.0,
            bottom: point.1,
        });
    enforce_min_size(clamp_to_desktop(chosen, bounds), MIN_REGION_SIZE_PX, bounds)
}

/// One top-level window captured by [`win32::capture_window_snapshot`], in
/// the Z-order `EnumWindows` already hands back (topmost first).
///
/// **#271:** querying `WindowFromPoint` live, once the overlay is already
/// showing, can only ever return the overlay's OWN `HWND` -- it is itself a
/// real, opaque, `HWND_TOPMOST`, full-virtual-desktop `WS_POPUP` with no
/// `WS_EX_TRANSPARENT` hit-test exemption, so it occupies literally every
/// point a click could land on (see the module doc comment). A snapshot
/// taken once, before the overlay's own window is created, has no such
/// problem -- the overlay cannot be in a list captured before it exists --
/// and it turns "which window is under this click" into the pure, testable
/// [`window_at_point`] below, per CLAUDE.md rule 8 (pure logic is unit
/// tested; Win32 is checked by hand). The alternative (hide the overlay,
/// `WindowFromPoint`, restore) was rejected: it races a repaint (a visible
/// flicker) and stays untestable without a live desktop, where this shape
/// keeps only the actual `EnumWindows`/`GetWindowRect`/
/// `DwmGetWindowAttribute` calls in Win32 territory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotEntry {
    /// The rect this entry is hit-tested against (virtual-desktop space):
    /// the DWM extended frame bounds when available, else `GetWindowRect`
    /// -- the same preference order [`resolve_window_selection`] applies to
    /// the rect it eventually stages.
    hit_rect: Rect,
    dwm_rect: Option<Rect>,
    fallback_rect: Option<Rect>,
}

#[cfg(test)]
impl SnapshotEntry {
    /// Test-only constructor (rule 9: no production seam skipped, just a
    /// plain value builder) -- production code only ever builds these from
    /// [`win32::capture_window_snapshot`]'s real Win32 calls.
    fn for_test(rect: Rect) -> Self {
        SnapshotEntry {
            hit_rect: rect,
            dwm_rect: Some(rect),
            fallback_rect: Some(rect),
        }
    }
}

/// Finds the topmost entry in `snapshot` (ordered topmost-first, exactly as
/// [`win32::capture_window_snapshot`] produces it) whose `hit_rect` contains
/// `point` (virtual-desktop space), and returns the `(dwm_rect,
/// fallback_rect)` pair [`resolve_window_selection`] wants. `None` when
/// nothing in the snapshot covers `point`.
pub fn window_at_point(
    snapshot: &[SnapshotEntry],
    point: (i32, i32),
) -> Option<(Option<Rect>, Option<Rect>)> {
    snapshot
        .iter()
        .find(|entry| rect_contains(entry.hit_rect, point))
        .map(|entry| (entry.dwm_rect, entry.fallback_rect))
}

/// Half-open containment (`[left, right)` x `[top, bottom)`), matching
/// `Rect::width`/`Rect::height`'s own treatment of `right`/`bottom` as
/// exclusive edges.
fn rect_contains(rect: Rect, point: (i32, i32)) -> bool {
    point.0 >= rect.left && point.0 < rect.right && point.1 >= rect.top && point.1 < rect.bottom
}

/// Converts a virtual-desktop-space `rect` (space 1, see the module doc
/// comment) into the non-negative, buffer-local `capture::RectPx` a
/// `RawShot` captured over `desktop` (`capture::grab_virtual_desktop_raw`)
/// can be cropped with (space 3). This is this crate's one "mixed DPI"
/// conversion -- see the module doc comment's "Coordinate spaces" for why
/// there is no separate DPI-to-pixel scaling step anywhere in this module.
pub fn to_buffer_rect(rect: Rect, desktop: Rect) -> RectPx {
    let x = (rect.left - desktop.left).max(0) as u32;
    let y = (rect.top - desktop.top).max(0) as u32;
    let w = rect.width().max(1) as u32;
    let h = rect.height().max(1) as u32;
    RectPx { x, y, w, h }
}

/// The "N x M" pixel-size label the overlay draws next to a live selection.
pub fn size_label(rect: Rect) -> String {
    format!("{} x {}", rect.width(), rect.height())
}

/// Darkens `rgba`'s RGB channels in place by `factor` (0.0 = black, 1.0 =
/// unchanged), leaving alpha untouched. The overlay's "frozen screenshot,
/// dimmed" background is exactly this applied once to the captured
/// virtual-desktop buffer at overlay-open time.
pub fn dim_rgba(rgba: &mut [u8], factor: f32) {
    let factor = factor.clamp(0.0, 1.0);
    for px in rgba.chunks_exact_mut(4) {
        px[0] = (px[0] as f32 * factor).round() as u8;
        px[1] = (px[1] as f32 * factor).round() as u8;
        px[2] = (px[2] as f32 * factor).round() as u8;
    }
}

/// How much the frozen background is dimmed while the overlay is open (0.45
/// keeps enough detail to recognize what is under the cursor while making
/// the live selection rectangle read clearly against it).
const BACKGROUND_DIM_FACTOR: f32 = 0.45;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// What the overlay produced when it closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayOutcome {
    /// A confirmed rectangle (Enter, after a drag or a window click staged
    /// one), already clamped to the desktop and minimum-size-enforced. In
    /// virtual-desktop space.
    Selected(Rect),
    /// Esc or right-click.
    Cancelled,
}

/// Opens the region-selection overlay, blocks (pumping its own message loop)
/// until the user confirms or cancels, and returns the cropped RGBA region
/// on confirm -- `Ok(None)` on cancel, never an error for "the user backed
/// out". An `Err` means the overlay itself could not be set up (desktop
/// capture failed, window creation failed).
///
/// Must run on the main thread (it creates and pumps a real window) -- same
/// requirement `ui::settings::show_modal` documents for the settings
/// window, and the same reason `App::extract_text`/`App::ask` do their
/// Win32-only steps on the main thread before handing off to a worker.
///
/// MEASURED 2026-09-17 (`ui::region::tests::measure_overlay_open_latency`,
/// `cargo test ui::region::tests::measure_overlay_open_latency -- --ignored
/// --nocapture`, unoptimized `dev` profile, this machine's attached
/// monitor(s)): `Overlay::open` (virtual-desktop capture + dim + window
/// creation/show, i.e. everything before the user can start dragging) took
/// 1029.3 ms. Dominated by the multi-monitor `xcap` capture + per-pixel dim
/// pass at native resolution, not window creation itself -- a release build
/// (`opt-level = "z"`, LTO) would very likely be faster, but this number is
/// from a debug build and has not been re-measured in release.
pub fn select_region(instance: HINSTANCE) -> anyhow::Result<Option<RawShot>> {
    let mut overlay = Overlay::open(instance)?;
    let outcome = overlay.run();
    let desktop = overlay.desktop();
    match outcome {
        OverlayOutcome::Cancelled => Ok(None),
        OverlayOutcome::Selected(rect) => {
            let buffer_rect = to_buffer_rect(rect, desktop);
            Ok(Some(overlay.crop_original(buffer_rect)))
        }
    }
}

// ---------------------------------------------------------------------------
// Overlay: the real Win32 window
// ---------------------------------------------------------------------------

const CLASS_NAME: &str = "Wingman.Region.Overlay.4a2f7c31";
/// Rule 9: tests never touch production names.
#[cfg(test)]
const TEST_CLASS_NAME: &str = "Wingman.Region.Overlay.4a2f7c31.Test";

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
    let cursor = LoadCursorW(None, IDC_CROSS).unwrap_or_default();
    let wc = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(wndproc),
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: instance,
        hIcon: Default::default(),
        hCursor: cursor,
        // We paint the entire client area ourselves; no background brush.
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

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut OverlayInner;
    if !ptr.is_null() {
        let inner = &mut *ptr;
        if let Some(result) = inner.handle_message(msg, wparam, lparam) {
            return result;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// One open overlay window. Owns the frozen (undimmed) capture it will crop
/// from on confirm, plus the GDI bitmap/DC pair holding the DIMMED copy it
/// paints as its background -- "the overlay freezes the frame at open so
/// the crop matches what was shown" means the CONTENT matches (the real
/// screen at press-time), not the dimmed rendering the overlay itself draws
/// for contrast.
pub struct Overlay {
    inner: Box<OverlayInner>,
}

impl Overlay {
    /// Production entry: captures and dims the whole virtual desktop, then
    /// creates the real overlay window over it.
    pub fn open(instance: HINSTANCE) -> anyhow::Result<Self> {
        let desktop_rect = capture::virtual_desktop_rect()?;
        let desktop = Rect::from(desktop_rect);
        let original = capture::grab_virtual_desktop_raw()?;
        Self::create(instance, CLASS_NAME, desktop, original)
    }

    /// Same as [`Overlay::open`], but registers (once) and uses a class name
    /// distinct from the production one (rule 9), and takes a synthetic
    /// background instead of capturing the real desktop -- the real-Win32
    /// test's seam: it drives mouse/keyboard messages to a known rectangle,
    /// so it needs a known, small `desktop` rect, not whatever monitors
    /// happen to be attached to the machine running the test.
    #[cfg(test)]
    fn open_for_test(instance: HINSTANCE, desktop: Rect) -> anyhow::Result<Self> {
        let w = desktop.width().max(1) as u32;
        let h = desktop.height().max(1) as u32;
        let original = RawShot {
            rgba: vec![0u8; (w as usize) * (h as usize) * 4],
            width: w,
            height: h,
        };
        Self::create(instance, TEST_CLASS_NAME, desktop, original)
    }

    fn create(
        instance: HINSTANCE,
        class_name: &str,
        desktop: Rect,
        original: RawShot,
    ) -> anyhow::Result<Self> {
        let registered = if class_name == CLASS_NAME {
            ensure_class_registered(instance)
        } else {
            #[cfg(test)]
            {
                ensure_test_class_registered(instance)
            }
            #[cfg(not(test))]
            {
                false
            }
        };
        if !registered {
            anyhow::bail!("Wingman: failed to register the region overlay window class");
        }

        // #271: captured BEFORE `CreateWindowExW` below, so the overlay's
        // own (about-to-exist) window cannot be in it -- see
        // `win32::capture_window_snapshot`'s doc comment.
        let window_snapshot = win32::capture_window_snapshot();

        let mut dimmed = original.rgba.clone();
        dim_rgba(&mut dimmed, BACKGROUND_DIM_FACTOR);
        let (background_bitmap, background_dc) =
            build_background(&dimmed, original.width, original.height);

        let inner = Box::new(OverlayInner {
            hwnd: HWND(std::ptr::null_mut()),
            desktop,
            width: desktop.width(),
            height: desktop.height(),
            original,
            background_bitmap,
            background_dc,
            dragging: false,
            drag_start: (0, 0),
            current_point: (0, 0),
            current_rect: None,
            outcome: None,
            window_snapshot,
        });
        let raw = Box::into_raw(inner);

        let class_name_w = wide_z(class_name);
        let title = wide_z("Wingman region overlay");
        let create_result = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
                PCWSTR(class_name_w.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                desktop.left,
                desktop.top,
                desktop.width().max(1),
                desktop.height().max(1),
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
                    let inner = Box::from_raw(raw);
                    free_background(inner.background_bitmap, inner.background_dc);
                }
                return Err(anyhow::anyhow!(
                    "Wingman: CreateWindowExW (region overlay) failed: {e}"
                ));
            }
        };

        let inner_ref = unsafe { &mut *raw };
        inner_ref.hwnd = hwnd;

        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOACTIVATE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOMOVE
                    | windows::Win32::UI::WindowsAndMessaging::SWP_NOSIZE,
            );
        }

        Ok(Overlay {
            inner: unsafe { Box::from_raw(raw) },
        })
    }

    /// Exposed for tests (which need the raw `HWND` to `PostMessageW`
    /// against and to confirm teardown via `IsWindow`); `select_region`
    /// itself never needs it.
    #[allow(dead_code)]
    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd
    }

    pub fn desktop(&self) -> Rect {
        self.inner.desktop
    }

    /// Blocking, real message loop: pumps this window's own messages until
    /// [`OverlayInner::handle_message`] records an outcome. Filtered to
    /// exactly this `HWND` (unlike `ui::settings`'s modal loop, the overlay
    /// has no child controls to also route input to).
    pub fn run(&mut self) -> OverlayOutcome {
        loop {
            let mut msg = MSG::default();
            let ret = unsafe { GetMessageW(&mut msg, Some(self.inner.hwnd), 0, 0) };
            if ret.0 <= 0 {
                // WM_QUIT or an error -- treat as a cancel rather than
                // hanging forever (CLAUDE.md rule 7: every path ends in a
                // card, never a silent hang).
                return OverlayOutcome::Cancelled;
            }
            unsafe {
                DispatchMessageW(&msg);
            }
            if let Some(outcome) = self.inner.outcome {
                return outcome;
            }
        }
    }

    /// Crops `rect` (buffer-local, space 3) out of the frozen, UNDIMMED
    /// capture this overlay was opened with.
    fn crop_original(&self, rect: RectPx) -> RawShot {
        capture::crop_rgba(
            &self.inner.original.rgba,
            self.inner.original.width,
            self.inner.original.height,
            rect,
        )
    }

    /// Test-only seam: feeds a synthetic message directly to the window's
    /// own handler, the same seam `ui::card::Card::handle_message` exposes.
    /// Unused by this module's own tests today (they drive the overlay
    /// through real posted messages instead, to exercise the `wndproc` ->
    /// `GWLP_USERDATA` routing too -- see `overlay_drag_then_enter_selects_
    /// exactly_the_dragged_rectangle`'s doc comment), kept as the seam a
    /// future test can reach for without going through a real message pump.
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
    pub(crate) fn outcome(&self) -> Option<OverlayOutcome> {
        self.inner.outcome
    }

    /// Test-only seam: the staged (not-yet-confirmed) rect, overlay-local
    /// space -- lets #271's regression test inspect what a click-not-drag
    /// resolved to without needing Enter/an `OverlayOutcome`.
    #[cfg(test)]
    pub(crate) fn current_rect(&self) -> Option<Rect> {
        self.inner.current_rect
    }

    /// Test-only seam: replaces the real, `EnumWindows`-captured snapshot
    /// with a synthetic one, so #271's regression test can assert the
    /// click-not-drag path resolves to a KNOWN rect instead of whatever
    /// real windows happen to be on the machine running the test.
    #[cfg(test)]
    pub(crate) fn set_window_snapshot(&mut self, snapshot: Vec<SnapshotEntry>) {
        self.inner.window_snapshot = snapshot;
    }
}

impl Drop for Overlay {
    fn drop(&mut self) {
        unsafe {
            if !self.inner.hwnd.0.is_null() {
                let _ = DestroyWindow(self.inner.hwnd);
            }
        }
        free_background(self.inner.background_bitmap, self.inner.background_dc);
        // Guard against a double-free if Drop somehow runs twice (it never
        // should, but HBITMAP/HDC's null check inside free_background makes
        // a repeat call harmless either way).
        self.inner.background_bitmap = HBITMAP(std::ptr::null_mut());
        self.inner.background_dc = HDC(std::ptr::null_mut());
    }
}

struct OverlayInner {
    hwnd: HWND,
    /// This overlay's coverage, in virtual-desktop space (space 1). Also
    /// the window's own screen position/size, so overlay-local coordinates
    /// (space 2, what mouse messages arrive in) need no scaling to convert
    /// to/from this -- only an origin shift, done in [`Self::to_desktop`].
    desktop: Rect,
    width: i32,
    height: i32,
    /// The frozen, UNDIMMED capture -- what the eventual crop reads from.
    original: RawShot,
    /// Owned GDI bitmap holding the DIMMED copy, selected into
    /// `background_dc`; painted with `BitBlt` on every `WM_PAINT`.
    background_bitmap: HBITMAP,
    background_dc: HDC,
    dragging: bool,
    /// Overlay-local (space 2).
    drag_start: (i32, i32),
    /// Overlay-local (space 2).
    current_point: (i32, i32),
    /// The staged selection (overlay-local, space 2) -- set by finishing a
    /// drag or resolving a window click; Enter turns this into `outcome`.
    current_rect: Option<Rect>,
    outcome: Option<OverlayOutcome>,
    /// #271: the window list [`win32::capture_window_snapshot`] captured
    /// once, before this overlay's own window was created. Consulted by
    /// [`Self::on_lbuttonup`]'s click-not-drag path instead of a live
    /// `WindowFromPoint` call (which, once the overlay is showing, could
    /// only ever find the overlay itself).
    window_snapshot: Vec<SnapshotEntry>,
}

impl OverlayInner {
    fn local_bounds(&self) -> Rect {
        Rect {
            left: 0,
            top: 0,
            right: self.width,
            bottom: self.height,
        }
    }

    fn to_desktop(&self, r: Rect) -> Rect {
        Rect {
            left: r.left + self.desktop.left,
            top: r.top + self.desktop.top,
            right: r.right + self.desktop.left,
            bottom: r.bottom + self.desktop.top,
        }
    }

    fn to_local(&self, r: Rect) -> Rect {
        Rect {
            left: r.left - self.desktop.left,
            top: r.top - self.desktop.top,
            right: r.right - self.desktop.left,
            bottom: r.bottom - self.desktop.top,
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    fn on_lbuttondown(&mut self, x: i32, y: i32) {
        self.dragging = true;
        self.drag_start = (x, y);
        self.current_point = (x, y);
        self.current_rect = None;
        self.invalidate();
    }

    fn on_mousemove(&mut self, x: i32, y: i32) {
        self.current_point = (x, y);
        if self.dragging {
            self.invalidate();
        }
    }

    fn on_lbuttonup(&mut self, x: i32, y: i32) {
        if !self.dragging {
            return;
        }
        self.dragging = false;
        let start = self.drag_start;
        let end = (x, y);
        let bounds = self.local_bounds();

        let rect = if is_drag(start, end, DRAG_THRESHOLD_PX) {
            enforce_min_size(
                clamp_to_desktop(normalize_drag(start, end), bounds),
                MIN_REGION_SIZE_PX,
                bounds,
            )
        } else {
            let screen_pt = (self.desktop.left + end.0, self.desktop.top + end.1);
            let (dwm_rect, fallback_rect) =
                window_at_point(&self.window_snapshot, screen_pt).unwrap_or((None, None));
            resolve_window_selection(
                dwm_rect.map(|r| self.to_local(r)),
                fallback_rect.map(|r| self.to_local(r)),
                end,
                bounds,
            )
        };

        self.current_rect = Some(rect);
        self.invalidate();
    }

    fn on_keydown(&mut self, vk: u32) {
        if vk == VK_RETURN.0 as u32 {
            if let Some(rect) = self.current_rect {
                self.outcome = Some(OverlayOutcome::Selected(self.to_desktop(rect)));
            }
        } else if vk == VK_ESCAPE.0 as u32 {
            self.outcome = Some(OverlayOutcome::Cancelled);
        }
    }

    fn on_rbuttondown(&mut self) {
        self.outcome = Some(OverlayOutcome::Cancelled);
    }

    fn visible_rect(&self) -> Option<Rect> {
        if self.dragging {
            Some(normalize_drag(self.drag_start, self.current_point))
        } else {
            self.current_rect
        }
    }

    fn on_paint(&self) {
        unsafe {
            let mut ps = PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;

            if !hdc.0.is_null() && w > 0 && h > 0 && !self.background_dc.0.is_null() {
                let _ = BitBlt(hdc, 0, 0, w, h, Some(self.background_dc), 0, 0, SRCCOPY);
            }
            if let Some(rect) = self.visible_rect() {
                draw_selection(hdc, rect);
            }

            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    /// The overlay's own window proc dispatches internally; this is the
    /// seam tests use to feed synthetic (or, in the real-Win32 test,
    /// genuinely dispatched) messages directly.
    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                self.on_paint();
                Some(LRESULT(0))
            }
            WM_ERASEBKGND => Some(LRESULT(1)), // we paint the whole client area ourselves
            WM_LBUTTONDOWN => {
                let (x, y) = point_from_lparam(lparam);
                self.on_lbuttondown(x, y);
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let (x, y) = point_from_lparam(lparam);
                self.on_mousemove(x, y);
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = point_from_lparam(lparam);
                self.on_lbuttonup(x, y);
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                self.on_rbuttondown();
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                self.on_keydown(wparam.0 as u32);
                Some(LRESULT(0))
            }
            WM_DESTROY | WM_NCDESTROY => Some(LRESULT(0)),
            _ => None,
        }
    }
}

fn point_from_lparam(lparam: LPARAM) -> (i32, i32) {
    let raw = lparam.0 as u32;
    let x = (raw & 0xFFFF) as u16 as i16 as i32;
    let y = ((raw >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

unsafe fn draw_selection(hdc: HDC, rect: Rect) {
    let pen = CreatePen(PS_SOLID, 2, COLORREF(0x0000FFFF)); // bright yellow, BGR-packed
    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
    let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));
    let _ = Rectangle(hdc, rect.left, rect.top, rect.right, rect.bottom);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(HGDIOBJ(pen.0));

    // `size_label` always formats "<width> x <height>" (see its own doc
    // comment/test), so this can never actually be empty -- still routed
    // through the shared guard (issue #221) rather than a raw `DrawTextW`
    // call, so a future change to `size_label` can't silently reintroduce
    // the empty-buffer crash MEASURED on the palette branch (commit
    // `453fe0b`).
    let label = size_label(rect);
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(0x00FFFFFF));
    let text_rc = RECT {
        left: rect.left + 4,
        top: (rect.top - 20).max(0),
        right: rect.left + 240,
        bottom: rect.top.max(20),
    };
    crate::ui::text::draw_text_line(hdc, &label, text_rc, DT_SINGLELINE | DT_NOPREFIX);
}

/// Builds the top-down BGRA DIB section (and the memory DC it is selected
/// into) `on_paint` `BitBlt`s from on every repaint. `rgba` is TOP-DOWN
/// row-major RGBA8 (a dimmed copy of a `RawShot`'s own layout); GDI's own
/// convention for 32bpp `BI_RGB` is BGRA (see `executors::image_clipboard`'s
/// `encode_dib` for the other place this crate does the same channel swap,
/// for the opposite -- bottom-up -- clipboard convention). Returns null
/// handles on any GDI failure; `on_paint` already checks for a null
/// `background_dc` before using it, so this degrades to "no background
/// drawn, selection rectangle still works" rather than a panic.
fn build_background(rgba: &[u8], width: u32, height: u32) -> (HBITMAP, HDC) {
    unsafe {
        let mem_dc = CreateCompatibleDC(None);
        if mem_dc.0.is_null() {
            return (HBITMAP(std::ptr::null_mut()), HDC(std::ptr::null_mut()));
        }

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // negative: top-down, matches `rgba`'s own layout
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits_ptr: *mut c_void = std::ptr::null_mut();
        let bmp = match CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0)
        {
            Ok(h) => h,
            Err(_) => {
                let _ = DeleteDC(mem_dc);
                return (HBITMAP(std::ptr::null_mut()), HDC(std::ptr::null_mut()));
            }
        };

        let px_count = (width as usize) * (height as usize);
        if !bits_ptr.is_null() && px_count > 0 {
            let dst = std::slice::from_raw_parts_mut(bits_ptr as *mut u8, px_count * 4);
            for (s, d) in rgba.chunks_exact(4).zip(dst.chunks_exact_mut(4)) {
                d[0] = s[2]; // B
                d[1] = s[1]; // G
                d[2] = s[0]; // R
                d[3] = s[3]; // A
            }
        }

        SelectObject(mem_dc, HGDIOBJ(bmp.0));
        (bmp, mem_dc)
    }
}

fn free_background(bitmap: HBITMAP, dc: HDC) {
    unsafe {
        if !bitmap.0.is_null() {
            let _ = DeleteObject(HGDIOBJ(bitmap.0));
        }
        if !dc.0.is_null() {
            let _ = DeleteDC(dc);
        }
    }
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

// ---------------------------------------------------------------------------
// Win32: window snapshot for the click-select-window path (#271)
// ---------------------------------------------------------------------------

mod win32 {
    use super::{Rect, SnapshotEntry};
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, IsIconic, IsWindowVisible,
    };

    /// Captures every visible, non-minimized top-level window's bounds, in
    /// Z-order (topmost first -- `EnumWindows` already enumerates that way),
    /// at the instant this is called. Real Win32, not unit-tested directly
    /// (CLAUDE.md rule 8) -- [`super::window_at_point`] is the pure decision
    /// this feeds, and IS unit-tested.
    ///
    /// **#271's fix**: [`super::Overlay::create`] calls this BEFORE
    /// `CreateWindowExW` runs for the overlay itself, so the overlay's own
    /// `HWND` cannot be in the snapshot -- there is no live-`WindowFromPoint`
    /// race with the overlay's own opaque, topmost, full-desktop window to
    /// guard against, because the snapshot is deterministic and taken
    /// before that window exists.
    ///
    /// **Manual check** (filed to #166): open the overlay over two
    /// overlapping real windows (e.g. Notepad in front of File Explorer)
    /// and confirm a click-without-drag on the visible (topmost) one stages
    /// ITS bounds -- watch the "W x H" label before pressing Enter -- not
    /// the full desktop rect and not the window it is covering. Also check
    /// a click on bare desktop background: `THEORY (unverified)`: the
    /// shell's own desktop window (`Progman`/`WorkerW`) may appear in this
    /// snapshot with a real, full-monitor `GetWindowRect`, which would
    /// stage that whole rect rather than falling back to a point-sized one
    /// -- this was equally possible in the pre-#271 code once corrected for
    /// the overlay-always-wins bug, so it is not a regression this fix
    /// introduces, but it has not been observed on a live desktop.
    pub(super) fn capture_window_snapshot() -> Vec<SnapshotEntry> {
        let mut entries: Vec<SnapshotEntry> = Vec::new();
        unsafe {
            let _ = EnumWindows(
                Some(enum_proc),
                LPARAM(&mut entries as *mut Vec<SnapshotEntry> as isize),
            );
        }
        entries
    }

    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let entries = unsafe { &mut *(lparam.0 as *mut Vec<SnapshotEntry>) };
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        let minimized = unsafe { IsIconic(hwnd) }.as_bool();
        if !visible || minimized {
            return BOOL(1); // continue enumerating
        }

        let fallback_rect = {
            let mut rc = RECT::default();
            if unsafe { GetWindowRect(hwnd, &mut rc) }.is_ok() {
                Some(Rect::from(rc))
            } else {
                None
            }
        };

        let dwm_rect = {
            let mut rc = RECT::default();
            let ok = unsafe {
                DwmGetWindowAttribute(
                    hwnd,
                    DWMWA_EXTENDED_FRAME_BOUNDS,
                    &mut rc as *mut _ as *mut core::ffi::c_void,
                    std::mem::size_of::<RECT>() as u32,
                )
            };
            if ok.is_ok() {
                Some(Rect::from(rc))
            } else {
                None
            }
        };

        if let Some(hit_rect) = dwm_rect.filter(|r| !r.is_empty()).or(fallback_rect) {
            entries.push(SnapshotEntry {
                hit_rect,
                dwm_rect,
                fallback_rect,
            });
        }

        BOOL(1) // continue enumerating
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- normalize_drag ----------------------------------------------------

    #[test]
    fn normalize_drag_handles_every_diagonal_direction() {
        let expected = Rect {
            left: 10,
            top: 20,
            right: 110,
            bottom: 220,
        };
        assert_eq!(normalize_drag((10, 20), (110, 220)), expected); // top-left to bottom-right
        assert_eq!(normalize_drag((110, 220), (10, 20)), expected); // bottom-right to top-left
        assert_eq!(normalize_drag((10, 220), (110, 20)), expected); // bottom-left to top-right
        assert_eq!(normalize_drag((110, 20), (10, 220)), expected); // top-right to bottom-left
    }

    #[test]
    fn normalize_drag_of_a_single_point_is_empty() {
        let r = normalize_drag((5, 5), (5, 5));
        assert!(r.is_empty());
    }

    // -- clamp_to_desktop ----------------------------------------------------

    #[test]
    fn clamp_to_desktop_leaves_a_fully_contained_rect_unchanged() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 1000,
        };
        let r = Rect {
            left: 10,
            top: 10,
            right: 100,
            bottom: 100,
        };
        assert_eq!(clamp_to_desktop(r, bounds), r);
    }

    #[test]
    fn clamp_to_desktop_clips_an_overhanging_rect() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let r = Rect {
            left: -50,
            top: -50,
            right: 150,
            bottom: 150,
        };
        assert_eq!(
            clamp_to_desktop(r, bounds),
            Rect {
                left: 0,
                top: 0,
                right: 100,
                bottom: 100
            }
        );
    }

    #[test]
    fn clamp_to_desktop_rect_entirely_outside_bounds_becomes_empty() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let r = Rect {
            left: 500,
            top: 500,
            right: 600,
            bottom: 600,
        };
        let clamped = clamp_to_desktop(r, bounds);
        assert!(clamped.is_empty());
    }

    // -- is_drag -------------------------------------------------------------

    #[test]
    fn is_drag_true_when_either_axis_exceeds_threshold() {
        assert!(is_drag((0, 0), (10, 0), 4));
        assert!(is_drag((0, 0), (0, 10), 4));
        assert!(is_drag((0, 0), (-10, 0), 4));
    }

    #[test]
    fn is_drag_false_for_a_tiny_jitter() {
        assert!(!is_drag((100, 100), (101, 102), 4));
    }

    #[test]
    fn is_drag_exactly_at_threshold_counts_as_a_drag() {
        assert!(is_drag((0, 0), (4, 0), 4));
    }

    // -- enforce_min_size ------------------------------------------------

    #[test]
    fn enforce_min_size_leaves_an_already_large_rect_unchanged() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 1000,
        };
        let r = Rect {
            left: 10,
            top: 10,
            right: 110,
            bottom: 110,
        };
        assert_eq!(enforce_min_size(r, 8, bounds), r);
    }

    #[test]
    fn enforce_min_size_grows_a_tiny_rect_about_its_center() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 1000,
            bottom: 1000,
        };
        let r = Rect {
            left: 100,
            top: 100,
            right: 101,
            bottom: 101,
        }; // 1x1
        let grown = enforce_min_size(r, 8, bounds);
        assert!(grown.width() >= 8);
        assert!(grown.height() >= 8);
        // Still centered on roughly the same point.
        let cx = (grown.left + grown.right) / 2;
        assert!((cx - 100).abs() <= 1);
    }

    #[test]
    fn enforce_min_size_never_escapes_bounds() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 20,
            bottom: 20,
        };
        let r = Rect {
            left: 1,
            top: 1,
            right: 2,
            bottom: 2,
        };
        let grown = enforce_min_size(r, 8, bounds);
        assert!(grown.left >= bounds.left);
        assert!(grown.top >= bounds.top);
        assert!(grown.right <= bounds.right);
        assert!(grown.bottom <= bounds.bottom);
    }

    // -- resolve_window_selection ------------------------------------------

    #[test]
    fn resolve_window_selection_prefers_dwm_rect() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 2000,
        };
        let dwm = Rect {
            left: 100,
            top: 100,
            right: 400,
            bottom: 300,
        };
        let fallback = Rect {
            left: 90,
            top: 90,
            right: 410,
            bottom: 310,
        };
        let chosen = resolve_window_selection(Some(dwm), Some(fallback), (200, 200), bounds);
        assert_eq!(chosen, dwm);
    }

    #[test]
    fn resolve_window_selection_falls_back_when_dwm_is_empty() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 2000,
        };
        let empty_dwm = Rect {
            left: 100,
            top: 100,
            right: 100,
            bottom: 100,
        };
        let fallback = Rect {
            left: 90,
            top: 90,
            right: 410,
            bottom: 310,
        };
        let chosen = resolve_window_selection(Some(empty_dwm), Some(fallback), (200, 200), bounds);
        assert_eq!(chosen, fallback);
    }

    #[test]
    fn resolve_window_selection_falls_back_to_point_when_nothing_is_available() {
        let bounds = Rect {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 2000,
        };
        let chosen = resolve_window_selection(None, None, (500, 500), bounds);
        assert!(chosen.width() >= MIN_REGION_SIZE_PX);
        assert!(chosen.height() >= MIN_REGION_SIZE_PX);
        let cx = (chosen.left + chosen.right) / 2;
        let cy = (chosen.top + chosen.bottom) / 2;
        assert!((cx - 500).abs() <= 1);
        assert!((cy - 500).abs() <= 1);
    }

    // -- window_at_point: #271's pure snapshot lookup -----------------------

    #[test]
    fn window_at_point_returns_none_for_an_empty_snapshot() {
        assert_eq!(window_at_point(&[], (10, 10)), None);
    }

    #[test]
    fn window_at_point_returns_none_when_point_is_outside_every_entry() {
        let snapshot = vec![SnapshotEntry::for_test(Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        })];
        assert_eq!(window_at_point(&snapshot, (200, 200)), None);
    }

    #[test]
    fn window_at_point_finds_the_single_covering_entry() {
        let rect = Rect {
            left: 10,
            top: 10,
            right: 110,
            bottom: 210,
        };
        let snapshot = vec![SnapshotEntry::for_test(rect)];
        assert_eq!(
            window_at_point(&snapshot, (50, 50)),
            Some((Some(rect), Some(rect)))
        );
    }

    #[test]
    fn window_at_point_prefers_the_topmost_of_two_overlapping_entries() {
        // Both entries cover (50, 50); `topmost` is listed FIRST, matching
        // the order `win32::capture_window_snapshot` produces (EnumWindows
        // already enumerates Z-order top-to-bottom). This is the exact
        // shape #271 needs: a click over two overlapping real windows must
        // resolve to the visible (topmost) one, never whichever happens to
        // be underneath.
        let topmost = Rect {
            left: 0,
            top: 0,
            right: 60,
            bottom: 60,
        };
        let behind = Rect {
            left: 0,
            top: 0,
            right: 2000,
            bottom: 2000,
        };
        let snapshot = vec![
            SnapshotEntry::for_test(topmost),
            SnapshotEntry::for_test(behind),
        ];
        let (dwm, fallback) = window_at_point(&snapshot, (50, 50)).expect("a covering entry");
        assert_eq!(dwm, Some(topmost));
        assert_eq!(fallback, Some(topmost));
    }

    #[test]
    fn window_at_point_containment_is_half_open_on_the_right_and_bottom_edges() {
        let rect = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let snapshot = vec![SnapshotEntry::for_test(rect)];
        // Left/top edges are inside.
        assert!(window_at_point(&snapshot, (0, 0)).is_some());
        // Right/bottom edges are exclusive, matching `Rect::width`/`height`
        // already treating them that way.
        assert_eq!(window_at_point(&snapshot, (100, 50)), None);
        assert_eq!(window_at_point(&snapshot, (50, 100)), None);
    }

    // -- to_buffer_rect: the crate's one "mixed DPI" conversion -------------

    #[test]
    fn to_buffer_rect_localizes_a_positive_origin_desktop() {
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        };
        let rect = Rect {
            left: 100,
            top: 200,
            right: 300,
            bottom: 400,
        };
        let buf = to_buffer_rect(rect, desktop);
        assert_eq!(
            buf,
            RectPx {
                x: 100,
                y: 200,
                w: 200,
                h: 200
            }
        );
    }

    #[test]
    fn to_buffer_rect_handles_a_secondary_monitor_left_of_primary_at_a_different_dpi() {
        // A realistic mixed-DPI layout: primary monitor 1920x1080 at 100%
        // starting at (0,0); a secondary monitor physically 2560px wide at
        // 150% scaling placed to its LEFT, so its physical-pixel rect (per
        // GetMonitorInfoW, the space this crate already works in -- see
        // capture.rs's own module doc) is negative: (-2560, -140) to
        // (0, 940) for a 2560x1080-physical secondary at a slightly
        // different physical height due to scaling. The desktop's bounding
        // rect is therefore (-2560, -140) to (1920, 1080).
        let desktop = Rect {
            left: -2560,
            top: -140,
            right: 1920,
            bottom: 1080,
        };
        // A selection entirely on the secondary (negative-origin) monitor.
        let rect = Rect {
            left: -2000,
            top: -100,
            right: -1500,
            bottom: 200,
        };
        let buf = to_buffer_rect(rect, desktop);
        assert_eq!(
            buf,
            RectPx {
                x: (-2000 - -2560) as u32, // 560
                y: (-100 - -140) as u32,   // 40
                w: 500,
                h: 300,
            }
        );
    }

    #[test]
    fn to_buffer_rect_never_produces_a_zero_size_rect() {
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 100,
            bottom: 100,
        };
        let rect = Rect {
            left: 10,
            top: 10,
            right: 10,
            bottom: 10,
        }; // degenerate
        let buf = to_buffer_rect(rect, desktop);
        assert!(buf.w >= 1);
        assert!(buf.h >= 1);
    }

    // -- size_label / dim_rgba ------------------------------------------

    #[test]
    fn size_label_formats_width_x_height() {
        let r = Rect {
            left: 0,
            top: 0,
            right: 1024,
            bottom: 768,
        };
        assert_eq!(size_label(r), "1024 x 768");
    }

    #[test]
    fn dim_rgba_scales_rgb_and_leaves_alpha_untouched() {
        let mut buf = vec![200u8, 100, 50, 255];
        dim_rgba(&mut buf, 0.5);
        assert_eq!(buf, vec![100, 50, 25, 255]);
    }

    #[test]
    fn dim_rgba_clamps_factor_above_one() {
        let mut buf = vec![10u8, 20, 30, 255];
        dim_rgba(&mut buf, 2.0);
        assert_eq!(buf, vec![10, 20, 30, 255]);
    }

    // -- point_from_lparam ---------------------------------------------------

    #[test]
    fn point_from_lparam_decodes_low_high_words() {
        let lparam = LPARAM(((300i32 << 16) | (150i32 & 0xFFFF)) as isize);
        assert_eq!(point_from_lparam(lparam), (150, 300));
    }

    // -- real Win32: overlay creation, message-driven selection, teardown --

    fn test_instance() -> HINSTANCE {
        let h = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
            .expect("GetModuleHandleW");
        HINSTANCE(h.0)
    }

    fn make_lparam(x: i32, y: i32) -> LPARAM {
        let xw = (x as i16) as u16 as u32;
        let yw = (y as i16) as u16 as u32;
        LPARAM(((yw << 16) | xw) as isize)
    }

    /// Drains whatever is currently queued for `hwnd`, dispatching each
    /// message through the real window proc (so `WM_PAINT` -- synthesized
    /// as a side effect of the `InvalidateRect` calls our handlers make --
    /// is always properly `BeginPaint`/`EndPaint`'d, never left pending;
    /// see `ui::card`'s own test module for why a check that only
    /// `PeekMessageW`s without dispatching cannot loop safely here).
    /// Bounded by `max_iterations` as a safety net; stops early once `done`
    /// returns true.
    fn pump_until(hwnd: HWND, max_iterations: u32, mut done: impl FnMut() -> bool) {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, PM_REMOVE};
        for _ in 0..max_iterations {
            if done() {
                return;
            }
            let mut msg = MSG::default();
            let has_msg = unsafe { PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE) }.as_bool();
            if !has_msg {
                return;
            }
            unsafe {
                DispatchMessageW(&msg);
            }
        }
    }

    #[test]
    fn overlay_drag_then_enter_selects_exactly_the_dragged_rectangle() {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, PostMessageW};

        let instance = test_instance();
        // Origin deliberately non-zero (and negative), to exercise the same
        // desktop-space-to-overlay-local translation a real secondary
        // monitor would need -- see `to_buffer_rect`'s mixed-DPI test above
        // for the pure version of this same arithmetic.
        let desktop = Rect {
            left: -100,
            top: -50,
            right: 700,
            bottom: 550,
        }; // 800x600
        let overlay = Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();
        assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());

        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), make_lparam(50, 60));
            let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), make_lparam(250, 260));
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), make_lparam(250, 260));
            let _ = PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                WPARAM(VK_RETURN.0 as usize),
                LPARAM(0),
            );
        }

        pump_until(hwnd, 50, || overlay.outcome().is_some());

        let outcome = overlay
            .outcome()
            .expect("Enter after a drag must produce an outcome");
        let expected = OverlayOutcome::Selected(Rect {
            left: -100 + 50,
            top: -50 + 60,
            right: -100 + 250,
            bottom: -50 + 260,
        });
        assert_eq!(outcome, expected);

        drop(overlay);
        assert!(
            !unsafe { IsWindow(Some(hwnd)) }.as_bool(),
            "Overlay::drop must destroy the window"
        );
    }

    #[test]
    fn overlay_esc_cancels_without_a_staged_selection() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300,
        };
        let overlay = Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();

        unsafe {
            let _ = PostMessageW(
                Some(hwnd),
                WM_KEYDOWN,
                WPARAM(VK_ESCAPE.0 as usize),
                LPARAM(0),
            );
        }
        pump_until(hwnd, 20, || overlay.outcome().is_some());

        assert_eq!(overlay.outcome(), Some(OverlayOutcome::Cancelled));
    }

    #[test]
    fn overlay_right_click_cancels_mid_drag() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300,
        };
        let overlay = Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();

        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), make_lparam(10, 10));
            let _ = PostMessageW(Some(hwnd), WM_MOUSEMOVE, WPARAM(0), make_lparam(50, 50));
            let _ = PostMessageW(Some(hwnd), WM_RBUTTONDOWN, WPARAM(0), LPARAM(0));
        }
        pump_until(hwnd, 20, || overlay.outcome().is_some());

        assert_eq!(overlay.outcome(), Some(OverlayOutcome::Cancelled));
    }

    #[test]
    fn overlay_click_without_drag_stages_a_rect_but_enter_is_still_required() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300,
        };
        let overlay = Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();

        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), make_lparam(10, 10));
            // Released 1px away: below DRAG_THRESHOLD_PX, so this is a
            // click, not a drag -- the window-selection path runs (against
            // the real `EnumWindows`-captured snapshot, whatever that
            // resolves to on the machine running this test), staging SOME
            // rect but never confirming without Enter.
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), make_lparam(11, 10));
        }
        pump_until(hwnd, 20, || overlay.outcome().is_some());

        assert_eq!(
            overlay.outcome(),
            None,
            "a click alone must stage a rectangle, not confirm one"
        );
    }

    // -- #271: click-without-drag must resolve to the window UNDER the ----
    // -- click, never the overlay's own full-desktop bounds ---------------

    #[test]
    fn overlay_click_without_drag_stages_the_snapshot_window_not_the_whole_overlay() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300,
        };
        let mut overlay =
            Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();

        // A synthetic "real window" entirely inside the overlay's local
        // bounds, in DESKTOP space (here identical to local space since
        // `desktop` starts at (0, 0)). Before #271's fix, the click path
        // called `WindowFromPoint` live while the overlay itself covers
        // every point on screen, so it could only ever resolve to the
        // overlay's own bounds (0,0)-(400,300) -- never this rect.
        let window_rect = Rect {
            left: 20,
            top: 20,
            right: 120,
            bottom: 90,
        };
        overlay.set_window_snapshot(vec![SnapshotEntry::for_test(window_rect)]);

        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), make_lparam(50, 50));
            // 1px away: a click, not a drag.
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), make_lparam(51, 50));
        }
        pump_until(hwnd, 20, || overlay.current_rect().is_some());

        assert_eq!(
            overlay.current_rect(),
            Some(window_rect),
            "a click over a window in the snapshot must stage THAT window's \
             bounds, not the overlay's own full-desktop rect"
        );
    }

    #[test]
    fn overlay_click_without_drag_with_nothing_in_the_snapshot_stages_a_point_sized_rect() {
        use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 400,
            bottom: 300,
        };
        let mut overlay =
            Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        let hwnd = overlay.hwnd();

        // Nothing in the snapshot covers (200, 150) -- the empty-desktop
        // case; must fall back to a small, point-sized rect (grown to
        // MIN_REGION_SIZE_PX by `resolve_window_selection`), never the
        // overlay's own full-desktop bounds.
        overlay.set_window_snapshot(vec![]);

        unsafe {
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONDOWN, WPARAM(0), make_lparam(200, 150));
            let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), make_lparam(201, 150));
        }
        pump_until(hwnd, 20, || overlay.current_rect().is_some());

        let rect = overlay.current_rect().expect("a click must stage a rect");
        assert!(rect.width() <= MIN_REGION_SIZE_PX * 2);
        assert!(rect.height() <= MIN_REGION_SIZE_PX * 2);
        assert_ne!(
            rect,
            Rect {
                left: 0,
                top: 0,
                right: 400,
                bottom: 300
            },
            "must never fall back to the overlay's own full-desktop bounds"
        );
    }

    // -- crop_original: the overlay crops the UNDIMMED frozen frame -------

    #[test]
    fn crop_original_reads_from_the_undimmed_capture_not_the_dimmed_background() {
        let instance = test_instance();
        let desktop = Rect {
            left: 0,
            top: 0,
            right: 4,
            bottom: 4,
        };
        let mut overlay =
            Overlay::open_for_test(instance, desktop).expect("Overlay::open_for_test");
        // open_for_test seeds an all-zero buffer; overwrite one pixel in
        // the ORIGINAL directly (bypassing dim_rgba entirely) so this test
        // does not depend on open_for_test's synthetic content.
        overlay.inner.original.rgba[0..4].copy_from_slice(&[10, 20, 30, 255]);

        let cropped = overlay.crop_original(RectPx {
            x: 0,
            y: 0,
            w: 1,
            h: 1,
        });
        assert_eq!(cropped.rgba, vec![10, 20, 30, 255]);
    }

    // -- overlay-open latency -----------------------------------------------

    #[test]
    #[ignore = "manual: opens a real overlay window covering every attached \
                monitor; run with `cargo test ui::region::tests::measure_overlay_open_latency \
                -- --ignored --nocapture`"]
    fn measure_overlay_open_latency() {
        let instance = test_instance();
        let start = std::time::Instant::now();
        match Overlay::open(instance) {
            Ok(overlay) => {
                let elapsed = start.elapsed();
                println!(
                    "MEASURED 2026-09-17: Overlay::open latency: {:.1} ms",
                    elapsed.as_secs_f64() * 1000.0
                );
                drop(overlay);
            }
            Err(e) => {
                println!("Overlay::open failed in this environment: {e:#}");
            }
        }
    }

    // -- issue #221: DrawTextW must not crash on empty text -----------------
    //
    // `draw_selection`'s own text (`size_label`) can never actually be empty
    // -- `size_label` always formats "<width> x <height>" (see
    // `size_label_formats_width_x_height` above) -- so there is no reachable
    // empty-text call through `draw_selection` itself to reproduce. This
    // mirrors `src/ui/palette.rs`'s `draw_text_line_tolerates_an_empty_string`
    // (commit `453fe0b`) and `crate::ui::text`'s own regression test by
    // exercising the shared guard `draw_selection` now routes through
    // directly, against a real memory DC, so a future caller of
    // `crate::ui::text::draw_text_line` from this module is covered too.
    // Per the module doc comment on `crate::ui::text`: this does NOT
    // re-trigger the raw, unguarded `DrawTextW` crash (MEASURED 2026-09-17
    // on the palette branch) -- doing so would crash this whole test binary.
    // No named kernel object, registry value or file path is created here
    // (rule 9 is moot: nothing needs a name).
    #[test]
    fn draw_text_line_tolerates_an_empty_string_in_a_memory_dc() {
        use windows::Win32::Graphics::Gdi::{DT_LEFT, DT_VCENTER};

        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 8,
                    biHeight: -8,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbitmap = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection failed");
            let old_bitmap = SelectObject(hdc, hbitmap.into());

            let rect = RECT {
                left: 0,
                top: 0,
                right: 8,
                bottom: 8,
            };
            crate::ui::text::draw_text_line(hdc, "", rect, DT_LEFT | DT_VCENTER);
            crate::ui::text::draw_text_line(hdc, "8 x 8", rect, DT_LEFT | DT_VCENTER);

            SelectObject(hdc, old_bitmap);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc);
        }
    }

    #[test]
    fn draw_selection_with_a_zero_size_rect_does_not_crash() {
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 16,
                    biHeight: -16,
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbitmap = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection failed");
            let old_bitmap = SelectObject(hdc, hbitmap.into());

            draw_selection(
                hdc,
                Rect {
                    left: 0,
                    top: 0,
                    right: 0,
                    bottom: 0,
                },
            );

            SelectObject(hdc, old_bitmap);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc);
        }
    }
}
