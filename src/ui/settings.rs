//! The settings window: a plain Win32 dialog-style window (real controls,
//! not a text editor) for everything in [`Config`] a user would reasonably
//! change. Replaces the old "Edit settings" tray action that opened
//! `config.toml` in a text editor.
//!
//! # Public API
//!
//! ```ignore
//! pub fn show_modal(instance: HINSTANCE, config: &Config) -> Option<Config>;
//! ```
//!
//! `show_modal` creates the window, runs its own `GetMessageW` loop until
//! the window is closed, and returns the edited config on Save or `None` on
//! Cancel/close. The caller (the integrating agent's tray/app glue) is
//! responsible for persisting (`Config::save`) and applying the result --
//! this module never touches disk.
//!
//! Notes for the integrating agent:
//! - If a settings window is already open, a second `show_modal` call does
//!   not open a second one: it brings the existing window to the front and
//!   returns `None` immediately. This is guarded by a process-wide static,
//!   not by anything the caller has to manage.
//! - The message loop here pumps *all* thread messages (`GetMessageW` with
//!   `hwnd = NULL`), not just this window's -- a window with child controls
//!   cannot filter by its own `HWND`, because input messages for a control
//!   are addressed to that control's `HWND`, not its parent's, and
//!   `GetMessageW(hwnd)` only ever returns messages addressed to exactly
//!   that handle. Filtering here would leave every edit/combo/button
//!   keystroke and click undelivered. Consequently, anything else already
//!   queued on this thread (e.g. a posted hotkey message aimed at the main
//!   app window) can still be dispatched to its own window proc while this
//!   modal loop runs. Whether that should be suppressed is a decision for
//!   the integrating app's own message handling, not something this module
//!   can enforce without owning that window.
//! - **Theme**: this window is always drawn with the stock (light) system
//!   control appearance, regardless of the OS light/dark setting. Unlike
//!   `card.rs` (which paints its own surface and can safely follow
//!   `AppsUseLightTheme`), a real dark mode for *native* controls (edit,
//!   combo, button, trackbar) needs per-control owner-draw or
//!   `SetWindowTheme("DarkMode_Explorer", ..)` plus `WM_CTLCOLOR*` brushes,
//!   and even then list/combo popups and the trackbar thumb stay
//!   light-on-light or otherwise inconsistent. Per the design note that a
//!   correct light window beats a broken half-dark one, this module does
//!   not attempt it.
//!
//! Fields intentionally not exposed here:
//! - `providers.openai.models` / `providers.anthropic.models` (the list
//!   backing each model dropdown) is shown and selectable from, but not
//!   editable as a list -- adding/removing model names stays a
//!   `config.toml` / tray-submenu-driven thing. Only the *active* model
//!   (one entry from that list) is set here.
//! - `hotkeys.primary` / `hotkeys.secondary` are shown with a Change button
//!   each (#345). Change arms the tray's own learn mode (`hotkey::arm_learning`),
//!   and the captured chord comes back as `WM_APP_LEARNED`, posted to the main
//!   window. This module's message loop takes that message off the queue
//!   itself (`take_learned`) while a Change is pending, because the main
//!   window's proc ignores it whenever Settings is open. The new chord is
//!   only staged in the window and saved with everything else on Save.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, EnumFontFamiliesExW, GetDC, ReleaseDC, FW_NORMAL, HFONT,
    HGDIOBJ, LOGFONTW, TEXTMETRICW,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromRect, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, SetScrollInfo, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX,
};
use windows::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SystemParametersInfoForDpi,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{GetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgCtrlID, GetDlgItem,
    GetMessageW, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, IsChild,
    IsDialogMessageW, KillTimer, LoadCursorW, PeekMessageW, PostMessageW, RegisterClassExW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, SetWindowTextW, ShowWindow,
    SystemParametersInfoW, TranslateMessage, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    GWLP_USERDATA, HWND_TOP, IDC_ARROW, MSG, NONCLIENTMETRICSW, PM_REMOVE, SPI_GETNONCLIENTMETRICS,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_ACTIVATE, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_DPICHANGED, WM_HSCROLL, WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY, WM_SETFONT, WM_SETREDRAW,
    WM_TIMER, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_DISABLED, WS_EX_CONTROLPARENT,
    WS_EX_DLGMODALFRAME, WS_GROUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetParent, GetScrollInfo, GetWindow, GW_CHILD, GW_HWNDNEXT, SB_VERT, SCROLLINFO,
    SIF_DISABLENOSCROLL, SIF_PAGE, SIF_POS, SIF_RANGE, SIF_TRACKPOS, SM_CXVSCROLL, WM_MOUSEWHEEL,
    WM_VSCROLL,
};

use crate::config::{ActiveProvider, CardCorner, Config, Hotkeys, OllamaConfig, RequireTickFor};
use crate::hotkey::{
    apply_learned, arm_learning, chord_to_string, disarm_learning, slot_name, Chord, HK_PAUSE,
    HK_PRIMARY, HK_SECONDARY, WM_APP_LEARNED,
};
use crate::provider::ollama_admin::{self, GpuStatus, ListenerKind, OllamaHealth};
use crate::provider::DEFAULT_PROMPT;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Opens the settings window modally and pumps messages until it is closed.
/// Returns the edited config on Save, `None` on Cancel/close.
///
/// If a settings window is already open, brings it to the front and returns
/// `None` without opening a second one.
pub fn show_modal(instance: HINSTANCE, config: &Config) -> Option<Config> {
    let existing = OPEN_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        return None;
    }

    if !ensure_class_registered(instance) {
        return None;
    }
    ensure_common_controls();

    let inner = Box::new(SettingsInner::new(config.clone()));
    let raw = Box::into_raw(inner);

    let class_name = wide_z(CLASS_NAME);
    let title = wide_z("Wingman settings");

    // 96dpi guess for the very first CreateWindowExW call, before we have an
    // HWND to ask GetDpiForWindow about -- corrected immediately below, same
    // two-step approach card.rs uses for its own fonts.
    let w0 = window_width_px(96);
    let h0 = to_px(WIN_H_DP, 96);

    let create_result = unsafe {
        CreateWindowExW(
            WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR(title.as_ptr()),
            WS_CAPTION | WS_SYSMENU,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w0,
            h0,
            None,
            None,
            Some(instance),
            Some(raw as *const c_void),
        )
    };

    let hwnd = match create_result {
        Ok(hwnd) => hwnd,
        Err(_) => {
            unsafe {
                drop(Box::from_raw(raw));
            }
            return None;
        }
    };

    OPEN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

    let inner_ref = unsafe { &mut *raw };
    inner_ref.hwnd = hwnd;

    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
    inner_ref.font = build_font(dpi);
    let fitted_h = fitted_height_dp(hwnd, dpi);

    // Now that we know the real DPI, resize the window (position stays
    // wherever CW_USEDEFAULT cascaded it) and lay out every child control.
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            window_width_px(dpi),
            to_px(fitted_h, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }

    // The layout pins the button row to the bottom of the client area, so it
    // needs the CLIENT height, not the window height: the caption and borders
    // are ~100px at 250% scaling. Ask the window what it actually got.
    // Floored at 1 so a later subtraction never goes negative. (#340 had this
    // clamp up to a minimum, which made `build_ui` believe in room the screen
    // did not have; with the body scrolling (#481) the true value is right.)
    let client_h_dp = {
        let mut rc = windows::Win32::Foundation::RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rc) }.is_ok() {
            ((rc.bottom - rc.top) * 96 / dpi.max(1) as i32).max(1)
        } else {
            fitted_h
        }
    };

    inner_ref.dpi = dpi;
    let built = build_ui_recorded(hwnd, instance, dpi, inner_ref.font, config, client_h_dp);
    inner_ref.adopt(built);
    // #527, #568: the Ollama status and button arrive later, off the UI
    // thread; the group says "Checking Ollama..." until then.
    spawn_ollama_status_probe(
        hwnd,
        config.providers.ollama.clone(),
        config.providers.order.iter().any(|p| p == "ollama"),
        inner_ref.ollama_status.clone(),
        inner_ref.alive.clone(),
    );
    // #183: the model list arrives later, off the UI thread.
    spawn_ollama_models_fetch(
        hwnd,
        config.providers.ollama.base_url.clone(),
        inner_ref.ollama_tags.clone(),
        inner_ref.alive.clone(),
    );
    // #380: the cloud providers' live model lists, the same way.
    spawn_cloud_models_fetch(
        hwnd,
        config,
        inner_ref.cloud_models.clone(),
        inner_ref.alive.clone(),
    );
    // #342: the prompt starts hidden, so shrink the window to match.
    unsafe { show_prompt_state(inner_ref) };
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let mut wr = windows::Win32::Foundation::RECT::default();
        if GetMonitorInfoW(mon, &mut mi).as_bool() && GetWindowRect(hwnd, &mut wr).is_ok() {
            let (x, y) = center_in_work(&mi.rcWork, wr.right - wr.left, wr.bottom - wr.top);
            let _ = SetWindowPos(
                hwnd,
                None,
                x,
                y,
                0,
                0,
                SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        // #346: without an explicit initial focus, keyboard focus landed
        // wherever it happened to fall (reported as the Max edge box) --
        // pin it to the first field instead, so Tab order starts from the
        // top of the window every time it opens.
        if let Some(first) = get_dlg_item(hwnd, ID_ACTIVE_PROVIDER) {
            let _ = SetFocus(Some(first));
        }
    }

    run_message_loop(hwnd, inner_ref.prompt_edit);

    // #345: a key captured while Save was being pressed may still be queued
    // for the main window, which would take it as a rebind. Free it unused.
    drain_learned_messages();

    // `inner_ref` is only valid up to WM_NCDESTROY, which has already run by
    // the time DestroyWindow (called from our WM_CLOSE/command handling)
    // returns, and that happens synchronously inside the message loop above.
    // Reclaim the box now to read the result and free everything.
    let inner = unsafe { Box::from_raw(raw) };
    OPEN_HWND.store(0, Ordering::SeqCst);
    inner.result
}

/// Process-wide guard: 0 when no settings window is open, else the open
/// window's `HWND` value. `isize` rather than a `Mutex<HWND>` because `HWND`
/// is just a pointer-sized value and this only ever needs a single atomic
/// compare/store, never a critical section.
static OPEN_HWND: AtomicIsize = AtomicIsize::new(0);

/// The command Enter triggers when `ctrl_id` has focus (#342): a focused
/// push button other than Save is pressed (Enter on Cancel cancels, on the
/// prompt toggle toggles); anywhere else Enter saves.
fn enter_command(ctrl_id: i32) -> i32 {
    match ctrl_id {
        ID_CANCEL | ID_PROMPT_TOGGLE | ID_RESET_PROMPT | ID_OLLAMA_LINK | ID_CHANGE_PRIMARY
        | ID_CHANGE_SECONDARY => ctrl_id,
        _ => ID_SAVE,
    }
}

/// The top-left that centres a `w` x `h` window in `work` (#342): used once
/// after the initial collapse, so the shorter window does not sit where the
/// old taller one was cascaded. Never above or left of the work area.
fn center_in_work(work: &windows::Win32::Foundation::RECT, w: i32, h: i32) -> (i32, i32) {
    let x = work.left + ((work.right - work.left) - w) / 2;
    let y = work.top + ((work.bottom - work.top) - h) / 2;
    (x.max(work.left), y.max(work.top))
}

fn run_message_loop(hwnd: HWND, prompt_edit: HWND) {
    // #481: the focused window as of the last message, to scroll a newly
    // focused control into view without a timer.
    let mut last_focus = HWND(std::ptr::null_mut());
    loop {
        let mut msg = MSG::default();
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            // WM_QUIT (0) or an error (-1). We never post WM_QUIT ourselves,
            // but bail cleanly either way rather than spin.
            break;
        }

        // #345: the hook posts a captured key to the MAIN window, so it is
        // not addressed to us. While a Change is pending it is ours.
        if msg.message == WM_APP_LEARNED && unsafe { take_learned(hwnd, &msg) } {
            continue;
        }

        let belongs_to_us = msg.hwnd == hwnd || unsafe { IsChild(hwnd, msg.hwnd) }.as_bool();

        if belongs_to_us && msg.message == WM_KEYDOWN {
            let vk = msg.wParam.0 as u32;
            if vk == VK_ESCAPE.0 as u32 {
                // #345: while a Change is pending, Esc cancels that, not the
                // whole window. (The hook normally swallows this Esc and
                // reports it as a captured key instead; see `judge_learned`.)
                if unsafe { cancel_learning_if_active(hwnd) } {
                    continue;
                }
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(ID_CANCEL as usize),
                        LPARAM(0),
                    );
                }
                continue;
            }
            // Enter submits, except inside the multiline prompt box, where
            // ES_WANTRETURN means Enter should insert a newline instead, and
            // on a focused push button, where it presses that button (#342).
            if vk == VK_RETURN.0 as u32 && msg.hwnd != prompt_edit {
                let ctrl_id = unsafe { GetDlgCtrlID(msg.hwnd) };
                unsafe {
                    let _ = PostMessageW(
                        Some(hwnd),
                        WM_COMMAND,
                        WPARAM(enter_command(ctrl_id) as usize),
                        LPARAM(0),
                    );
                }
                continue;
            }
        }

        // #481: the wheel scrolls the body wherever the pointer is in the
        // window (a control under it would otherwise eat the notch).
        if belongs_to_us && unsafe { scroll_with_wheel(hwnd, &msg, prompt_edit) } {
            continue;
        }

        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            let focus = GetFocus();
            if focus != last_focus {
                last_focus = focus;
                // Only keyboard moves scroll: scrolling between a click's
                // mouse-down and mouse-up would cancel the click.
                if !is_mouse_message(msg.message) {
                    follow_focus(hwnd, focus);
                }
            }
        }

        // SAFETY: `hwnd` is still alive until WM_NCDESTROY finishes, which
        // is dispatched synchronously above; GWLP_USERDATA was set to a
        // valid, still-owned SettingsInner pointer at WM_NCCREATE time.
        let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *const SettingsInner;
        let should_close = if ptr.is_null() {
            true
        } else {
            unsafe { (*ptr).should_close }
        };
        if should_close {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// Window class + WNDPROC
// ---------------------------------------------------------------------------

const CLASS_NAME: &str = "Wingman.Settings.Window.9d4e2b71";

static CLASS_INIT: std::sync::Once = std::sync::Once::new();
static CLASS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_class_registered(instance: HINSTANCE) -> bool {
    CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance) };
        let _ = CLASS_OK.set(ok);
    });
    CLASS_OK.get().copied().unwrap_or(false)
}

static COMMON_CONTROLS_INIT: std::sync::Once = std::sync::Once::new();

fn ensure_common_controls() {
    COMMON_CONTROLS_INIT.call_once(|| {
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_BAR_CLASSES, // covers the trackbar (msctls_trackbar32)
        };
        unsafe {
            let _ = InitCommonControlsEx(&icc);
        }
    });
}

unsafe fn register_class(instance: HINSTANCE) -> bool {
    register_class_named(instance, CLASS_NAME)
}

/// Registers the settings window class under `name`. Production uses
/// [`CLASS_NAME`]; tests pass their own so they never touch it (rule 9).
unsafe fn register_class_named(instance: HINSTANCE, name: &str) -> bool {
    let class_name = wide_z(name);
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
        hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
            (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 as isize + 1) as *mut c_void,
        ),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc) != 0
}

struct SettingsInner {
    hwnd: HWND,
    font: HFONT,
    /// The window's current DPI; updated by `WM_DPICHANGED` (#233).
    dpi: u32,
    /// Every child with its design rect, for re-layout on a DPI change.
    children: Vec<ChildLayout>,
    /// See [`BuiltUi`]: the inputs `prompt_footer_layout` needs again.
    content_top_dp: i32,
    win_h_dp: i32,
    prompt_edit: HWND,
    /// #481: the scrolling body that holds every group above the footer.
    body: HWND,
    /// The dpi the body's scroll position is expressed in, so a dpi change
    /// can rescale it.
    body_dpi: u32,
    /// #343: where the Ollama group's link button goes, if it has one. Set
    /// when the status probe reports (#527).
    ollama_link_url: Option<&'static str>,
    /// #527: the Ollama status probe's result, filled by its worker.
    ollama_status: OllamaStatusSlot,
    /// #183: the pulled-model list the background fetch reports into.
    ollama_tags: OllamaTags,
    /// #380: the cloud providers' live model lists, filled the same way.
    cloud_models: CloudModels,
    /// True while this window is open. Cleared in `WM_DESTROY`; the model
    /// fetch workers check it before posting (#569).
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// #342: whether the Advanced group shows the Check my work prompt.
    /// Starts collapsed on every open.
    prompt_expanded: bool,
    /// The config this window was opened with -- fields not touched by the
    /// UI (e.g. model lists, hotkeys) are carried through from here.
    original: Config,
    /// #345: the hotkeys as edited in this window. Starts as the original's
    /// and only reaches the returned config on Save.
    hotkeys: Hotkeys,
    /// #345: the slot a pending Change is waiting on a key for.
    learning: Option<usize>,
    /// #345: the `(slot, chord)` last warned about, so pressing the same
    /// conflicting chord again confirms it (same flow as the tray's).
    pending_conflict: Option<(usize, Chord)>,
    /// Set on Save; left `None` on Cancel/close.
    result: Option<Config>,
    should_close: bool,
}

impl SettingsInner {
    fn new(original: Config) -> Self {
        SettingsInner {
            hwnd: HWND(std::ptr::null_mut()),
            font: HFONT(std::ptr::null_mut()),
            dpi: 96,
            children: Vec::new(),
            content_top_dp: 0,
            win_h_dp: WIN_H_DP,
            prompt_edit: HWND(std::ptr::null_mut()),
            body: HWND(std::ptr::null_mut()),
            body_dpi: 96,
            ollama_link_url: None,
            ollama_status: Default::default(),
            ollama_tags: Default::default(),
            cloud_models: Default::default(),
            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            prompt_expanded: false,
            hotkeys: original.hotkeys.clone(),
            learning: None,
            pending_conflict: None,
            original,
            result: None,
            should_close: false,
        }
    }

    /// Takes over what `build_ui_recorded` built. Call after `dpi` is set:
    /// the controls were laid out for it.
    fn adopt(&mut self, built: BuiltUi) {
        self.prompt_edit = built.prompt_edit;
        self.body = built.body;
        self.body_dpi = self.dpi;
        self.children = built.children;
        self.content_top_dp = built.content_top_dp;
        self.win_h_dp = built.win_h_dp;
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        if !cs.lpCreateParams.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner;
    if ptr.is_null() {
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }
    let inner = &mut *ptr;

    match msg {
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (wparam.0 & 0xFFFF) as i32;
            let notify = ((wparam.0 >> 16) & 0xFFFF) as u32;
            handle_command(inner, id, notify);
            LRESULT(0)
        }
        WM_HSCROLL => {
            update_text_scale_label(hwnd);
            LRESULT(0)
        }
        WM_OLLAMA_STATUS => {
            let section = inner.ollama_status.lock().ok().and_then(|g| g.clone());
            if let Some(section) = section {
                apply_ollama_section(inner, &section);
            }
            LRESULT(0)
        }
        WM_OLLAMA_MODELS => {
            let models = inner.ollama_tags.lock().ok().and_then(|g| g.clone());
            if let Some(models) = models {
                apply_ollama_models(hwnd, &models);
            }
            LRESULT(0)
        }
        WM_CLOUD_MODELS => {
            let index = wparam.0;
            let fetched = inner
                .cloud_models
                .lock()
                .ok()
                .and_then(|g| g.get(index).cloned().flatten());
            if let Some(fetched) = fetched {
                let p = &inner.original.providers;
                let (combo_id, configured) = match index {
                    0 => (ID_OPENAI_MODEL, &p.openai.models),
                    1 => (ID_ANTHROPIC_MODEL, &p.anthropic.models),
                    _ => (ID_GEMINI_MODEL, &p.gemini.models),
                };
                apply_cloud_models(hwnd, combo_id, &fetched, configured);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            on_dpi_changed(inner, wparam, lparam);
            LRESULT(0)
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) == 0 && inner.learning.is_some() => {
            // Alt-tabbing away mid-Change: the hook is global, so a key typed
            // in another app would otherwise be captured as the new shortcut.
            stop_learning(inner);
            set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER if wparam.0 == LEARN_TIMER_ID => {
            learn_timed_out(inner);
            LRESULT(0)
        }
        WM_DESTROY => {
            inner.alive.store(false, Ordering::SeqCst);
            if inner.learning.take().is_some() {
                let _ = KillTimer(Some(hwnd), LEARN_TIMER_ID);
                disarm_learning();
            }
            if !inner.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(inner.font.0));
                inner.font = HFONT(std::ptr::null_mut());
            }
            inner.should_close = true;
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Detach GWLP_USERDATA so no later message can dereference a
            // pointer whose Box the caller is about to reclaim.
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_command(inner: &mut SettingsInner, id: i32, notify: u32) {
    match id {
        ID_SAVE => {
            let raw = read_raw_form(inner);
            // #344: an invalid numeric field must not be silently kept at
            // its old value -- reject the Save, keep the window open, focus
            // the bad field and say what is wrong inline (rule 7: never a
            // dialog box). `config.toml` is never touched in this branch.
            if let Some(invalid) = find_invalid_numeric_field(&raw) {
                if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                    set_text(msg, invalid.message());
                }
                focus_and_select_field(inner.hwnd, invalid.control_id());
                return;
            }
            if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                set_text(msg, "");
            }

            // Autostart is registry state rather than a Config field, so it is
            // applied here instead of riding along in the returned Config.
            // Only written when it actually changed, to avoid rewriting the
            // Run key on every Save.
            let want = checkbox_checked(inner.hwnd, ID_AUTOSTART);
            if want != crate::autostart::is_enabled() {
                let _ = crate::autostart::set_enabled(want);
            }
            inner.result = Some(finalize_config(inner, &raw));
            unsafe {
                let _ = DestroyWindow(inner.hwnd);
            }
        }
        ID_CANCEL => {
            inner.result = None;
            unsafe {
                let _ = DestroyWindow(inner.hwnd);
            }
        }
        ID_OLLAMA_LINK if notify == BN_CLICKED => {
            if let Some(url) = inner.ollama_link_url {
                if !open_url(url) {
                    // Rule 7: say so inline, never a dialog or a silent no-op.
                    if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
                        set_text(status, &format!("Could not open your browser. Go to {url}"));
                    }
                }
            }
        }
        ID_RESET_PROMPT => {
            set_text(inner.prompt_edit, DEFAULT_PROMPT);
        }
        ID_CHANGE_PRIMARY if notify == BN_CLICKED => start_change(inner, HK_PRIMARY),
        ID_CHANGE_SECONDARY if notify == BN_CLICKED => start_change(inner, HK_SECONDARY),
        ID_PROMPT_TOGGLE if notify == BN_CLICKED => {
            inner.prompt_expanded = !inner.prompt_expanded;
            unsafe { show_prompt_state(inner) };
        }
        ID_OPENAI_SHOW_KEY if notify == BN_CLICKED => {
            toggle_password(inner.hwnd, ID_OPENAI_SHOW_KEY, ID_OPENAI_KEY);
        }
        ID_ANTHROPIC_SHOW_KEY if notify == BN_CLICKED => {
            toggle_password(inner.hwnd, ID_ANTHROPIC_SHOW_KEY, ID_ANTHROPIC_KEY);
        }
        ID_GEMINI_SHOW_KEY if notify == BN_CLICKED => {
            toggle_password(inner.hwnd, ID_GEMINI_SHOW_KEY, ID_GEMINI_KEY);
        }
        _ => {}
    }
}

fn toggle_password(hwnd: HWND, checkbox_id: i32, edit_id: i32) {
    let checked = get_dlg_item(hwnd, checkbox_id)
        .map(|h| unsafe { SendMessageW(h, BM_GETCHECK, None, None).0 as i32 == BST_CHECKED })
        .unwrap_or(false);
    if let Some(edit) = get_dlg_item(hwnd, edit_id) {
        let ch: u32 = if checked { 0 } else { '*' as u32 };
        unsafe {
            SendMessageW(edit, EM_SETPASSWORDCHAR, Some(WPARAM(ch as usize)), None);
            let _ = InvalidateRect(Some(edit), None, true);
        }
    }
}

/// #344: sets keyboard focus on the named control and selects its whole
/// contents, so the user can just start typing over the bad value. For any
/// plain `WC_EDIT` control (e.g. `ID_CARD_SECONDS`). `ID_MAX_EDGE` became a
/// `CBS_DROPDOWNLIST` combo in #341 (no free-typed text, so it can no longer
/// actually reach this function with an invalid value from the real UI, only
/// from a direct unit-test call) -- `EM_SETSEL` on a `CBS_DROPDOWNLIST`
/// combo with no editable text portion is simply a no-op, so no special case
/// is needed here any more.
fn focus_and_select_field(hwnd: HWND, id: i32) {
    let Some(ctrl) = get_dlg_item(hwnd, id) else {
        return;
    };
    unsafe {
        let _ = SetFocus(Some(ctrl));
        SendMessageW(ctrl, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1isize)));
    }
}

fn update_text_scale_label(hwnd: HWND) {
    let (Some(track), Some(label)) = (
        get_dlg_item(hwnd, ID_TEXT_SCALE_TRACK),
        get_dlg_item(hwnd, ID_TEXT_SCALE_LABEL),
    ) else {
        return;
    };
    let pos = unsafe { SendMessageW(track, TBM_GETPOS, None, None).0 as i32 };
    let scale = clamp_text_scale(pos as f32 / 100.0);
    set_text(label, &format!("{scale:.2}x"));
}

// ---------------------------------------------------------------------------
// Control IDs
// ---------------------------------------------------------------------------

const ID_ACTIVE_PROVIDER: i32 = 100;
const ID_OPENAI_KEY: i32 = 101;
const ID_OPENAI_SHOW_KEY: i32 = 102;
const ID_OPENAI_MODEL: i32 = 103;
const ID_OPENAI_EFFORT: i32 = 104;
const ID_ANTHROPIC_KEY: i32 = 105;
const ID_ANTHROPIC_SHOW_KEY: i32 = 106;
const ID_ANTHROPIC_MODEL: i32 = 107;
const ID_ANTHROPIC_EFFORT: i32 = 108;
const ID_MAX_EDGE: i32 = 109;
const ID_MONITOR: i32 = 110;
const ID_CARD_SECONDS: i32 = 111;
const ID_TEXT_SCALE_TRACK: i32 = 113;
const ID_TEXT_SCALE_LABEL: i32 = 114;
const ID_PROMPT_EDIT: i32 = 115;
const ID_RESET_PROMPT: i32 = 116;
const ID_SAVE: i32 = 117;
const ID_AUTOSTART: i32 = 118;
const ID_CANCEL: i32 = 119;
/// Read-only status line combining #15's health check and #14's GPU/CPU
/// indicator (see [`ollama_status_line`]). Not a form field -- never read
/// back in `read_form`/`build_config`.
const ID_OLLAMA_STATUS: i32 = 120;
/// #344: the inline "that field is wrong" message shown above Save/Cancel
/// after a Save attempt with an invalid numeric field. Not a form field --
/// never read back in `read_form`/`build_config`.
const ID_VALIDATION_MESSAGE: i32 = 121;
/// #403: the fill-form tick-requirement combo (`config.forms.require_tick_for`).
const ID_REQUIRE_TICK_FOR: i32 = 122;
/// #343: the Ollama group's link button (Get Ollama / Local models).
const ID_OLLAMA_LINK: i32 = 124;
/// #342: shows or hides the Check my work prompt in the Advanced group.
const ID_PROMPT_TOGGLE: i32 = 123;
/// #481: the scrolling body window that holds every group above the footer.
const ID_BODY: i32 = 125;
/// #183: the Ollama model combo, filled from the locally pulled models.
const ID_OLLAMA_MODEL: i32 = 126;
/// #195: the Gemini key field, its Show toggle, and its model and thinking combos.
const ID_GEMINI_KEY: i32 = 127;
const ID_GEMINI_SHOW_KEY: i32 = 128;
const ID_GEMINI_MODEL: i32 = 129;
const ID_GEMINI_EFFORT: i32 = 130;
/// #345: the Change buttons beside the two shortcuts, the labels that show
/// them, and the one status line the learn flow writes into.
const ID_CHANGE_PRIMARY: i32 = 131;
const ID_CHANGE_SECONDARY: i32 = 132;
const ID_HOTKEY_STATUS: i32 = 133;
const ID_PRIMARY_LABEL: i32 = 134;
const ID_SECONDARY_LABEL: i32 = 135;
/// #498: the card-corner and card-monitor combos in the Card group.
const ID_CARD_CORNER: i32 = 136;
const ID_CARD_MONITOR: i32 = 137;
/// #509: the capture.redact checkbox in the Capture group.
const ID_REDACT: i32 = 138;
/// #509: what capture.redact covers, in plain words. Three things: the
/// screenshot (card numbers, keys, emails and password fields painted over),
/// the text sent to a cloud or remote model, and the answer (real values put
/// back). Local models skip it.
const REDACT_LABEL: &str = "Hide card numbers, keys, emails and passwords before sending";
const REDACT_HINT: &str = "Painted over in screenshots and masked in text sent to cloud or remote models. The answer shows the real values. Local models skip it.";
/// #345: the one-shot timer that ends a pending Change that nobody pressed a
/// key for. Only ever set while a Change is pending, so nothing ticks idle.
const LEARN_TIMER_ID: usize = 0x345;
/// A little over the hook's own 10 second learn window.
const LEARN_TIMER_MS: u32 = 10_500;

/// Every control id declared above, paired with its constant name for a
/// legible test failure. Two controls sharing an id means `GetDlgItem`
/// resolves to whichever was created last and a `WM_COMMAND` from one fires
/// the other's handler (issue #144: `ID_AUTOSTART` and `ID_CANCEL` both being
/// `118` made the autostart checkbox close the dialog instead of toggling).
/// Add new `ID_*` constants here too -- `control_ids_are_pairwise_unique`
/// below iterates this array, so an omitted id is invisible to the test.
#[cfg(test)]
const ALL_CONTROL_IDS: &[(&str, i32)] = &[
    ("ID_ACTIVE_PROVIDER", ID_ACTIVE_PROVIDER),
    ("ID_OPENAI_KEY", ID_OPENAI_KEY),
    ("ID_OPENAI_SHOW_KEY", ID_OPENAI_SHOW_KEY),
    ("ID_OPENAI_MODEL", ID_OPENAI_MODEL),
    ("ID_OPENAI_EFFORT", ID_OPENAI_EFFORT),
    ("ID_ANTHROPIC_KEY", ID_ANTHROPIC_KEY),
    ("ID_ANTHROPIC_SHOW_KEY", ID_ANTHROPIC_SHOW_KEY),
    ("ID_ANTHROPIC_MODEL", ID_ANTHROPIC_MODEL),
    ("ID_ANTHROPIC_EFFORT", ID_ANTHROPIC_EFFORT),
    ("ID_MAX_EDGE", ID_MAX_EDGE),
    ("ID_MONITOR", ID_MONITOR),
    ("ID_CARD_SECONDS", ID_CARD_SECONDS),
    ("ID_TEXT_SCALE_TRACK", ID_TEXT_SCALE_TRACK),
    ("ID_TEXT_SCALE_LABEL", ID_TEXT_SCALE_LABEL),
    ("ID_PROMPT_EDIT", ID_PROMPT_EDIT),
    ("ID_RESET_PROMPT", ID_RESET_PROMPT),
    ("ID_SAVE", ID_SAVE),
    ("ID_AUTOSTART", ID_AUTOSTART),
    ("ID_CANCEL", ID_CANCEL),
    ("ID_OLLAMA_STATUS", ID_OLLAMA_STATUS),
    ("ID_VALIDATION_MESSAGE", ID_VALIDATION_MESSAGE),
    ("ID_REQUIRE_TICK_FOR", ID_REQUIRE_TICK_FOR),
    ("ID_OLLAMA_LINK", ID_OLLAMA_LINK),
    ("ID_PROMPT_TOGGLE", ID_PROMPT_TOGGLE),
    ("ID_BODY", ID_BODY),
    ("ID_OLLAMA_MODEL", ID_OLLAMA_MODEL),
    ("ID_GEMINI_KEY", ID_GEMINI_KEY),
    ("ID_GEMINI_SHOW_KEY", ID_GEMINI_SHOW_KEY),
    ("ID_GEMINI_MODEL", ID_GEMINI_MODEL),
    ("ID_GEMINI_EFFORT", ID_GEMINI_EFFORT),
    ("ID_CHANGE_PRIMARY", ID_CHANGE_PRIMARY),
    ("ID_CHANGE_SECONDARY", ID_CHANGE_SECONDARY),
    ("ID_HOTKEY_STATUS", ID_HOTKEY_STATUS),
    ("ID_PRIMARY_LABEL", ID_PRIMARY_LABEL),
    ("ID_SECONDARY_LABEL", ID_SECONDARY_LABEL),
    ("ID_CARD_CORNER", ID_CARD_CORNER),
    ("ID_CARD_MONITOR", ID_CARD_MONITOR),
    ("ID_REDACT", ID_REDACT),
];

// ---------------------------------------------------------------------------
// Win32 constants this crate's `windows` feature set doesn't expose
// ---------------------------------------------------------------------------
// `Cargo.toml` is owned by another agent and enables only the `windows`
// feature groups other modules already need; rather than touch it, the
// handful of message/style constants missing from those groups are
// hand-declared here (values from the platform SDK's winuser.h /
// commctrl.h, which never change -- these are stable ABI constants, not
// something the crate chose to omit for a reason that could shift).

const BM_GETCHECK: u32 = 0x00F0;
const BST_CHECKED: i32 = 1;
const BST_UNCHECKED: i32 = 0;
const BN_CLICKED: u32 = 0;
const EM_SETPASSWORDCHAR: u32 = 0x00CC;
const CB_ADDSTRING: u32 = 0x0143;
const CB_RESETCONTENT: u32 = 0x014B;
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
const CB_GETDROPPEDSTATE: u32 = 0x0157;
#[cfg(test)]
const CB_SHOWDROPDOWN: u32 = 0x014F;
const CB_SELECTSTRING: u32 = 0x014D;
const TBM_GETPOS: u32 = 0x0400;
const TBM_SETRANGE: u32 = 0x0406;
const TBM_SETPOS: u32 = 0x0405;
const TBM_SETPAGESIZE: u32 = 0x0415;
const TBM_SETLINESIZE: u32 = 0x0417;
const CBS_DROPDOWNLIST: i32 = 0x0003;
const CBS_HASSTRINGS: i32 = 0x0200;
const ES_PASSWORD: i32 = 0x0020;
const ES_MULTILINE: i32 = 0x0004;
const ES_WANTRETURN: i32 = 0x1000;
const ES_AUTOVSCROLL: i32 = 0x0040;
/// Single-line edits NEED this. Without it the control accepts only as much
/// text as fits its visible width, so pasting a ~108-character API key
/// silently drops everything past the right-hand edge. `ES_AUTOVSCROLL` is the
/// multiline vertical equivalent and does nothing here.
const ES_AUTOHSCROLL: i32 = 0x0080;
#[cfg(test)]
const WM_PASTE: u32 = 0x0302;
const ES_NUMBER: i32 = 0x2000;
const BS_GROUPBOX: i32 = 0x0007;
const BS_AUTOCHECKBOX: i32 = 0x0003;
const BS_PUSHBUTTON: i32 = 0x0000;
const BS_DEFPUSHBUTTON: i32 = 0x0001;
const TBS_HORZ: i32 = 0x0000;
/// #344: selects text in a plain edit control, used to highlight an invalid
/// numeric field after a failed Save.
const EM_SETSEL: u32 = 0x00B1;

use windows::Win32::Graphics::Gdi::{
    InvalidateRect, RedrawWindow, RDW_ALLCHILDREN, RDW_ERASE, RDW_INVALIDATE, RDW_UPDATENOW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
use windows::Win32::UI::WindowsAndMessaging::SendMessageW;

// ---------------------------------------------------------------------------
// Layout (logical pixels at 96 DPI; scaled per-window by `to_px`)
// ---------------------------------------------------------------------------

/// The window's height in design units, clamped to what this monitor can
/// actually display.
///
/// `WIN_H_DP` is the height the layout wants. At a high scale factor that can
/// exceed the screen: 820dp at 250% is 2050px on a 2000px-tall panel, which
/// puts Save and Cancel below the bottom edge where they cannot be clicked.
/// The prompt box is the elastic part of the layout, so giving back a smaller
/// height simply shrinks it and everything stays reachable.
fn fitted_height_dp(hwnd: HWND, dpi: u32) -> i32 {
    let mut work_px = 0i32;
    unsafe {
        let mon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            work_px = mi.rcWork.bottom - mi.rcWork.top;
        }
    }
    fitted_height_for_work_px(work_px, dpi)
}

/// Pure core of [`fitted_height_dp`]: the window height in dp for a monitor
/// work area `work_px` tall at `dpi`. A non-positive height (failed query)
/// gives the full design height.
fn fitted_height_for_work_px(work_px: i32, dpi: u32) -> i32 {
    if work_px <= 0 {
        return WIN_H_DP;
    }
    // Leave room for the title bar and a little breathing space.
    let avail_dp = (work_px * 96 / dpi.max(1) as i32) - 48;
    // #481: the body scrolls, so the window can be as short as the screen
    // needs. The floor only guards a failed or absurd query, and holds the
    // pinned footer plus a sliver of body; it never lifts the window above
    // a work area that is really that small.
    WIN_H_DP.min(avail_dp).max(MIN_WIN_H_DP)
}

/// `WM_DPICHANGED`'s suggested `(x, y, w, h)` clamped to the target
/// monitor's work area the way `show_modal` does at open (#340): the height
/// never exceeds `fitted_height_for_work_px`, and the window is slid up if
/// its bottom would fall below the work area, so Save and Cancel stay on
/// screen. `work` is the target monitor's `rcWork`.
fn clamp_suggested_to_work(
    pos: (i32, i32, i32, i32),
    work: &windows::Win32::Foundation::RECT,
    dpi: u32,
) -> (i32, i32, i32, i32) {
    let (x, mut y, w, mut h) = pos;
    let work_h = work.bottom - work.top;
    if work_h <= 0 {
        return pos;
    }
    h = h
        .min(to_px(fitted_height_for_work_px(work_h, dpi), dpi))
        .max(1);
    if y + h > work.bottom {
        y = (work.bottom - h).max(work.top);
    }
    (x, y, w, h)
}

const WIN_W_DP: i32 = 620;
/// #403 review: raised from 820 by the Forms group's full height (68dp: see
/// `CONTENT_TOP_DP`'s comment). #341 raised it again by 30dp (one more
/// `ROW_H + ROW_GAP` row: the Capture group's new "higher reads small text
/// better" hint line) so the default window still has room for the Prompt
/// group at something close to its natural size instead of always falling
/// back to `prompt_footer_layout`'s minimum -- `prompt_footer_layout` itself
/// never overlaps regardless of this constant, but a taller default is nicer
/// than always squeezing.
const WIN_H_DP: i32 = 918 + 3 * (ROW_H + ROW_GAP);
/// The shortest the window is ever made, in dp: the pinned footer and a sliver
/// of body. It is only a floor against a failed or absurd monitor query (see
/// `fitted_height_for_work_px`); a real work area is always taller, and a
/// shorter one just scrolls (#481).
///
/// History (#340, MEASURED 2026-09-24): this used to be 420 and
/// `show_modal` clamped the measured client height up to it, which made
/// `build_ui` think the window was taller than it was and pushed the footer
/// below the visible area on small, high-DPI screens. Neither the floor nor
/// that clamp exists in that form any more: the footer is pinned to the real
/// client bottom and the body scrolls.
const MIN_WIN_H_DP: i32 = 2 * FOOTER_H_DP;

/// Height of the pinned Save/Cancel footer, in dp: the button row and its
/// top and bottom margins (#481).
const FOOTER_H_DP: i32 = BTN_H + MARGIN * 2;
/// Space kept below the last group inside the scrolling body, in dp.
const BODY_PAD_BOTTOM: i32 = MARGIN / 2;

const MARGIN: i32 = 16;
const ROW_H: i32 = 22;
const ROW_GAP: i32 = 8;
const GROUP_GAP: i32 = 14;
const GROUP_LABEL_TOP: i32 = 20; // room for the groupbox caption
const LABEL_W: i32 = 130;
const BTN_W: i32 = 100;
const BTN_H: i32 = 28;

fn to_px(dp: i32, dpi: u32) -> i32 {
    ((dp as i64 * dpi as i64 + 48) / 96) as i32
}

/// Below this the prompt box stops being usable; scroll rather than shrink
/// further. (`MIN_PROMPT_EDIT_H` in dp, three text rows tall.)
const MIN_PROMPT_EDIT_H: i32 = ROW_H * 3;

/// Width of the Ollama group's link button (#343), in dp.
const OLLAMA_LINK_W: i32 = 120;
/// Width of the "Show/Hide Check my work prompt" toggle (#342), in dp.
const PROMPT_TOGGLE_W: i32 = 210;

/// Rects for the Prompt group box and the Save/Cancel footer, computed
/// purely from where the fixed-height groups above it end (`content_top`,
/// in dp) and how tall the window's client area actually is (`win_h_dp`).
///
/// #481: the footer (Save, Cancel, the validation line) is pinned to the
/// bottom of the client area and the groups live in a scrolling body above
/// it, so nothing can be cut off however short the window is: what does not
/// fit scrolls. `buttons_y` is therefore always `win_h_dp` minus the footer,
/// and [`PromptFooterLayout::content_h`] says how tall the body's content is,
/// which the caller compares with the viewport to size the scroll range.
///
/// History: issue #340 (MEASURED 2026-09-24, 150% scaling) had the footer
/// placed from `win_h_dp` while the Prompt group's top did not shrink with
/// the window, so Save and Cancel were drawn over the Prompt box. Pinning
/// the footer and scrolling the body makes that overlap impossible rather
/// than merely avoided: the two live in different windows.
struct PromptFooterLayout {
    prompt_top: i32,
    prompt_bottom: i32,
    prompt_edit_h: i32,
    reset_btn_y: i32,
    buttons_y: i32,
}

impl PromptFooterLayout {
    /// The height of everything in the scrolling body, in dp.
    fn content_h(&self) -> i32 {
        self.prompt_bottom + BODY_PAD_BOTTOM
    }
}

fn prompt_footer_layout(content_top: i32, win_h_dp: i32) -> PromptFooterLayout {
    let reset_btn_h = BTN_H;
    let prompt_top = content_top;
    let prompt_label_bottom = prompt_top + GROUP_LABEL_TOP;

    // The smallest the Prompt group can be and still hold a usable edit box
    // and the reset button: label, minimum edit height, the gap before the
    // reset button, the reset button itself, and a bottom margin before the
    // group's own border.
    let min_prompt_group_h = GROUP_LABEL_TOP + MIN_PROMPT_EDIT_H + ROW_GAP + reset_btn_h + MARGIN;

    // With room to spare the group stretches to the viewport's bottom, so a
    // tall window shows no scrollbar range; without it the group takes its
    // minimum and the body scrolls.
    let desired_prompt_bottom = win_h_dp - FOOTER_H_DP - BODY_PAD_BOTTOM;
    let prompt_bottom = desired_prompt_bottom.max(prompt_top + min_prompt_group_h);

    let prompt_edit_h = ((prompt_bottom - MARGIN) - prompt_label_bottom - reset_btn_h - ROW_GAP)
        .max(MIN_PROMPT_EDIT_H);
    let reset_btn_y = prompt_label_bottom + prompt_edit_h + ROW_GAP;

    PromptFooterLayout {
        prompt_top,
        prompt_bottom,
        prompt_edit_h,
        reset_btn_y,
        buttons_y: win_h_dp - BTN_H - MARGIN,
    }
}

/// Height of the Advanced group while the prompt is hidden (#342): the
/// caption, one button row (the show/hide toggle) and a bottom margin.
const COLLAPSED_PROMPT_GROUP_H: i32 = GROUP_LABEL_TOP + BTN_H + MARGIN;

/// The footer layout for the Advanced group in either state (#342).
/// Expanded is [`prompt_footer_layout`] unchanged. Collapsed has no edit box:
/// the group is just its toggle row, so it does not stretch with the window.
fn footer_layout(content_top: i32, win_h_dp: i32, expanded: bool) -> PromptFooterLayout {
    if expanded {
        return prompt_footer_layout(content_top, win_h_dp);
    }
    PromptFooterLayout {
        prompt_top: content_top,
        prompt_bottom: content_top + COLLAPSED_PROMPT_GROUP_H,
        prompt_edit_h: 0,
        reset_btn_y: content_top + GROUP_LABEL_TOP,
        buttons_y: win_h_dp - BTN_H - MARGIN,
    }
}

/// The window height in dp for the Advanced group's state (#342).
/// `fitted_dp` is what [`fitted_height_for_work_px`] allows on this monitor
/// and `nc_dp` the title bar and borders. Expanded keeps the old behaviour
/// (the full fitted height, the prompt box takes what is left). Collapsed is
/// just tall enough for the whole body and the footer, and never taller than
/// the fit; anything beyond the fit scrolls (#481).
fn window_height_dp(content_top: i32, expanded: bool, fitted_dp: i32, nc_dp: i32) -> i32 {
    if expanded {
        return fitted_dp;
    }
    let client = footer_layout(content_top, 0, false).content_h() + FOOTER_H_DP;
    (client + nc_dp).min(fitted_dp)
}

// ---------------------------------------------------------------------------
// Scrolling body (#481). The pure arithmetic: positions and sizes are all in
// physical pixels, content coordinates start at the top of the body's content.
// ---------------------------------------------------------------------------

/// How far the body can scroll: the content's overflow past the viewport.
fn max_scroll(content: i32, viewport: i32) -> i32 {
    (content - viewport).max(0)
}

/// `pos` limited to the scrollable range.
fn clamp_scroll(pos: i32, content: i32, viewport: i32) -> i32 {
    pos.clamp(0, max_scroll(content, viewport))
}

/// The scroll position that makes the content-space span `top..bottom`
/// visible with the least movement: unchanged when it already is, the
/// nearest edge otherwise. A span taller than the viewport is aligned to its
/// top, so what the user just focused starts in view.
fn scroll_into_view(pos: i32, content: i32, viewport: i32, top: i32, bottom: i32) -> i32 {
    let pos = clamp_scroll(pos, content, viewport);
    let target = if top < pos {
        top
    } else if bottom > pos + viewport {
        if bottom - top >= viewport {
            top
        } else {
            bottom - viewport
        }
    } else {
        pos
    };
    clamp_scroll(target, content, viewport)
}

/// WM_MOUSEWHEEL's notch size.
const WHEEL_DELTA: i32 = 120;

/// The change in scroll position for a wheel `delta` at `dpi`: three rows per
/// notch, and against the wheel's direction (wheel up scrolls the content
/// down the screen, so the position decreases).
fn wheel_scroll_px(delta: i32, dpi: u32) -> i32 {
    -(delta * 3 * to_px(ROW_H, dpi)) / WHEEL_DELTA
}

// The low word of a WM_VSCROLL wParam.
const SB_LINEUP: u32 = 0;
const SB_LINEDOWN: u32 = 1;
const SB_PAGEUP: u32 = 2;
const SB_PAGEDOWN: u32 = 3;
const SB_THUMBPOSITION: u32 = 4;
const SB_THUMBTRACK: u32 = 5;
const SB_TOP: u32 = 6;
const SB_BOTTOM: u32 = 7;

/// Where a `WM_VSCROLL` request `code` leaves the position, limited to
/// `0..=max`. `track` is the thumb's live position. An unknown code changes
/// nothing.
fn scroll_request_target(code: u32, pos: i32, track: i32, line: i32, page: i32, max: i32) -> i32 {
    let target = match code {
        SB_LINEUP => pos - line,
        SB_LINEDOWN => pos + line,
        SB_PAGEUP => pos - page,
        SB_PAGEDOWN => pos + page,
        SB_THUMBPOSITION | SB_THUMBTRACK => track,
        SB_TOP => 0,
        SB_BOTTOM => max,
        _ => pos,
    };
    target.clamp(0, max.max(0))
}

// ---------------------------------------------------------------------------
// DPI change (#233)
// ---------------------------------------------------------------------------

/// Which part of the layout a child belongs to when the window's height
/// changes along with its DPI. Everything above the Prompt group is
/// fixed-height; the Prompt group stretches, and the reset button and the
/// footer row ride its bottom edge (see [`prompt_footer_layout`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Anchor {
    Fixed,
    PromptEdit,
    PromptGroup,
    ResetBtn,
    Footer,
}

/// A child control and the design (96dpi) rect it was created with.
#[derive(Clone, Copy, Debug)]
struct ChildLayout {
    hwnd: HWND,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    anchor: Anchor,
}

/// The DPI carried in a `WM_DPICHANGED` `wParam`: the low word is the new X
/// DPI (the high word is the Y DPI, always equal for a window). Floored at 1
/// so a later division can never be by zero.
fn dpi_from_wparam(wparam: usize) -> u32 {
    ((wparam & 0xFFFF) as u32).max(1)
}

/// The suggested window rect from a `WM_DPICHANGED` `lParam` as
/// `(x, y, width, height)`, with width and height floored at 1.
fn suggested_pos(r: &windows::Win32::Foundation::RECT) -> (i32, i32, i32, i32) {
    (
        r.left,
        r.top,
        (r.right - r.left).max(1),
        (r.bottom - r.top).max(1),
    )
}

/// One child's rect in physical pixels at `dpi`, as `(x, y, w, h)`. `old` is
/// the elastic-region layout the child's design rect was recorded under and
/// `new` the layout for the window's current client height: the anchored
/// children move or stretch by the difference, then everything is scaled.
fn child_rect_px(
    c: &ChildLayout,
    old: &PromptFooterLayout,
    new: &PromptFooterLayout,
    dpi: u32,
) -> (i32, i32, i32, i32) {
    let (mut y, mut h) = (c.y, c.h);
    match c.anchor {
        Anchor::Fixed => {}
        Anchor::PromptEdit => h += new.prompt_edit_h - old.prompt_edit_h,
        Anchor::PromptGroup => {
            h += (new.prompt_bottom - new.prompt_top) - (old.prompt_bottom - old.prompt_top)
        }
        Anchor::ResetBtn => y += new.reset_btn_y - old.reset_btn_y,
        Anchor::Footer => y += new.buttons_y - old.buttons_y,
    }
    (
        to_px(c.x, dpi),
        to_px(y, dpi),
        to_px(c.w, dpi),
        to_px(h, dpi),
    )
}

/// `WM_DPICHANGED`: the window moved to a monitor with a different scale.
/// Takes the new DPI, honors the system's suggested rect, rebuilds the font
/// and re-lays-out every child at the new scale.
unsafe fn on_dpi_changed(inner: &mut SettingsInner, wparam: WPARAM, lparam: LPARAM) {
    inner.dpi = dpi_from_wparam(wparam.0);
    if lparam.0 != 0 {
        let rect = &*(lparam.0 as *const windows::Win32::Foundation::RECT);
        let mut pos = suggested_pos(rect);
        let mon = MonitorFromRect(rect, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(mon, &mut mi).as_bool() {
            pos = clamp_suggested_to_work(pos, &mi.rcWork, inner.dpi);
        }
        let (x, y, w, h) = pos;
        let _ = SetWindowPos(inner.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    }
    apply_dpi(inner);
}

/// Rebuilds the font and re-lays-out the children for `inner.dpi`, using the
/// window's current client size: the footer is pinned to the bottom, the body
/// fills the rest, and its scroll range follows the content (#481).
unsafe fn apply_dpi(inner: &mut SettingsInner) {
    let dpi = inner.dpi;
    let mut rc = windows::Win32::Foundation::RECT::default();
    let (client_w_px, client_h_px) = if GetClientRect(inner.hwnd, &mut rc).is_ok() {
        (rc.right - rc.left, rc.bottom - rc.top)
    } else {
        (to_px(WIN_W_DP, dpi), to_px(inner.win_h_dp, dpi))
    };
    let client_h_dp = (client_h_px * 96 / dpi as i32).max(1);
    // The design rects were recorded for the collapsed layout (#342).
    let old = footer_layout(inner.content_top_dp, inner.win_h_dp, false);
    let new = footer_layout(inner.content_top_dp, client_h_dp, inner.prompt_expanded);

    // The body is the client area above the footer. Its scroll position
    // carries over a dpi change (rescaled) and is kept inside the new range.
    let view_px = (client_h_px - to_px(FOOTER_H_DP, dpi)).max(1);
    let content_px = to_px(new.content_h(), dpi);
    let have_body = !inner.body.0.is_null();
    let old_pos = if have_body {
        body_scroll_info(inner.body).nPos
    } else {
        0
    };
    let pos = clamp_scroll(
        (old_pos as i64 * dpi as i64 / inner.body_dpi.max(1) as i64) as i32,
        content_px,
        view_px,
    );
    inner.body_dpi = dpi;
    // The footer sits right of the body's scrollbar, so it is shifted over by
    // the bar's width to keep its right edge on the window's.
    let sbw = scrollbar_width_px(dpi);

    let new_font = build_font(dpi);
    // No repaint between the individual moves: one redraw at the end, so a
    // drag across monitors does not flicker.
    SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(0)), None);
    if have_body {
        let _ = SetWindowPos(
            inner.body,
            None,
            0,
            0,
            client_w_px.max(1),
            view_px,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    for c in &inner.children {
        let (x, y, w, h) = child_rect_px(c, &old, &new, dpi);
        let (x, y) = if c.anchor == Anchor::Footer {
            (x + sbw, y)
        } else {
            (x, y - pos)
        };
        let _ = SetWindowPos(c.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        SendMessageW(
            c.hwnd,
            WM_SETFONT,
            Some(WPARAM(new_font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    if have_body {
        set_body_scroll(inner.body, content_px, view_px, pos);
    }
    // Every child now uses the new font, so the old one can go.
    if !inner.font.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(inner.font.0));
    }
    inner.font = new_font;
    // `win_h_dp` stays the baseline the recorded design rects were built
    // for, so every later change diffs against the same reference.
    SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(1)), None);
    let _ = RedrawWindow(
        Some(inner.hwnd),
        None,
        None,
        RDW_ERASE | RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// #342: makes the window match `inner.prompt_expanded`: shows or hides the
/// prompt box and its reset button, relabels the toggle, resizes the window
/// (kept inside the monitor's work area) and re-lays-out the children.
/// The prompt text stays in the hidden edit, so Save still reads it.
unsafe fn show_prompt_state(inner: &mut SettingsInner) {
    let cmd = if inner.prompt_expanded {
        SW_SHOW
    } else {
        SW_HIDE
    };
    let _ = ShowWindow(inner.prompt_edit, cmd);
    if let Some(reset) = get_dlg_item(inner.hwnd, ID_RESET_PROMPT) {
        let _ = ShowWindow(reset, cmd);
    }
    if let Some(toggle) = get_dlg_item(inner.hwnd, ID_PROMPT_TOGGLE) {
        set_text(toggle, prompt_toggle_label(inner.prompt_expanded));
    }

    let dpi = inner.dpi.max(1);
    let mon = MonitorFromWindow(inner.hwnd, MONITOR_DEFAULTTONEAREST);
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let have_work = GetMonitorInfoW(mon, &mut mi).as_bool();
    let work = mi.rcWork;
    let fitted = fitted_height_for_work_px(if have_work { work.bottom - work.top } else { 0 }, dpi);

    let mut wr = windows::Win32::Foundation::RECT::default();
    let mut cr = windows::Win32::Foundation::RECT::default();
    if GetWindowRect(inner.hwnd, &mut wr).is_err() || GetClientRect(inner.hwnd, &mut cr).is_err() {
        apply_dpi(inner);
        return;
    }
    let nc_dp = ((wr.bottom - wr.top) - (cr.bottom - cr.top)) * 96 / dpi as i32;
    let h = to_px(
        window_height_dp(inner.content_top_dp, inner.prompt_expanded, fitted, nc_dp),
        dpi,
    );
    let mut y = wr.top;
    if have_work && y + h > work.bottom {
        y = (work.bottom - h).max(work.top);
    }
    let _ = SetWindowPos(
        inner.hwnd,
        None,
        wr.left,
        y,
        wr.right - wr.left,
        h,
        SWP_NOZORDER | SWP_NOACTIVATE,
    );
    apply_dpi(inner);
    // Opening the prompt on a short window: bring the box into view.
    if inner.prompt_expanded && !inner.body.0.is_null() {
        ensure_visible(inner.body, inner.prompt_edit);
    }
}

/// The settings window's outer width in px: the design width plus the body's
/// scrollbar, which is always shown (disabled when unneeded) so the layout
/// never shifts (#481).
fn window_width_px(dpi: u32) -> i32 {
    to_px(WIN_W_DP, dpi) + scrollbar_width_px(dpi)
}

// ---------------------------------------------------------------------------
// Scrolling body (#481)
// ---------------------------------------------------------------------------
// Every group lives in a child window (the body) with its own vertical
// scrollbar; Save, Cancel and the validation line are siblings of the body, so
// they stay pinned at the bottom of the client area. The scrollbar's own state
// (range = content height, page = viewport height, position) is the single
// source of truth for the scroll offset: nothing else stores it.

/// The body's window class. Tests get their own name (rule 9).
#[cfg(test)]
const BODY_CLASS_NAME: &str = "Wingman.Settings.Body.Test.5c1d8e03";
#[cfg(not(test))]
const BODY_CLASS_NAME: &str = "Wingman.Settings.Body.c41f0a7e";

static BODY_CLASS_INIT: std::sync::Once = std::sync::Once::new();
static BODY_CLASS_OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();

fn ensure_body_class_registered(instance: HINSTANCE) -> bool {
    BODY_CLASS_INIT.call_once(|| {
        let class_name = wide_z(BODY_CLASS_NAME);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: Default::default(),
            lpfnWndProc: Some(body_wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: Default::default(),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW) }.unwrap_or_default(),
            hbrBackground: windows::Win32::Graphics::Gdi::HBRUSH(
                (windows::Win32::Graphics::Gdi::COLOR_BTNFACE.0 as isize + 1) as *mut c_void,
            ),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hIconSm: Default::default(),
        };
        let _ = BODY_CLASS_OK.set(unsafe { RegisterClassExW(&wc) } != 0);
    });
    BODY_CLASS_OK.get().copied().unwrap_or(false)
}

/// Creates the (empty) body under `parent`, sized to the client area above
/// the footer. The caller adds the controls and later lays them out.
fn create_body(parent: HWND, instance: HINSTANCE, dpi: u32) -> HWND {
    if !ensure_body_class_registered(instance) {
        return HWND(std::ptr::null_mut());
    }
    let mut rc = windows::Win32::Foundation::RECT::default();
    let (w, h) = if unsafe { GetClientRect(parent, &mut rc) }.is_ok() {
        (rc.right - rc.left, rc.bottom - rc.top)
    } else {
        (to_px(WIN_W_DP, dpi), to_px(WIN_H_DP, dpi))
    };
    let class_name = wide_z(BODY_CLASS_NAME);
    unsafe {
        CreateWindowExW(
            WS_EX_CONTROLPARENT,
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WS_CHILD | WS_VISIBLE | WS_VSCROLL,
            0,
            0,
            w.max(1),
            (h - to_px(FOOTER_H_DP, dpi)).max(1),
            Some(parent),
            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                ID_BODY as *mut c_void,
            )),
            Some(instance),
            None,
        )
    }
    .unwrap_or(HWND(std::ptr::null_mut()))
}

/// The body forwards what its controls tell their parent (button clicks, the
/// trackbar) up to the settings window, and runs its own scrollbar.
unsafe extern "system" fn body_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND | WM_HSCROLL => {
            let parent = GetParent(hwnd).unwrap_or_default();
            SendMessageW(parent, msg, Some(wparam), Some(lparam))
        }
        WM_VSCROLL => {
            let si = body_scroll_info(hwnd);
            if si.nPage != 0 {
                let dpi = GetDpiForWindow(hwnd).max(1);
                let max = max_scroll(si.nMax + 1, si.nPage as i32);
                let target = scroll_request_target(
                    (wparam.0 & 0xFFFF) as u32,
                    si.nPos,
                    si.nTrackPos,
                    to_px(ROW_H, dpi),
                    si.nPage as i32,
                    max,
                );
                scroll_body_to(hwnd, target);
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// The body's scroll state; `nPage == 0` means no range has been set yet.
unsafe fn body_scroll_info(body: HWND) -> SCROLLINFO {
    let mut si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_TRACKPOS,
        ..Default::default()
    };
    let _ = GetScrollInfo(body, SB_VERT, &mut si);
    si
}

/// Gives the body's scrollbar its range (the content height), page (the
/// viewport height) and position, all in pixels. The bar stays visible but
/// disabled when nothing overflows, so the body's client width never changes.
unsafe fn set_body_scroll(body: HWND, content_px: i32, view_px: i32, pos: i32) {
    let si = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_RANGE | SIF_PAGE | SIF_POS | SIF_DISABLENOSCROLL,
        nMin: 0,
        nMax: (content_px - 1).max(0),
        nPage: view_px.max(1) as u32,
        nPos: pos,
        nTrackPos: 0,
    };
    SetScrollInfo(body, SB_VERT, &si, true);
}

/// The width of a vertical scrollbar at `dpi`.
fn scrollbar_width_px(dpi: u32) -> i32 {
    unsafe { GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) }.max(0)
}

/// Scrolls the body to `target` (limited to its range) by moving every one of
/// its child controls, hidden ones too, so a later show lands in place.
/// `ScrollWindowEx` is not used: it only moves children that intersect the
/// viewport, which would leave the off-screen ones behind.
unsafe fn scroll_body_to(body: HWND, target: i32) {
    let si = body_scroll_info(body);
    if si.nPage == 0 {
        return;
    }
    let new = clamp_scroll(target, si.nMax + 1, si.nPage as i32);
    if new == si.nPos {
        return;
    }
    let pos = SCROLLINFO {
        cbSize: std::mem::size_of::<SCROLLINFO>() as u32,
        fMask: SIF_POS,
        nPos: new,
        ..Default::default()
    };
    SetScrollInfo(body, SB_VERT, &pos, true);

    let dy = si.nPos - new;
    SendMessageW(body, WM_SETREDRAW, Some(WPARAM(0)), None);
    let mut cur = GetWindow(body, GW_CHILD).ok();
    while let Some(c) = cur {
        let mut r = windows::Win32::Foundation::RECT::default();
        if GetWindowRect(c, &mut r).is_ok() {
            let mut pt = windows::Win32::Foundation::POINT {
                x: r.left,
                y: r.top,
            };
            if ScreenToClient(body, &mut pt).as_bool() {
                let _ = SetWindowPos(
                    c,
                    None,
                    pt.x,
                    pt.y + dy,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
        }
        cur = GetWindow(c, GW_HWNDNEXT).ok();
    }
    SendMessageW(body, WM_SETREDRAW, Some(WPARAM(1)), None);
    let _ = RedrawWindow(
        Some(body),
        None,
        None,
        RDW_ERASE | RDW_INVALIDATE | RDW_ALLCHILDREN | RDW_UPDATENOW,
    );
}

/// Scrolls the body just enough to show `ctrl` (a control in the body, or one
/// of its own children such as a combo's edit), with a little room around it.
unsafe fn ensure_visible(body: HWND, ctrl: HWND) {
    let si = body_scroll_info(body);
    if si.nPage == 0 || ctrl == body || !IsChild(body, ctrl).as_bool() {
        return;
    }
    // Walk up to the control that is the body's direct child.
    let mut c = ctrl;
    loop {
        match GetParent(c) {
            Ok(p) if p == body => break,
            Ok(p) => c = p,
            Err(_) => return,
        }
    }
    let mut r = windows::Win32::Foundation::RECT::default();
    if GetWindowRect(c, &mut r).is_err() {
        return;
    }
    let mut top = windows::Win32::Foundation::POINT {
        x: r.left,
        y: r.top,
    };
    let mut bottom = windows::Win32::Foundation::POINT {
        x: r.left,
        y: r.bottom,
    };
    if !ScreenToClient(body, &mut top).as_bool() || !ScreenToClient(body, &mut bottom).as_bool() {
        return;
    }
    // Room above for the group's caption (and the window's top margin for the
    // first group), a little below.
    let dpi = GetDpiForWindow(body).max(1);
    let pad_top = to_px(GROUP_LABEL_TOP + MARGIN, dpi);
    let pad_bottom = to_px(ROW_GAP, dpi);
    let new = scroll_into_view(
        si.nPos,
        si.nMax + 1,
        si.nPage as i32,
        top.y + si.nPos - pad_top,
        bottom.y + si.nPos + pad_bottom,
    );
    scroll_body_to(body, new);
}

/// Whether keyboard focus moved to a control inside the body: scrolls it into
/// view. Called by the message loop whenever the focused window changes, which
/// covers Tab, Shift+Tab, mnemonics and clicks without a timer.
unsafe fn follow_focus(main: HWND, focus: HWND) {
    if let Ok(body) = GetDlgItem(Some(main), ID_BODY) {
        ensure_visible(body, focus);
    }
}

/// Whether `message` is a mouse button message (client or non-client).
fn is_mouse_message(message: u32) -> bool {
    // WM_LBUTTONDOWN..WM_MBUTTONDBLCLK, WM_NCLBUTTONDOWN..WM_NCMBUTTONDBLCLK.
    (0x0201..=0x0209).contains(&message) || (0x00A1..=0x00A9).contains(&message)
}

/// The prompt keeps the wheel when the pointer is over it; where the focus is
/// does not matter.
fn wheel_is_over_prompt(under_cursor: HWND, prompt_edit: HWND) -> bool {
    under_cursor == prompt_edit
}

/// Whether `ctrl` is a combo with its list open (the list scrolls itself).
unsafe fn wheel_over_open_combo(ctrl: HWND) -> bool {
    SendMessageW(ctrl, CB_GETDROPPEDSTATE, None, None).0 != 0
}

/// Turns a mouse wheel message for the settings window into body scrolling.
/// Returns true when it was used. Left alone: a wheel over the Check my work
/// prompt (decided by the window under the cursor, not the focused one: it
/// scrolls its text) and a wheel while a combo's list is open (the list
/// scrolls itself).
unsafe fn scroll_with_wheel(main: HWND, msg: &MSG, prompt_edit: HWND) -> bool {
    if msg.message != WM_MOUSEWHEEL {
        return false;
    }
    // The wheel's lParam is the pointer in screen coordinates.
    let pt = windows::Win32::Foundation::POINT {
        x: (msg.lParam.0 & 0xFFFF) as u16 as i16 as i32,
        y: ((msg.lParam.0 >> 16) & 0xFFFF) as u16 as i16 as i32,
    };
    if wheel_is_over_prompt(WindowFromPoint(pt), prompt_edit) || wheel_over_open_combo(msg.hwnd) {
        return false;
    }
    let Ok(body) = GetDlgItem(Some(main), ID_BODY) else {
        return false;
    };
    let si = body_scroll_info(body);
    if si.nPage == 0 {
        return false;
    }
    let delta = ((msg.wParam.0 >> 16) & 0xFFFF) as u16 as i16 as i32;
    scroll_body_to(
        body,
        si.nPos + wheel_scroll_px(delta, GetDpiForWindow(main).max(1)),
    );
    true
}

fn prompt_toggle_label(expanded: bool) -> &'static str {
    if expanded {
        "Hide Check my work &prompt"
    } else {
        "Show Check my work &prompt"
    }
}

fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_text(hwnd: HWND, s: &str) {
    if hwnd.0.is_null() {
        return;
    }
    let w = wide_z(s);
    unsafe {
        let _ = SetWindowTextW(hwnd, PCWSTR(w.as_ptr()));
    }
}

fn get_text(hwnd: HWND) -> String {
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

/// The control `id` of the settings window `hwnd`: a direct child (the
/// footer) or one inside the scrolling body (#481).
fn get_dlg_item(hwnd: HWND, id: i32) -> Option<HWND> {
    unsafe {
        if let Ok(h) = GetDlgItem(Some(hwnd), id) {
            return Some(h);
        }
        let body = GetDlgItem(Some(hwnd), ID_BODY).ok()?;
        GetDlgItem(Some(body), id).ok()
    }
}

fn combo_selected_text(hwnd: HWND, id: i32) -> String {
    get_dlg_item(hwnd, id).map(get_text).unwrap_or_default()
}

fn set_checkbox(hwnd: HWND, on: bool) {
    let state = if on { BST_CHECKED } else { BST_UNCHECKED };
    unsafe {
        SendMessageW(hwnd, BM_SETCHECK, Some(WPARAM(state as usize)), None);
    }
}

fn checkbox_checked(hwnd: HWND, id: i32) -> bool {
    get_dlg_item(hwnd, id)
        .map(|h| unsafe { SendMessageW(h, BM_GETCHECK, None, None).0 as i32 == BST_CHECKED })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Font
// ---------------------------------------------------------------------------

unsafe extern "system" fn record_font_found(
    _logfont: *const LOGFONTW,
    _metric: *const TEXTMETRICW,
    _font_type: u32,
    lparam: LPARAM,
) -> i32 {
    unsafe {
        *(lparam.0 as *mut bool) = true;
    }
    0 // stop after the first match -- existence is all this needs
}

/// #346: true if a font face with exactly this name is installed. Used by
/// [`build_font`]'s rare fallback path (both `SystemParametersInfoForDpi`
/// and `SystemParametersInfoW` failing) to check "Segoe UI Variable Text"
/// is actually present before naming it, rather than trusting
/// `CreateFontIndirectW` to silently substitute something reasonable if it
/// isn't -- GDI's substitute for a missing face need not resemble either
/// Segoe face.
fn font_face_exists(name: &str) -> bool {
    unsafe {
        let hdc = GetDC(None);
        if hdc.is_invalid() {
            return false;
        }
        let mut lf = LOGFONTW {
            // DEFAULT_CHARSET: match the face regardless of charset.
            lfCharSet: windows::Win32::Graphics::Gdi::FONT_CHARSET(1),
            ..Default::default()
        };
        for (i, u) in name.encode_utf16().enumerate() {
            if i < lf.lfFaceName.len() {
                lf.lfFaceName[i] = u;
            }
        }
        let mut found = false;
        EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(record_font_found),
            LPARAM(std::ptr::addr_of_mut!(found) as isize),
            0,
        );
        ReleaseDC(None, hdc);
        found
    }
}

/// A single dialog font derived from the shell's message font, same source
/// `card.rs` uses for its own fonts, just without the headline/detail split
/// this window has no need for.
fn build_font(dpi: u32) -> HFONT {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };
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

        let mut lf = if got {
            ncm.lfMessageFont
        } else {
            let mut lf = LOGFONTW {
                lfHeight: -12,
                ..Default::default()
            };
            // #346: prefer the newer variable-weight face when it's actually
            // installed (Windows 11), falling back to plain "Segoe UI"
            // (present since Windows 7) rather than trusting GDI's own
            // silent substitute, which need not be either.
            let face = if font_face_exists("Segoe UI Variable Text") {
                "Segoe UI Variable Text"
            } else {
                "Segoe UI"
            };
            for (i, u) in face.encode_utf16().enumerate() {
                if i < lf.lfFaceName.len() {
                    lf.lfFaceName[i] = u;
                }
            }
            lf.lfHeight = (lf.lfHeight as f32 * dpi as f32 / 96.0).round() as i32;
            lf
        };
        lf.lfWeight = FW_NORMAL.0 as i32;

        let f = CreateFontIndirectW(&lf);
        if f.0.is_null() {
            HFONT(
                windows::Win32::Graphics::Gdi::GetStockObject(
                    windows::Win32::Graphics::Gdi::DEFAULT_GUI_FONT,
                )
                .0,
            )
        } else {
            f
        }
    }
}

// ---------------------------------------------------------------------------
// UI construction
// ---------------------------------------------------------------------------

/// A tiny cursor for stacking rows top-to-bottom inside one group's content
/// area. All coordinates are logical (96dpi) units, scaled at creation time.
struct Rows {
    x: i32,
    y: i32,
    w: i32,
}

impl Rows {
    fn new(x: i32, y: i32, w: i32) -> Self {
        Rows { x, y, w }
    }

    fn advance(&mut self) {
        self.y += ROW_H + ROW_GAP;
    }
}

struct Ctx {
    /// Where [`Ctx::create`] puts the next control: the scrolling body, then
    /// the window itself for the pinned footer (#481).
    parent: std::cell::Cell<HWND>,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
    /// Every child created through [`Ctx::create`], with its design (96dpi)
    /// rect, so a later `WM_DPICHANGED` can re-lay-out each one (#233).
    children: std::cell::RefCell<Vec<ChildLayout>>,
}

impl Ctx {
    #[allow(clippy::too_many_arguments)]
    fn create(
        &self,
        class: &str,
        text: &str,
        style: u32,
        ex_style: u32,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        let class_w = wide_z(class);
        let text_w = wide_z(text);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(ex_style),
                PCWSTR(class_w.as_ptr()),
                PCWSTR(text_w.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    style | WS_CHILD.0 | WS_VISIBLE.0,
                ),
                to_px(x, self.dpi),
                to_px(y, self.dpi),
                to_px(w, self.dpi),
                to_px(h, self.dpi),
                Some(self.parent.get()),
                Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                    id as *mut c_void,
                )),
                Some(self.instance),
                None,
            )
        };
        if let Ok(hwnd) = hwnd {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(self.font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            self.children.borrow_mut().push(ChildLayout {
                hwnd,
                x,
                y,
                w,
                h,
                anchor: Anchor::Fixed,
            });
            hwnd
        } else {
            HWND(std::ptr::null_mut())
        }
    }
}

impl Ctx {
    /// Marks an already-created child as one of the elastic pieces of the
    /// Prompt/footer region (see [`Anchor`]).
    fn set_anchor(&self, hwnd: HWND, anchor: Anchor) {
        if let Some(c) = self
            .children
            .borrow_mut()
            .iter_mut()
            .find(|c| c.hwnd == hwnd)
        {
            c.anchor = anchor;
        }
    }
}

const WC_STATIC: &str = "STATIC";
const WC_EDIT: &str = "EDIT";
const WC_BUTTON: &str = "BUTTON";
const WC_COMBOBOX: &str = "COMBOBOX";

fn combo_add(hwnd: HWND, item: &str) {
    let w = wide_z(item);
    unsafe {
        SendMessageW(hwnd, CB_ADDSTRING, None, Some(LPARAM(w.as_ptr() as isize)));
    }
}

fn combo_select(hwnd: HWND, text: &str) {
    let w = wide_z(text);
    unsafe {
        SendMessageW(
            hwnd,
            CB_SELECTSTRING,
            Some(WPARAM((-1i32) as usize)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
    }
}

fn combo_select_index(hwnd: HWND, index: i32) {
    unsafe {
        SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(index as usize)), None);
    }
}

/// Populates a model combo, ensuring the active model is present even if it
/// is missing from `models` (mirrors the tray submenu's own comment on
/// `ProviderConfig::models` in `config.rs`: the active model is always
/// shown even if it has fallen out of the configured list).
fn fill_model_combo(hwnd: HWND, models: &[String], active: &str) {
    let mut list: Vec<&str> = models.iter().map(|s| s.as_str()).collect();
    if !active.is_empty() && !list.contains(&active) {
        list.insert(0, active);
    }
    for m in &list {
        combo_add(hwnd, m);
    }
    combo_select(hwnd, active);
    if unsafe { SendMessageW(hwnd, CB_GETCURSEL, None, None).0 } < 0 && !list.is_empty() {
        combo_select_index(hwnd, 0);
    }
}

/// The suffix the Ollama model combo adds to a model that can read images.
const VISION_BADGE: &str = " (vision)";

/// `text` without a trailing [`VISION_BADGE`]: the model name to save.
fn strip_vision_badge(text: &str) -> &str {
    text.strip_suffix(VISION_BADGE).unwrap_or(text)
}

/// The Ollama model combo's items (#183) for the locally pulled `models`,
/// and the index to select. Vision models carry [`VISION_BADGE`], decided by
/// `ollama_admin::vision_from_tags_entry`. A `current` model that is not
/// pulled is kept at the top, unbadged, so a hand-edited value is never
/// silently swapped for another model.
fn ollama_model_entries(models: &[ollama_admin::TagsModel], current: &str) -> (Vec<String>, usize) {
    let mut labels: Vec<String> = models
        .iter()
        .map(|m| {
            if ollama_admin::vision_from_tags_entry(m) {
                format!("{}{VISION_BADGE}", m.name)
            } else {
                m.name.clone()
            }
        })
        .collect();
    let mut selected = models.iter().position(|m| m.name == current);
    if selected.is_none() && !current.is_empty() {
        labels.insert(0, current.to_string());
        selected = Some(0);
    }
    (labels, selected.unwrap_or(0))
}

/// Fills the Ollama model combo from the pulled models, keeping whatever is
/// selected now (#183). Called on the UI thread when the background fetch
/// reports in.
fn apply_ollama_models(hwnd: HWND, models: &[ollama_admin::TagsModel]) {
    let Some(combo) = get_dlg_item(hwnd, ID_OLLAMA_MODEL) else {
        return;
    };
    let shown = combo_selected_text(hwnd, ID_OLLAMA_MODEL);
    let (labels, selected) = ollama_model_entries(models, strip_vision_badge(shown.trim()));
    unsafe {
        SendMessageW(combo, CB_RESETCONTENT, None, None);
    }
    for label in &labels {
        combo_add(combo, label);
    }
    if !labels.is_empty() {
        combo_select_index(combo, selected as i32);
    }
}

/// The model combo's items for a cloud provider (#380): the live `fetched`
/// list merged with the `configured` one, and the index to select. The
/// `current` model is never dropped: when neither list has it, it is kept at
/// the top and stays selected, so a hand-edited value is never swapped.
fn cloud_model_entries(
    fetched: &[String],
    configured: &[String],
    current: &str,
) -> (Vec<String>, usize) {
    let mut labels = crate::provider::model_list::merge(fetched, configured);
    let mut selected = labels.iter().position(|m| m == current);
    if selected.is_none() && !current.is_empty() {
        labels.insert(0, current.to_string());
        selected = Some(0);
    }
    (labels, selected.unwrap_or(0))
}

/// Fills a cloud provider's model combo from its live list (#380), keeping
/// whatever is selected now. Called on the UI thread when the fetch reports in.
fn apply_cloud_models(hwnd: HWND, combo_id: i32, fetched: &[String], configured: &[String]) {
    let Some(combo) = get_dlg_item(hwnd, combo_id) else {
        return;
    };
    let shown = combo_selected_text(hwnd, combo_id);
    let (labels, selected) = cloud_model_entries(fetched, configured, shown.trim());
    unsafe {
        SendMessageW(combo, CB_RESETCONTENT, None, None);
    }
    for label in &labels {
        combo_add(combo, label);
    }
    if !labels.is_empty() {
        combo_select_index(combo, selected as i32);
    }
}

/// Posted to the Settings window when a cloud model-list fetch has stored
/// its result in [`SettingsInner::cloud_models`]; `wparam` is the vendor's
/// index in [`CLOUD_VENDORS`].
const WM_CLOUD_MODELS: u32 = 0x8000 + 42; // WM_APP + 42

/// The cloud vendors Settings refreshes, in the order [`CloudModels`] stores
/// them.
const CLOUD_VENDORS: [crate::provider::model_list::Vendor; 3] = [
    crate::provider::model_list::Vendor::OpenAi,
    crate::provider::model_list::Vendor::Anthropic,
    crate::provider::model_list::Vendor::Gemini,
];

/// The fetched cloud model lists, one slot per [`CLOUD_VENDORS`] entry,
/// shared with the worker threads. A slot stays `None` until its fetch
/// succeeds.
type CloudModels = std::sync::Arc<std::sync::Mutex<[Option<Vec<String>>; 3]>>;

/// Asks each cloud provider that has a key for its live model list, one
/// worker thread per provider, so a slow or hung vendor never holds up the
/// Settings window (#380). Nothing runs in Offline mode, so no cloud request
/// is even attempted. A failure keeps the built-in list and leaves one line
/// in the diagnostics report; it never shows a card, since the user did not
/// ask for anything.
fn spawn_cloud_models_fetch(
    hwnd: HWND,
    config: &Config,
    slots: CloudModels,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if crate::mode::is_offline_now() {
        return;
    }
    let keys = [
        &config.providers.openai.api_key,
        &config.providers.anthropic.api_key,
        &config.providers.gemini.api_key,
    ];
    for (index, key) in keys.into_iter().enumerate() {
        if key.trim().is_empty() || key == crate::config::UNREADABLE_KEY_MARKER {
            continue;
        }
        let key = key.clone();
        let vendor = CLOUD_VENDORS[index];
        let slots = slots.clone();
        let alive = alive.clone();
        let hwnd_raw = hwnd.0 as isize;
        let _ = std::thread::Builder::new()
            .name("settings-cloud-models".into())
            .spawn(move || {
                let ids = match crate::provider::model_list::fetch(vendor, &key) {
                    Ok(ids) => ids,
                    Err(_) => {
                        // No error text: it can carry a vendor body, and the
                        // report is pasted into public issues. The egress
                        // log has the failure.
                        crate::diagnostics::note_warning(&format!(
                            "Live model list unavailable for {}: using the built-in list",
                            vendor.tag()
                        ));
                        return;
                    }
                };
                if let Ok(mut guard) = slots.lock() {
                    guard[index] = Some(ids);
                }
                post_while_open(&alive, || unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_raw as *mut c_void)),
                        WM_CLOUD_MODELS,
                        WPARAM(index),
                        LPARAM(0),
                    );
                });
            });
    }
}

/// Runs `post` only while the Settings window is still open (#569). `alive`
/// is cleared on the UI thread in `WM_DESTROY`, before Windows can hand the
/// window's handle value to another window, so a worker that finishes after
/// the window closed does not post. A worker that checked the flag just
/// before `WM_DESTROY` can still post once; that post goes to a dead handle
/// and fails harmlessly unless the value was reused in that instant.
/// Returns whether it posted.
fn post_while_open(alive: &std::sync::atomic::AtomicBool, post: impl FnOnce()) -> bool {
    if alive.load(Ordering::SeqCst) {
        post();
        true
    } else {
        false
    }
}

/// Posted to the Settings window when the background Ollama model fetch has
/// stored its result in [`SettingsInner::ollama_tags`].
const WM_OLLAMA_MODELS: u32 = 0x8000 + 41; // WM_APP + 41

/// The fetched model list, shared with the worker thread. `None` until it
/// reports in, and left `None` when the server was not reachable.
type OllamaTags = std::sync::Arc<std::sync::Mutex<Option<Vec<ollama_admin::TagsModel>>>>;

/// Asks Ollama for its pulled models on a worker thread, so a slow or hung
/// server never holds up the Settings window (#183). The result goes into
/// `slot`, then [`WM_OLLAMA_MODELS`] wakes the window, but only while the
/// window is still open (`alive`, see [`post_while_open`]).
fn spawn_ollama_models_fetch(
    hwnd: HWND,
    base_url: String,
    slot: OllamaTags,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let hwnd_raw = hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("settings-ollama-models".into())
        .spawn(move || {
            let Ok(models) = ollama_admin::list_tags(&base_url) else {
                return;
            };
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(models);
            }
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut c_void)),
                    WM_OLLAMA_MODELS,
                    WPARAM(0),
                    LPARAM(0),
                );
            });
        });
}

/// Posted to the Settings window when the background Ollama status probe has
/// stored its result in [`SettingsInner::ollama_status`].
const WM_OLLAMA_STATUS: u32 = 0x8000 + 43; // WM_APP + 43

/// The probe's result, shared with the worker thread. `None` until it reports.
type OllamaStatusSlot = std::sync::Arc<std::sync::Mutex<Option<OllamaSection>>>;

/// Works out the Ollama group's text and button on a worker thread (#527,
/// #568): the listener check, the optional HTTP calls and the install
/// search (which can touch a slow network share on `PATH`) must not hold up
/// opening Settings. One shot per open, no timer. The result goes into
/// `slot`, then [`WM_OLLAMA_STATUS`] wakes the window, but only while the
/// window is still open (`alive`, see [`post_while_open`]).
fn spawn_ollama_status_probe(
    hwnd: HWND,
    cfg: OllamaConfig,
    ollama_configured: bool,
    slot: OllamaStatusSlot,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let hwnd_raw = hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("settings-ollama-status".into())
        .spawn(move || {
            let (status, listening) = ollama_status_line(&cfg, ollama_configured);
            // Only searched for when nothing listens: it is not needed
            // otherwise.
            let installed = !listening && ollama_installed();
            let section = ollama_section(status, listening, installed, ollama_configured);
            if let Ok(mut guard) = slot.lock() {
                *guard = Some(section);
            }
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut c_void)),
                    WM_OLLAMA_STATUS,
                    WPARAM(0),
                    LPARAM(0),
                );
            });
        });
}

/// Shows the probe's result: the text, and the link button with its label
/// (hidden when there is no link).
fn apply_ollama_section(inner: &mut SettingsInner, section: &OllamaSection) {
    if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
        set_text(status, &section.text);
    }
    inner.ollama_link_url = section.link.map(|(_, url)| url);
    if let Some(link) = get_dlg_item(inner.hwnd, ID_OLLAMA_LINK) {
        if let Some((label, _)) = section.link {
            set_text(link, label);
        }
        let cmd = if section.link.is_some() {
            SW_SHOW
        } else {
            SW_HIDE
        };
        unsafe {
            let _ = ShowWindow(link, cmd);
        }
    }
}

/// #188: total budget for the (at most two) optional HTTP calls
/// `ollama_status_line` can make (`/api/ps` for GPU/CPU, `/api/tags` for
/// vision counts). Tighter than `ollama_admin`'s general discovery timeout
/// (5s each, 10s worst case combined) because this runs synchronously on
/// the UI thread before the Settings window is shown -- a hung/wedged
/// server must not visibly stall opening Settings for more than a couple
/// of seconds.
const OLLAMA_STATUS_HTTP_BUDGET: Duration = Duration::from_secs(3);

/// #188: whether the status line should make its two optional HTTP calls at
/// all, on top of the Win32-only listener check. Pure so the gate itself is
/// unit-tested without a socket.
///
/// Only when BOTH:
/// - `ollama_configured` -- Ollama is actually in `providers.order`, this
///   codebase's own signal that the user opted in (see `config.rs`'s
///   `Providers::default` and `mode::should_probe_ollama`, which gates
///   Auto mode's own reachability probe the same way). A user who never
///   touched Ollama but happens to have something else listening on the
///   configured port must see zero added HTTP calls or latency here.
/// - the Win32-only health check found a real, non-stock-tray-app server
///   listening -- there is nothing useful to ask a not-listening server,
///   and the stock tray app is already known to be CPU-only.
fn should_query_ollama_details(ollama_configured: bool, health: &OllamaHealth) -> bool {
    ollama_configured
        && matches!(
            health,
            OllamaHealth::Listening {
                kind: ListenerKind::Other,
                ..
            }
        )
}

/// #343: where "Get Ollama" goes.
const OLLAMA_DOWNLOAD_URL: &str = "https://ollama.com/download";
/// #343: where "Local models" goes (the Ollama section of `docs/providers.md`).
const OLLAMA_DOCS_URL: &str =
    "https://github.com/Raaif-Yousuf/Wingman/blob/master/docs/providers.md#ollama";

/// What the Ollama group shows (#343): the text, and an optional button.
#[derive(Debug, Clone, PartialEq, Eq)]
struct OllamaSection {
    text: String,
    /// `(button label, url)`.
    link: Option<(&'static str, &'static str)>,
}

/// Turns the status line into an actionable section (#343). `status` is the
/// existing health/GPU line, `listening` whether anything answers on the
/// port, `installed` whether an Ollama install was found, `configured`
/// whether Ollama is already in `providers.order`.
/// - Not installed: say what it is and offer the download.
/// - Installed, not running: say what to do (no link; nothing to open).
/// - Running: the status line, plus a pointer to the docs when Ollama is not
///   set up as a provider yet.
fn ollama_section(
    status: String,
    listening: bool,
    installed: bool,
    configured: bool,
) -> OllamaSection {
    if !listening {
        return if installed {
            OllamaSection {
                text: "Ollama is installed but not running. Start Ollama, then reopen Settings."
                    .to_string(),
                link: None,
            }
        } else {
            OllamaSection {
                text: "Ollama runs AI models on this PC, free and offline.".to_string(),
                link: Some(("Get Ollama", OLLAMA_DOWNLOAD_URL)),
            }
        };
    }
    let text = if configured {
        status
    } else {
        join_within(
            OLLAMA_TEXT_BUDGET,
            &[&status, "To use it, see Local models."],
        )
    };
    OllamaSection {
        text,
        link: Some(("Local models", OLLAMA_DOCS_URL)),
    }
}

/// Where an Ollama install would be (#343): the default per-user install
/// folder, then each `PATH` entry. Pure; [`ollama_installed`] checks disk.
fn ollama_install_candidates(
    local_app_data: Option<&str>,
    path_var: Option<&str>,
) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Some(l) = local_app_data.filter(|l| !l.is_empty()) {
        out.push(std::path::Path::new(l).join(r"Programs\Ollama\ollama.exe"));
    }
    if let Some(p) = path_var {
        // `PATH` entries may be wrapped in quotes (#527).
        for dir in p
            .split(';')
            .map(|d| d.trim().trim_matches('"'))
            .filter(|d| !d.is_empty())
        {
            out.push(std::path::Path::new(dir).join("ollama.exe"));
        }
    }
    out
}

fn ollama_installed() -> bool {
    let local = std::env::var("LOCALAPPDATA").ok();
    let path = std::env::var("PATH").ok();
    ollama_install_candidates(local.as_deref(), path.as_deref())
        .iter()
        .any(|p| p.is_file())
}

/// Opens `url` in the default browser. `false` when the shell refused
/// (`ShellExecuteW` returns a value above 32 on success).
fn open_url(url: &str) -> bool {
    let wide = wide_z(url);
    let r = unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            PCWSTR(wide.as_ptr()),
            None,
            None,
            windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r.0 as isize > 32
}

/// What the Ollama status shows until the probe reports (#527, #568).
const OLLAMA_CHECKING_TEXT: &str = "Checking Ollama...";

/// The most characters the two-row Ollama status label shows without
/// clipping, with room to spare for word wrapping (#527). Optional parts
/// that would push the text past it are left out (see [`join_within`]).
const OLLAMA_TEXT_BUDGET: usize = 140;

/// Longest listener path shown in the status, in characters.
const OLLAMA_PATH_MAX_CHARS: usize = 60;

/// `s` cut to at most `max` characters by replacing the middle with `...`,
/// so a long path keeps its start and its file name. Cuts on character
/// boundaries.
fn shorten_middle(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    if max <= 3 {
        return chars.iter().take(max).collect();
    }
    let keep = max - 3;
    let head = keep.div_ceil(2);
    let tail = keep - head;
    let mut out: String = chars[..head].iter().collect();
    out.push_str("...");
    out.extend(chars[chars.len() - tail..].iter());
    out
}

/// Joins the non-empty `parts` with spaces: the first always, each later one
/// only if the result stays within `budget` characters. A part that does not
/// fit is skipped, and a shorter later one can still go in.
fn join_within(budget: usize, parts: &[&str]) -> String {
    let mut out = String::new();
    for part in parts.iter().filter(|p| !p.is_empty()) {
        if out.is_empty() {
            out.push_str(part);
        } else if out.chars().count() + 1 + part.chars().count() <= budget {
            out.push(' ');
            out.push_str(part);
        }
    }
    out
}

/// [`OllamaHealth::message`] with a long listener path shortened, so the
/// message fits the status label.
fn health_message(health: &OllamaHealth) -> String {
    match health {
        OllamaHealth::Listening {
            pid,
            image_path,
            kind,
        } => OllamaHealth::Listening {
            pid: *pid,
            image_path: shorten_middle(image_path, OLLAMA_PATH_MAX_CHARS),
            kind: *kind,
        }
        .message(),
        other => other.message(),
    }
}

/// Combines #15's health check with #14's GPU/CPU indicator into the one
/// status line Settings shows for Ollama. Runs on a worker thread
/// ([`spawn_ollama_status_probe`], #568) once when Settings opens (AGENTS.md
/// rule 5: discovery happens on demand, never on a timer): the health check
/// is Win32-only (no network at all) and always runs; the two HTTP calls it
/// can make (`/api/ps`, `/api/tags`) only run when
/// [`should_query_ollama_details`] says so (#188), and are bounded by
/// [`OLLAMA_STATUS_HTTP_BUDGET`] in total when they do.
fn ollama_status_line(cfg: &OllamaConfig, ollama_configured: bool) -> (String, bool) {
    let port = ollama_admin::port_from_base_url(&cfg.base_url).unwrap_or(11434);
    let health = ollama_admin::query_ollama_health(port);
    let health_msg = health_message(&health);
    let listening = matches!(health, OllamaHealth::Listening { .. });

    if !should_query_ollama_details(ollama_configured, &health) {
        return (health_msg, listening);
    }

    let budget_start = std::time::Instant::now();
    let gpu_line = match ollama_admin::ps_with_timeout(&cfg.base_url, OLLAMA_STATUS_HTTP_BUDGET) {
        Ok(entries) => match ollama_admin::gpu_status_for(&entries, &cfg.model) {
            GpuStatus::Gpu => format!("{} is loaded on GPU.", cfg.model),
            GpuStatus::Cpu => format!("{} is loaded on CPU.", cfg.model),
            GpuStatus::NotLoaded => format!("{} is not loaded yet.", cfg.model),
        },
        Err(_) => return (health_msg, listening),
    };

    // Whatever's left of the budget after `/api/ps`, capped at zero rather
    // than going negative -- a slow first call must not hand the second
    // call a needlessly long timeout of its own.
    let remaining = OLLAMA_STATUS_HTTP_BUDGET.saturating_sub(budget_start.elapsed());
    let vision_line = if remaining.is_zero() {
        String::new()
    } else {
        match ollama_admin::list_tags_with_timeout(&cfg.base_url, remaining) {
            Ok(models) if !models.is_empty() => {
                let vision_count = models
                    .iter()
                    .filter(|m| ollama_admin::vision_from_tags_entry(m))
                    .count();
                format!(
                    "{vision_count} of {} local models support vision.",
                    models.len()
                )
            }
            _ => String::new(),
        }
    };

    (
        join_within(OLLAMA_TEXT_BUDGET, &[&health_msg, &gpu_line, &vision_line]),
        listening,
    )
}

/// Builds every child control and returns the prompt edit's `HWND` (the
/// caller needs it to exempt Enter-as-newline from the Enter-submits rule).
#[cfg(test)]
fn build_ui(
    hwnd: HWND,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
    config: &Config,
    win_h_dp: i32,
) -> HWND {
    build_ui_recorded(hwnd, instance, dpi, font, config, win_h_dp).prompt_edit
}

/// What [`build_ui`] built, plus what a later DPI change needs to redo it.
struct BuiltUi {
    prompt_edit: HWND,
    /// The scrolling window the groups live in (#481).
    body: HWND,
    children: Vec<ChildLayout>,
    /// Where the fixed-height groups end, in dp (the Prompt group's top).
    content_top_dp: i32,
    /// The client height (dp) the elastic region was laid out for.
    win_h_dp: i32,
}

fn build_ui_recorded(
    hwnd: HWND,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
    config: &Config,
    win_h_dp: i32,
) -> BuiltUi {
    // #481: created first, so it is first in tab order; the footer controls
    // made last are the last stops.
    let body = create_body(hwnd, instance, dpi);
    let ctx = Ctx {
        parent: std::cell::Cell::new(if body.0.is_null() { hwnd } else { body }),
        instance,
        dpi,
        font,
        children: std::cell::RefCell::new(Vec::new()),
    };
    let content_x = MARGIN;
    let content_w = WIN_W_DP - 2 * MARGIN;

    let mut y = MARGIN;

    // -- Providers ----------------------------------------------------
    let providers_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);

    ctx.create(
        WC_STATIC,
        "Active provider:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let active_combo = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 6,
        ID_ACTIVE_PROVIDER,
    );
    combo_add(active_combo, "ChatGPT (OpenAI)");
    combo_add(active_combo, "Claude (Anthropic)");
    combo_add(active_combo, "Ollama (local)");
    combo_add(active_combo, "Gemini (Google)");
    combo_select_index(
        active_combo,
        choice_from_order(&config.providers.order) as i32,
    );
    r.advance();

    // OpenAI
    ctx.create(
        WC_STATIC,
        "OpenAI API key:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    ctx.create(
        WC_EDIT,
        &key_field_display(&config.providers.openai.api_key),
        (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
        WS_EX_BORDER,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W - 70,
        ROW_H,
        ID_OPENAI_KEY,
    );
    ctx.create(
        WC_BUTTON,
        "Show",
        BS_AUTOCHECKBOX as u32,
        0,
        r.x + r.w - 62,
        r.y,
        62,
        ROW_H,
        ID_OPENAI_SHOW_KEY,
    );
    r.advance();

    let half = (r.w - 12) / 2;
    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, 50, ROW_H, 0);
    let openai_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 50,
        r.y,
        half - 50,
        ROW_H * 8,
        ID_OPENAI_MODEL,
    );
    fill_model_combo(
        openai_model,
        &config.providers.openai.models,
        &config.providers.openai.model,
    );

    ctx.create(
        WC_STATIC,
        "Thinking:",
        0,
        0,
        r.x + half + 12,
        r.y,
        60,
        ROW_H,
        0,
    );
    let openai_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + half + 12 + 60,
        r.y,
        half - 60,
        ROW_H * 4,
        ID_OPENAI_EFFORT,
    );
    for e in EFFORT_LABELS {
        combo_add(openai_effort, e);
    }
    combo_select_with_custom(
        openai_effort,
        &EFFORT_LABELS,
        &effort_display(&config.providers.openai.effort),
    );
    r.advance();

    // Anthropic
    ctx.create(
        WC_STATIC,
        "Anthropic API key:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    ctx.create(
        WC_EDIT,
        &key_field_display(&config.providers.anthropic.api_key),
        (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
        WS_EX_BORDER,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W - 70,
        ROW_H,
        ID_ANTHROPIC_KEY,
    );
    ctx.create(
        WC_BUTTON,
        "Show",
        BS_AUTOCHECKBOX as u32,
        0,
        r.x + r.w - 62,
        r.y,
        62,
        ROW_H,
        ID_ANTHROPIC_SHOW_KEY,
    );
    r.advance();

    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, 50, ROW_H, 0);
    let anthropic_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 50,
        r.y,
        half - 50,
        ROW_H * 8,
        ID_ANTHROPIC_MODEL,
    );
    fill_model_combo(
        anthropic_model,
        &config.providers.anthropic.models,
        &config.providers.anthropic.model,
    );

    ctx.create(
        WC_STATIC,
        "Thinking:",
        0,
        0,
        r.x + half + 12,
        r.y,
        60,
        ROW_H,
        0,
    );
    let anthropic_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + half + 12 + 60,
        r.y,
        half - 60,
        ROW_H * 4,
        ID_ANTHROPIC_EFFORT,
    );
    for e in EFFORT_LABELS {
        combo_add(anthropic_effort, e);
    }
    combo_select_with_custom(
        anthropic_effort,
        &EFFORT_LABELS,
        &effort_display(&config.providers.anthropic.effort),
    );
    r.advance();

    // Gemini (#195): same three rows as the other cloud providers.
    ctx.create(
        WC_STATIC,
        "Gemini API key:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    ctx.create(
        WC_EDIT,
        &key_field_display(&config.providers.gemini.api_key),
        (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
        WS_EX_BORDER,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W - 70,
        ROW_H,
        ID_GEMINI_KEY,
    );
    ctx.create(
        WC_BUTTON,
        "Show",
        BS_AUTOCHECKBOX as u32,
        0,
        r.x + r.w - 62,
        r.y,
        62,
        ROW_H,
        ID_GEMINI_SHOW_KEY,
    );
    r.advance();

    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, 50, ROW_H, 0);
    let gemini_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 50,
        r.y,
        half - 50,
        ROW_H * 8,
        ID_GEMINI_MODEL,
    );
    fill_model_combo(
        gemini_model,
        &config.providers.gemini.models,
        &config.providers.gemini.model,
    );

    ctx.create(
        WC_STATIC,
        "Thinking:",
        0,
        0,
        r.x + half + 12,
        r.y,
        60,
        ROW_H,
        0,
    );
    let gemini_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + half + 12 + 60,
        r.y,
        half - 60,
        ROW_H * 4,
        ID_GEMINI_EFFORT,
    );
    for e in EFFORT_LABELS {
        combo_add(gemini_effort, e);
    }
    combo_select_with_custom(
        gemini_effort,
        &EFFORT_LABELS,
        &effort_display(&config.providers.gemini.effort),
    );
    r.advance();

    // Ollama (#183): no key, only which pulled model to use. The combo starts
    // with the configured model alone; the pulled models are added when the
    // background fetch reports in (see `spawn_ollama_models_fetch`).
    ctx.create(
        WC_STATIC,
        "Ollama model:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let ollama_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 8,
        ID_OLLAMA_MODEL,
    );
    let (labels, selected) = ollama_model_entries(&[], &config.providers.ollama.model);
    for label in &labels {
        combo_add(ollama_model, label);
    }
    if !labels.is_empty() {
        combo_select_index(ollama_model, selected as i32);
    }
    r.advance();

    let providers_bottom = r.y + ROW_H + 8;
    // Created last, once the real extent of what it should enclose is
    // known, so its height doesn't need to be precomputed by hand. This
    // does put it on top in z-order, but a BS_GROUPBOX only ever paints its
    // frame outline and caption text (its interior is left untouched, not
    // filled), and this layout keeps that outline/caption clear of every
    // field it encloses, so nothing ends up visually covered.
    ctx.create(
        WC_BUTTON,
        "Providers",
        BS_GROUPBOX as u32,
        0,
        content_x,
        providers_top,
        content_w,
        providers_bottom - providers_top,
        0,
    );

    y = providers_bottom + GROUP_GAP;

    // -- Ollama status (#14, #15) ----------------------------------------
    // One read-only line: whether anything answers on the configured
    // Ollama port, whether it's the stock tray app's CPU-only server
    // (AGENTS.md's "Stock Ollama's tray app steals port 11434" pitfall),
    // and -- once it's confirmed to be a real server -- whether the
    // configured model is currently loaded on GPU or CPU and how many
    // local models support vision. See `ollama_status_line`.
    //
    // Read-only: choosing Ollama as the active provider and its model is
    // done in the Providers group above (#183). `base_url` stays a
    // config.toml setting.
    let ollama_top = y;
    y += GROUP_LABEL_TOP;
    // The text is a placeholder until the worker reports (#527, #568), so the
    // window never waits on Ollama or the disk. The link button sits at the
    // right of the two text rows, so it adds no height to the window (#481);
    // it is created hidden and shown when the result says it has a link, so
    // the text is always laid out narrower to leave room for it.
    let link_w = OLLAMA_LINK_W + 12;
    ctx.create(
        WC_STATIC,
        OLLAMA_CHECKING_TEXT,
        0,
        0,
        content_x + MARGIN,
        y,
        content_w - 2 * MARGIN - link_w,
        ROW_H * 2,
        ID_OLLAMA_STATUS,
    );
    ctx.create(
        WC_BUTTON,
        "Local models",
        BS_PUSHBUTTON as u32,
        0,
        content_x + content_w - MARGIN - OLLAMA_LINK_W,
        y + (ROW_H * 2 - BTN_H) / 2,
        OLLAMA_LINK_W,
        BTN_H,
        ID_OLLAMA_LINK,
    );
    if let Some(link) = get_dlg_item(hwnd, ID_OLLAMA_LINK) {
        unsafe {
            let _ = ShowWindow(link, SW_HIDE);
        }
    }
    y += ROW_H * 2;
    let ollama_bottom = y + 8;
    ctx.create(
        WC_BUTTON,
        "Ollama",
        BS_GROUPBOX as u32,
        0,
        content_x,
        ollama_top,
        content_w,
        ollama_bottom - ollama_top,
        0,
    );
    y = ollama_bottom + GROUP_GAP;

    // -- Hotkeys --------------------------------------------------------
    let hotkeys_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);
    let change_w = 90;
    let label_w = r.w - change_w - 12;
    ctx.create(
        WC_STATIC,
        &format!("Copilot key: {}", chord_to_string(&config.hotkeys.primary)),
        0,
        0,
        r.x,
        r.y,
        label_w,
        ROW_H,
        ID_PRIMARY_LABEL,
    );
    ctx.create(
        WC_BUTTON,
        "Change...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.x + r.w - change_w,
        r.y,
        change_w,
        ROW_H,
        ID_CHANGE_PRIMARY,
    );
    r.advance();
    ctx.create(
        WC_STATIC,
        &format!(
            "Other shortcut: {}",
            chord_to_string(&config.hotkeys.secondary)
        ),
        0,
        0,
        r.x,
        r.y,
        label_w,
        ROW_H,
        ID_SECONDARY_LABEL,
    );
    ctx.create(
        WC_BUTTON,
        "Change...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.x + r.w - change_w,
        r.y,
        change_w,
        ROW_H,
        ID_CHANGE_SECONDARY,
    );
    r.advance();
    // The learn flow's whole conversation happens on this one line, inline:
    // never a dialog and never a card (rule 7).
    ctx.create(
        WC_STATIC,
        "Click Change, then press the new key.",
        WS_DISABLED.0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        ID_HOTKEY_STATUS,
    );
    r.advance();

    // Autostart state is owned by Windows, not by Config -- read the Run key
    // itself so the box can never disagree with reality.
    let autostart = ctx.create(
        WC_BUTTON,
        "Start with Windows",
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        ID_AUTOSTART,
    );
    set_checkbox(autostart, crate::autostart::is_enabled());
    r.advance();

    let hotkeys_bottom = r.y + 4;
    ctx.create(
        WC_BUTTON,
        "General",
        BS_GROUPBOX as u32,
        0,
        content_x,
        hotkeys_top,
        content_w,
        hotkeys_bottom - hotkeys_top,
        0,
    );
    y = hotkeys_bottom + GROUP_GAP;

    // -- Capture ----------------------------------------------------------
    let capture_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);
    let half = (r.w - 12) / 2;

    ctx.create(
        WC_STATIC,
        "Screenshot detail:",
        0,
        0,
        r.x,
        r.y,
        110,
        ROW_H,
        0,
    );
    let max_edge = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 110,
        r.y,
        half - 110,
        ROW_H * 6,
        ID_MAX_EDGE,
    );
    for v in MAX_EDGE_LABELS {
        combo_add(max_edge, v);
    }
    combo_select_with_custom(
        max_edge,
        &MAX_EDGE_LABELS,
        &max_edge_display(config.capture.max_edge),
    );

    ctx.create(
        WC_STATIC,
        "Screen:",
        0,
        0,
        r.x + half + 12,
        r.y,
        60,
        ROW_H,
        0,
    );
    let monitor = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + half + 12 + 60,
        r.y,
        half - 60,
        ROW_H * 4,
        ID_MONITOR,
    );
    for v in MONITOR_LABELS {
        combo_add(monitor, v);
    }
    combo_select_with_custom(
        monitor,
        &MONITOR_LABELS,
        &monitor_display(&config.capture.monitor),
    );
    r.advance();

    // #341: a one-line hint, since "screenshot detail" alone doesn't say
    // what changes or why it costs more.
    ctx.create(
        WC_STATIC,
        "Higher reads small text better, costs more.",
        0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        0,
    );
    r.advance();

    // #509: capture.redact. The hint is two rows tall so it wraps in words
    // rather than running off the group.
    let redact = ctx.create(
        WC_BUTTON,
        REDACT_LABEL,
        BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        ID_REDACT,
    );
    set_checkbox(redact, config.capture.redact);
    r.advance();
    ctx.create(
        WC_STATIC,
        REDACT_HINT,
        0,
        0,
        r.x + 20,
        r.y,
        r.w - 20,
        ROW_H * 2,
        0,
    );
    r.advance();
    r.advance();
    let capture_bottom = r.y + 4;
    ctx.create(
        WC_BUTTON,
        "Capture",
        BS_GROUPBOX as u32,
        0,
        content_x,
        capture_top,
        content_w,
        capture_bottom - capture_top,
        0,
    );
    y = capture_bottom + GROUP_GAP;

    // -- Card ---------------------------------------------------------
    let card_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);

    ctx.create(
        WC_STATIC,
        "Hide the card after this many seconds (0 = never):",
        0,
        0,
        r.x,
        r.y,
        260,
        ROW_H,
        0,
    );
    ctx.create(
        WC_EDIT,
        &config.ui.card_seconds.to_string(),
        (ES_NUMBER | ES_AUTOHSCROLL) as u32,
        WS_EX_BORDER,
        r.x + 260,
        r.y,
        50,
        ROW_H,
        ID_CARD_SECONDS,
    );
    r.advance();

    ctx.create(WC_STATIC, "Text scale:", 0, 0, r.x, r.y, 70, ROW_H, 0);
    let track = ctx.create(
        TRACKBAR_CLASS_STR,
        "",
        TBS_HORZ as u32,
        0,
        r.x + 70,
        r.y,
        r.w - 70 - 60,
        ROW_H,
        ID_TEXT_SCALE_TRACK,
    );
    unsafe {
        SendMessageW(
            track,
            TBM_SETRANGE,
            Some(WPARAM(1)),
            Some(LPARAM(((200u32) << 16 | 50u32) as isize)),
        );
        SendMessageW(track, TBM_SETLINESIZE, None, Some(LPARAM(5)));
        SendMessageW(track, TBM_SETPAGESIZE, None, Some(LPARAM(10)));
        let pos = (clamp_text_scale(config.ui.text_scale) * 100.0).round() as i32;
        SendMessageW(
            track,
            TBM_SETPOS,
            Some(WPARAM(1)),
            Some(LPARAM(pos as isize)),
        );
    }
    let scale_label = ctx.create(
        WC_STATIC,
        &format!("{:.2}x", clamp_text_scale(config.ui.text_scale)),
        0,
        0,
        r.x + r.w - 55,
        r.y,
        55,
        ROW_H,
        ID_TEXT_SCALE_LABEL,
    );
    let _ = scale_label;
    r.advance();

    // #498: where the card appears. The monitor list is read when Settings
    // opens, so a display plugged in afterwards shows up next time.
    ctx.create(WC_STATIC, "Card position:", 0, 0, r.x, r.y, 110, ROW_H, 0);
    let corner = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 110,
        r.y,
        r.w - 110,
        ROW_H * 7,
        ID_CARD_CORNER,
    );
    for label in CARD_CORNER_LABELS {
        combo_add(corner, label);
    }
    combo_select(corner, card_corner_label(config.ui.card_corner));
    r.advance();

    ctx.create(WC_STATIC, "Card display:", 0, 0, r.x, r.y, 110, ROW_H, 0);
    let card_monitor = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 110,
        r.y,
        r.w - 110,
        ROW_H * 8,
        ID_CARD_MONITOR,
    );
    let monitors = crate::ui::card::monitor_count();
    let monitor_options = card_monitor_options(monitors);
    for (label, _) in &monitor_options {
        combo_add(card_monitor, label);
    }
    let monitor_presets: Vec<&str> = monitor_options.iter().map(|(l, _)| l.as_str()).collect();
    combo_select_with_custom(
        card_monitor,
        &monitor_presets,
        &card_monitor_display(&config.ui.card_monitor, monitors),
    );
    r.advance();

    let card_bottom = r.y + 4;
    ctx.create(
        WC_BUTTON,
        "Card",
        BS_GROUPBOX as u32,
        0,
        content_x,
        card_top,
        content_w,
        card_bottom - card_top,
        0,
    );
    y = card_bottom + GROUP_GAP;

    // -- Forms (#403) -------------------------------------------------
    let forms_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);

    ctx.create(
        WC_STATIC,
        "Require a tick to fill:",
        0,
        0,
        r.x,
        r.y,
        150,
        ROW_H,
        0,
    );
    let require_tick_for = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + 150,
        r.y,
        r.w - 150,
        ROW_H * 4,
        ID_REQUIRE_TICK_FOR,
    );
    for label in REQUIRE_TICK_FOR_LABELS {
        combo_add(require_tick_for, label);
    }
    combo_select(
        require_tick_for,
        require_tick_for_label(config.forms.require_tick_for),
    );
    r.advance();

    let forms_bottom = r.y + 4;
    ctx.create(
        WC_BUTTON,
        "Fill forms",
        BS_GROUPBOX as u32,
        0,
        content_x,
        forms_top,
        content_w,
        forms_bottom - forms_top,
        0,
    );
    y = forms_bottom + GROUP_GAP;

    // -- Prompt (grows to fill remaining space above the button row) ------
    let footer = footer_layout(y, win_h_dp, false);
    let prompt_top = footer.prompt_top;
    let prompt_bottom = footer.prompt_bottom;
    y = prompt_top + GROUP_LABEL_TOP;

    let reset_btn_h = BTN_H;
    let prompt_edit_h = footer.prompt_edit_h;
    let content_top_dp = footer.prompt_top;
    let prompt_edit = ctx.create(
        WC_EDIT,
        &config.ui.prompt,
        (ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32,
        WS_EX_BORDER,
        content_x + MARGIN,
        y,
        content_w - 2 * MARGIN,
        prompt_edit_h,
        ID_PROMPT_EDIT,
    );
    ctx.set_anchor(prompt_edit, Anchor::PromptEdit);
    // #342: starts hidden behind the Advanced toggle; the text is still
    // there for Save to read.
    unsafe {
        let _ = ShowWindow(prompt_edit, SW_HIDE);
    }
    // WS_VSCROLL isn't representable via the `style` u32 alone without
    // pulling in the full WINDOW_STYLE const set; add it directly.
    unsafe {
        let cur = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            prompt_edit,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
        );
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            prompt_edit,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
            cur | WS_VSCROLL.0 as isize,
        );
    }

    let toggle_btn = ctx.create(
        WC_BUTTON,
        prompt_toggle_label(false),
        BS_PUSHBUTTON as u32,
        0,
        content_x + MARGIN,
        footer.reset_btn_y,
        PROMPT_TOGGLE_W,
        reset_btn_h,
        ID_PROMPT_TOGGLE,
    );
    ctx.set_anchor(toggle_btn, Anchor::ResetBtn);
    let reset_btn = ctx.create(
        WC_BUTTON,
        "Reset prompt to default",
        BS_PUSHBUTTON as u32,
        0,
        content_x + MARGIN + PROMPT_TOGGLE_W + 12,
        footer.reset_btn_y,
        200,
        reset_btn_h,
        ID_RESET_PROMPT,
    );
    ctx.set_anchor(reset_btn, Anchor::ResetBtn);
    unsafe {
        let _ = ShowWindow(reset_btn, SW_HIDE);
    }

    let prompt_group = ctx.create(
        WC_BUTTON,
        "Advanced",
        BS_GROUPBOX as u32,
        0,
        content_x,
        prompt_top,
        content_w,
        prompt_bottom - prompt_top,
        0,
    );
    ctx.set_anchor(prompt_group, Anchor::PromptGroup);

    // -- Save / Cancel ------------------------------------------------
    // Pinned footer: a sibling of the body, not inside it, so it never scrolls.
    ctx.parent.set(hwnd);
    let buttons_y = footer.buttons_y;
    // #344: inline "that field is wrong" message, blank until a Save
    // attempt fails validation. Sits to the left of the Save/Cancel row so
    // it never overlaps either the Prompt group or the buttons.
    let validation = ctx.create(
        WC_STATIC,
        "",
        0,
        0,
        content_x + MARGIN,
        buttons_y + (BTN_H - ROW_H) / 2,
        content_w - 2 * MARGIN - 2 * BTN_W - 12 - 12,
        ROW_H,
        ID_VALIDATION_MESSAGE,
    );
    ctx.set_anchor(validation, Anchor::Footer);
    let cancel = ctx.create(
        WC_BUTTON,
        "Cancel",
        (BS_PUSHBUTTON) as u32,
        0,
        WIN_W_DP - MARGIN - BTN_W,
        buttons_y,
        BTN_W,
        BTN_H,
        ID_CANCEL,
    );
    ctx.set_anchor(cancel, Anchor::Footer);
    let save = ctx.create(
        WC_BUTTON,
        "Save",
        (BS_DEFPUSHBUTTON) as u32,
        0,
        WIN_W_DP - MARGIN - BTN_W - 12 - BTN_W,
        buttons_y,
        BTN_W,
        BTN_H,
        ID_SAVE,
    );
    ctx.set_anchor(save, Anchor::Footer);

    // Tab order / grouping: give the first control of each visual group
    // WS_GROUP so arrow-key navigation and Tab-between-groups behave.
    // #346: this used to list only 5 controls, missing the first focusable
    // control of the "General" group (ID_AUTOSTART) and, once #403 added the
    // Forms group, its first control (ID_REQUIRE_TICK_FOR) too -- so
    // arrow-key/group navigation didn't match the visible group boxes.
    for id in [
        ID_ACTIVE_PROVIDER,
        ID_MAX_EDGE,
        ID_CARD_SECONDS,
        ID_REQUIRE_TICK_FOR,
        ID_PROMPT_EDIT,
        ID_SAVE,
        ID_AUTOSTART,
    ] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            add_style(h, WS_GROUP.0);
        }
    }
    for id in [
        ID_ACTIVE_PROVIDER,
        ID_OPENAI_KEY,
        ID_OPENAI_SHOW_KEY,
        ID_OPENAI_MODEL,
        ID_OPENAI_EFFORT,
        ID_ANTHROPIC_KEY,
        ID_ANTHROPIC_SHOW_KEY,
        ID_ANTHROPIC_MODEL,
        ID_ANTHROPIC_EFFORT,
        ID_GEMINI_KEY,
        ID_GEMINI_SHOW_KEY,
        ID_GEMINI_MODEL,
        ID_GEMINI_EFFORT,
        ID_OLLAMA_MODEL,
        ID_MAX_EDGE,
        ID_MONITOR,
        ID_REDACT,
        ID_CARD_SECONDS,
        ID_TEXT_SCALE_TRACK,
        ID_CARD_CORNER,
        ID_CARD_MONITOR,
        ID_REQUIRE_TICK_FOR,
        ID_OLLAMA_LINK,
        ID_PROMPT_TOGGLE,
        ID_PROMPT_EDIT,
        ID_RESET_PROMPT,
        ID_SAVE,
        ID_CANCEL,
    ] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            add_style(h, WS_TABSTOP.0);
        }
    }

    BuiltUi {
        prompt_edit,
        body,
        children: ctx.children.into_inner(),
        content_top_dp,
        win_h_dp,
    }
}

fn add_style(hwnd: HWND, bits: u32) {
    unsafe {
        let cur = windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
        );
        windows::Win32::UI::WindowsAndMessaging::SetWindowLongPtrW(
            hwnd,
            windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
            cur | bits as isize,
        );
    }
}

const TRACKBAR_CLASS_STR: &str = "msctls_trackbar32";
const WS_EX_BORDER: u32 = 0x0000_0100;
const BM_SETCHECK: u32 = 0x00F1;

// ---------------------------------------------------------------------------
// Pure logic: parsing, clamping, and the config-diff/apply used on Save.
// Kept free of any Win32 type so it can be unit-tested directly.
// ---------------------------------------------------------------------------

/// Everything read off the controls, before it is folded into a [`Config`].
/// Kept separate from the `Config` it produces so the merge logic
/// (`build_config`) can be unit-tested without creating a single window.
#[derive(Debug, Clone, Default)]
struct RawForm {
    provider_choice: usize,
    /// The Ollama model combo's text, which may carry a "(vision)" badge.
    ollama_model: String,
    openai_key: String,
    openai_model: String,
    openai_effort: String,
    anthropic_key: String,
    anthropic_model: String,
    anthropic_effort: String,
    gemini_key: String,
    gemini_model: String,
    gemini_effort: String,
    max_edge_text: String,
    monitor: String,
    card_seconds_text: String,
    text_scale_raw: f32,
    prompt: String,
    require_tick_for: String,
    /// #498: the card-corner and card-monitor combos' text.
    card_corner: String,
    card_monitor: String,
    /// #509: the redaction checkbox (capture.redact).
    redact: bool,
}

/// Reads every control into a [`RawForm`], without yet folding it into a
/// [`Config`]. Split out from [`read_form`] so Save can validate the raw
/// values (#344) before committing to `build_config`'s fallback behavior.
fn read_raw_form(inner: &SettingsInner) -> RawForm {
    let hwnd = inner.hwnd;
    RawForm {
        provider_choice: unsafe {
            SendMessageW(
                get_dlg_item(hwnd, ID_ACTIVE_PROVIDER).unwrap_or(HWND(std::ptr::null_mut())),
                CB_GETCURSEL,
                None,
                None,
            )
            .0 as usize
        },
        ollama_model: combo_selected_text(hwnd, ID_OLLAMA_MODEL),
        openai_key: get_dlg_item(hwnd, ID_OPENAI_KEY)
            .map(get_text)
            .unwrap_or_default(),
        openai_model: combo_selected_text(hwnd, ID_OPENAI_MODEL),
        openai_effort: combo_selected_text(hwnd, ID_OPENAI_EFFORT),
        anthropic_key: get_dlg_item(hwnd, ID_ANTHROPIC_KEY)
            .map(get_text)
            .unwrap_or_default(),
        anthropic_model: combo_selected_text(hwnd, ID_ANTHROPIC_MODEL),
        anthropic_effort: combo_selected_text(hwnd, ID_ANTHROPIC_EFFORT),
        gemini_key: get_dlg_item(hwnd, ID_GEMINI_KEY)
            .map(get_text)
            .unwrap_or_default(),
        gemini_model: combo_selected_text(hwnd, ID_GEMINI_MODEL),
        gemini_effort: combo_selected_text(hwnd, ID_GEMINI_EFFORT),
        // #341: the combo now shows "Low"/"Medium"/.../"Maximum" rather than
        // the raw number, so convert back to the numeric text `build_config`
        // and `find_invalid_numeric_field` (#344) already expect. An
        // unrecognized selection (should not happen with `CBS_DROPDOWNLIST`)
        // becomes blank, which both of those already treat as "leave alone".
        max_edge_text: label_to_max_edge(&combo_selected_text(hwnd, ID_MAX_EDGE))
            .map(|v| v.to_string())
            .unwrap_or_default(),
        monitor: combo_selected_text(hwnd, ID_MONITOR),
        card_seconds_text: get_dlg_item(hwnd, ID_CARD_SECONDS)
            .map(get_text)
            .unwrap_or_default(),
        text_scale_raw: get_dlg_item(hwnd, ID_TEXT_SCALE_TRACK)
            .map(|h| unsafe { SendMessageW(h, TBM_GETPOS, None, None).0 as f32 / 100.0 })
            .unwrap_or(inner.original.ui.text_scale),
        prompt: get_dlg_item(hwnd, ID_PROMPT_EDIT)
            .map(get_text)
            .unwrap_or_default(),
        require_tick_for: combo_selected_text(hwnd, ID_REQUIRE_TICK_FOR),
        card_corner: combo_selected_text(hwnd, ID_CARD_CORNER),
        card_monitor: combo_selected_text(hwnd, ID_CARD_MONITOR),
        // A missing control keeps the stored value, never flips it.
        redact: if get_dlg_item(hwnd, ID_REDACT).is_some() {
            checkbox_checked(hwnd, ID_REDACT)
        } else {
            inner.original.capture.redact
        },
    }
}

/// Reads every control and folds it into a [`Config`] via [`build_config`].
/// Exercised directly by UI smoke tests that need the full round trip; the
/// Save path (`handle_command`) instead calls [`read_raw_form`] and
/// [`find_invalid_numeric_field`] first, so it can reject before this ever
/// commits to `build_config`'s fallback behavior.
#[cfg(test)]
fn read_form(inner: &SettingsInner) -> Config {
    let raw = read_raw_form(inner);
    build_config(&inner.original, &raw)
}

// ---------------------------------------------------------------------------
// #345: changing the two shortcuts from here
// ---------------------------------------------------------------------------

/// What to do with a chord the learn hook captured for a pending Change.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LearnedVerdict {
    /// A bare Esc: the user is backing out.
    Cancel,
    /// Not allowed, with the inline text to show. Learning stays armed.
    Refuse(String),
    /// A known system shortcut: show the text, and a second press of the
    /// same chord applies it. Learning stays armed.
    Warn(String),
    /// Stage it.
    Apply,
}

/// Judges a captured `chord` for slot `which`. Pure, so the flow is tested
/// without a hook or a window. The conflict warning and its "press it again"
/// confirmation are `hotkey_conflicts::decide`, the same as the tray's.
fn judge_learned(
    hotkeys: &Hotkeys,
    pending: Option<(usize, Chord)>,
    which: usize,
    chord: Chord,
) -> LearnedVerdict {
    let bare = !(chord.ctrl || chord.shift || chord.alt || chord.win);
    if bare && chord.vk == VK_ESCAPE.0 as u32 {
        return LearnedVerdict::Cancel;
    }
    let anchored = chord.ctrl || chord.alt || chord.win;
    let name = chord_to_string(&chord);
    // A plain letter or Enter, or Shift plus a letter, as a global shortcut
    // would swallow that key in every app. Function keys are fine without.
    if !anchored && !(0x70..=0x87).contains(&chord.vk) {
        return LearnedVerdict::Refuse(format!(
            "{name} needs Ctrl, Alt or Win with it. Press another."
        ));
    }
    let others = [
        (HK_PRIMARY, Some(hotkeys.primary), slot_name(HK_PRIMARY)),
        (
            HK_SECONDARY,
            Some(hotkeys.secondary),
            slot_name(HK_SECONDARY),
        ),
        (HK_PAUSE, hotkeys.pause, slot_name(HK_PAUSE)),
        (0, hotkeys.palette, "palette key"),
    ];
    for (slot, bound, label) in others {
        if slot != which && bound == Some(chord) {
            return LearnedVerdict::Refuse(format!(
                "{name} is already the {label}. Press another."
            ));
        }
    }
    match crate::hotkey_conflicts::decide(pending, which, chord) {
        crate::hotkey_conflicts::LearnDecision::Apply => LearnedVerdict::Apply,
        crate::hotkey_conflicts::LearnDecision::Warn(conflict) => LearnedVerdict::Warn(format!(
            "{name} is already used by {}. Press it again to use it anyway.",
            conflict.owner
        )),
    }
}

/// Why a Change cannot work right now, or `None`. While paused the hook
/// passes every key through untouched, so nothing would ever be captured.
fn learn_unavailable_reason(paused: bool, hook_ready: bool) -> Option<&'static str> {
    if paused {
        Some("Wingman is paused. Resume it, then try again.")
    } else if !hook_ready {
        Some("The keyboard hook is not installed, so keys cannot be changed here.")
    } else {
        None
    }
}

/// The config Save returns: the form folded in, plus the hotkeys staged in
/// this window.
fn finalize_config(inner: &SettingsInner, raw: &RawForm) -> Config {
    let mut cfg = build_config(&inner.original, raw);
    cfg.hotkeys = inner.hotkeys.clone();
    cfg
}

fn set_hotkey_status(hwnd: HWND, text: &str) {
    if let Some(status) = get_dlg_item(hwnd, ID_HOTKEY_STATUS) {
        set_text(status, text);
    }
}

/// Points the two shortcut labels at `hotkeys`.
fn set_hotkey_labels(hwnd: HWND, hotkeys: &Hotkeys) {
    if let Some(l) = get_dlg_item(hwnd, ID_PRIMARY_LABEL) {
        set_text(
            l,
            &format!("Copilot key: {}", chord_to_string(&hotkeys.primary)),
        );
    }
    if let Some(l) = get_dlg_item(hwnd, ID_SECONDARY_LABEL) {
        set_text(
            l,
            &format!("Other shortcut: {}", chord_to_string(&hotkeys.secondary)),
        );
    }
}

/// Arms learn mode (and its end-of-window timer) for `which`.
fn arm_change(inner: &mut SettingsInner, which: usize) -> bool {
    if let Some(reason) = learn_unavailable_reason(crate::pause::is_paused_now(), true) {
        set_hotkey_status(inner.hwnd, reason);
        return false;
    }
    if !arm_learning(which) {
        let reason = learn_unavailable_reason(false, false).unwrap_or_default();
        set_hotkey_status(inner.hwnd, reason);
        return false;
    }
    inner.learning = Some(which);
    unsafe {
        SetTimer(Some(inner.hwnd), LEARN_TIMER_ID, LEARN_TIMER_MS, None);
    }
    true
}

fn stop_learning(inner: &mut SettingsInner) {
    inner.learning = None;
    inner.pending_conflict = None;
    unsafe {
        let _ = KillTimer(Some(inner.hwnd), LEARN_TIMER_ID);
    }
    disarm_learning();
}

/// Removes and frees every queued `WM_APP_LEARNED` on this thread.
fn drain_learned_messages() {
    let mut msg = MSG::default();
    while unsafe { PeekMessageW(&mut msg, None, WM_APP_LEARNED, WM_APP_LEARNED, PM_REMOVE) }
        .as_bool()
    {
        if msg.lParam.0 != 0 {
            drop(unsafe { Box::from_raw(msg.lParam.0 as *mut Chord) });
        }
    }
}

/// A Change button was pressed.
fn start_change(inner: &mut SettingsInner, which: usize) {
    if inner.learning != Some(which) {
        inner.pending_conflict = None;
    }
    if arm_change(inner, which) {
        set_hotkey_status(inner.hwnd, "Press the new key now. Esc cancels.");
    } else {
        // A Change armed earlier must not stay armed after a failed re-arm.
        // The reason `arm_change` put on the status line is left as it is.
        stop_learning(inner);
    }
}

/// The learn window ran out with no key.
fn learn_timed_out(inner: &mut SettingsInner) {
    if inner.learning.is_some() {
        stop_learning(inner);
        set_hotkey_status(inner.hwnd, "No key pressed. The shortcut is unchanged.");
    } else {
        unsafe {
            let _ = KillTimer(Some(inner.hwnd), LEARN_TIMER_ID);
        }
    }
}

/// A captured chord for the pending Change.
fn handle_learned(inner: &mut SettingsInner, which: usize, chord: Chord) {
    match judge_learned(&inner.hotkeys, inner.pending_conflict, which, chord) {
        LearnedVerdict::Cancel => {
            stop_learning(inner);
            set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
        }
        LearnedVerdict::Refuse(text) => {
            set_hotkey_status(inner.hwnd, &text);
            arm_change(inner, which);
        }
        LearnedVerdict::Warn(text) => {
            inner.pending_conflict = Some((which, chord));
            set_hotkey_status(inner.hwnd, &text);
            arm_change(inner, which);
        }
        LearnedVerdict::Apply => {
            stop_learning(inner);
            apply_learned(&mut inner.hotkeys, which, chord);
            set_hotkey_labels(inner.hwnd, &inner.hotkeys);
            set_hotkey_status(
                inner.hwnd,
                &format!("Set to {}. Press Save to keep it.", chord_to_string(&chord)),
            );
        }
    }
}

/// Takes a `WM_APP_LEARNED` off the loop if a Change is pending, handling it
/// and freeing its boxed chord. Returns `false` (leaving the message to be
/// dispatched, where the main window frees it) when none is.
unsafe fn take_learned(hwnd: HWND, msg: &MSG) -> bool {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner;
    if ptr.is_null() || (*ptr).learning.is_none() || msg.lParam.0 == 0 {
        return false;
    }
    let chord = *Box::from_raw(msg.lParam.0 as *mut Chord);
    handle_learned(&mut *ptr, msg.wParam.0, chord);
    true
}

/// Esc while a Change is pending: cancels that Change. `false` if none was.
unsafe fn cancel_learning_if_active(hwnd: HWND) -> bool {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner;
    if ptr.is_null() || (*ptr).learning.is_none() {
        return false;
    }
    let inner = &mut *ptr;
    stop_learning(inner);
    set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
    true
}

#[allow(dead_code)] // exercised by tests; #194 replaced its build_config call site with merge_provider_order
fn order_from_choice(choice: usize) -> Vec<String> {
    if choice == 1 {
        vec!["anthropic".to_string(), "openai".to_string()]
    } else {
        vec!["openai".to_string(), "anthropic".to_string()]
    }
}

/// The "Active provider" combo index for `order` (an index into
/// `ActiveProvider::ALL`: ChatGPT, Claude, Ollama, Gemini). A config led by
/// anything else shows as ChatGPT.
fn choice_from_order(order: &[String]) -> usize {
    ActiveProvider::from_order(order).index()
}

/// #194: `build_config`'s provider-order merge. `choice` (the Settings
/// radio: 0 = openai first, 1 = anthropic first) only ever decides the
/// relative order of `"openai"` and `"anthropic"` within `configured` --
/// every other entry (a hand-added `"ollama"` or `"gemini"`, a duplicate, an
/// unrecognized name) keeps its exact position. This is what replaced
/// `order_from_choice`'s old unconditional `cfg.providers.order =
/// order_from_choice(choice)`, which silently deleted everything else
/// `providers.order` held on every Settings save -- see the filed finding.
///
/// The precise rule, by how many of the two are present in `configured`
/// (first occurrence of each, so a duplicate's later copies are untouched):
/// - **Both present:** if their current relative order already matches
///   `choice`, nothing changes. Otherwise the two entries simply swap array
///   slots (only those two index positions change; everything else,
///   including anything between them, keeps its position).
/// - **Exactly one present:** the missing one is inserted immediately
///   adjacent to the present one, on the correct side for `choice` (right
///   before it if it must come first, right after if it must come second).
/// - **Neither present:** both are appended to the end, in `choice`'s order
///   -- matching what `order_from_choice` always produced when `configured`
///   was empty.
///
/// #183, #195: choosing Ollama or Gemini (choice 2 or 3) moves it to the
/// front (or adds it there) and leaves everything else where it was.
/// Switching away from an order led by one of them puts the chosen provider
/// first, since the swap above alone would leave the old leader answering.
fn merge_provider_order(configured: &[String], choice: usize) -> Vec<String> {
    let chosen = ActiveProvider::from_index(choice);
    if matches!(chosen, ActiveProvider::Ollama | ActiveProvider::Gemini) {
        if configured.is_empty() {
            return vec![
                chosen.order_name().to_string(),
                "openai".to_string(),
                "anthropic".to_string(),
            ];
        }
        let mut order = configured.to_vec();
        chosen.move_to_front(&mut order);
        return order;
    }
    let mut order = merge_cloud_order(configured, choice);
    if matches!(
        ActiveProvider::from_order(configured),
        ActiveProvider::Ollama | ActiveProvider::Gemini
    ) {
        chosen.move_to_front(&mut order);
    }
    order
}

/// The #194 rule: `choice` 0/1 only decides the relative order of openai and
/// anthropic within `configured`.
fn merge_cloud_order(configured: &[String], choice: usize) -> Vec<String> {
    let (first, second) = if choice == 1 {
        ("anthropic", "openai")
    } else {
        ("openai", "anthropic")
    };

    let mut order = configured.to_vec();
    let idx_first = order.iter().position(|s| s == first);
    let idx_second = order.iter().position(|s| s == second);

    match (idx_first, idx_second) {
        (Some(i), Some(j)) => {
            // `first` must end up before `second`. If it already is
            // (i < j), nothing to do; otherwise swap just those two slots.
            if i > j {
                order.swap(i, j);
            }
        }
        (Some(i), None) => order.insert(i + 1, second.to_string()),
        (None, Some(j)) => order.insert(j, first.to_string()),
        (None, None) => {
            order.push(first.to_string());
            order.push(second.to_string());
        }
    }
    order
}

/// Parses a `u32`, falling back to `fallback` on anything that doesn't
/// parse cleanly. Never panics, never produces a silently-zeroed value from
/// garbage input.
fn parse_u32_or(text: &str, fallback: u32) -> u32 {
    text.trim().parse::<u32>().unwrap_or(fallback)
}

/// Like [`parse_u32_or`], but also rejects zero -- a zero `max_edge` would
/// mean "downscale every capture to nothing", which is never a value a user
/// actually wants (unlike `card_seconds`, where 0 is meaningful).
fn parse_max_edge_or(text: &str, fallback: u32) -> u32 {
    match text.trim().parse::<u32>() {
        Ok(v) if v > 0 => v,
        _ => fallback,
    }
}

/// Review of #341 (data loss): a value outside a combo's fixed preset list
/// (a hand-edited `config.toml`, e.g. `max_edge = 999`) used to display as
/// the nearest/default preset -- so opening Settings and pressing Save with
/// nothing touched silently replaced the real value with that preset's.
/// Every preset combo below instead shows an honest `"Custom (<value>)"`
/// entry for an off-preset value and selects it, so an untouched Save writes
/// back the exact original value; the wrapping quotes are omitted from the
/// label itself, matching `parse_custom_label`'s expectation.
fn custom_label(raw: &str) -> String {
    format!("Custom ({raw})")
}

/// The inverse of [`custom_label`]: the raw text inside `"Custom (...)"`, or
/// `None` if `label` isn't one.
fn parse_custom_label(label: &str) -> Option<&str> {
    label.strip_prefix("Custom (")?.strip_suffix(')')
}

/// #341: the config's own `low`/`medium`/`high` effort tokens, and the plain
/// words shown in their place in Settings ("Thinking:"). Index-paired with
/// [`EFFORT_LABELS`].
const EFFORT_VALUES: [&str; 3] = ["low", "medium", "high"];
/// #341: display labels for [`EFFORT_VALUES`], in the same order.
const EFFORT_LABELS: [&str; 3] = ["Fast", "Balanced", "Thorough"];

/// The label shown for a given effort value: one of [`EFFORT_LABELS`] for an
/// exact preset match, or `"Custom (<value>)"` (never silently rounded to a
/// preset -- see the review-fix doc comment above [`custom_label`]).
fn effort_display(value: &str) -> String {
    match EFFORT_VALUES.iter().position(|&v| v == value) {
        Some(i) => EFFORT_LABELS[i].to_string(),
        None => custom_label(value),
    }
}

/// Parses a combo selection back into an effort value: one of
/// [`EFFORT_VALUES`] for a preset label, the wrapped text for a
/// `"Custom (...)"` label, or `None` for a blank/unrecognized selection so
/// callers can fall back to the original config value.
fn label_to_effort(label: &str) -> Option<String> {
    if let Some(i) = EFFORT_LABELS.iter().position(|&l| l == label) {
        return Some(EFFORT_VALUES[i].to_string());
    }
    parse_custom_label(label).map(|s| s.to_string())
}

/// #341: the four `capture.max_edge` presets, and the plain words shown in
/// their place in Settings ("Screenshot detail:"). Index-paired with
/// [`MAX_EDGE_LABELS`].
const MAX_EDGE_VALUES: [u32; 4] = [1024, 1280, 1568, 2048];
/// #341: display labels for [`MAX_EDGE_VALUES`], in the same order.
const MAX_EDGE_LABELS: [&str; 4] = ["Low", "Medium", "High", "Maximum"];

/// The label for a given `max_edge` value: one of [`MAX_EDGE_LABELS`] for an
/// exact preset match, or `"Custom (<value>)"` (never silently rounded to
/// the nearest preset -- see the review-fix doc comment above
/// [`custom_label`]).
fn max_edge_display(value: u32) -> String {
    match MAX_EDGE_VALUES.iter().position(|&v| v == value) {
        Some(i) => MAX_EDGE_LABELS[i].to_string(),
        None => custom_label(&value.to_string()),
    }
}

/// Parses a combo selection back into a `max_edge` value: one of
/// [`MAX_EDGE_VALUES`] for a preset label, the parsed number for a
/// `"Custom (...)"` label, or `None` for a blank/unrecognized/unparsable
/// selection so callers can fall back to the original config value.
fn label_to_max_edge(label: &str) -> Option<u32> {
    if let Some(i) = MAX_EDGE_LABELS.iter().position(|&l| l == label) {
        return Some(MAX_EDGE_VALUES[i]);
    }
    parse_custom_label(label)?.parse().ok()
}

/// #341: the config's own `active`/`primary` monitor tokens, and the plain
/// words shown in their place in Settings ("Screen:"). Index-paired with
/// [`MONITOR_LABELS`].
const MONITOR_VALUES: [&str; 2] = ["active", "primary"];
/// #341: display labels for [`MONITOR_VALUES`], in the same order.
const MONITOR_LABELS: [&str; 2] = ["The one I'm using", "The main display"];

/// The label shown for a given monitor value: one of [`MONITOR_LABELS`] for
/// an exact preset match, or `"Custom (<value>)"` (never silently defaulted
/// -- see the review-fix doc comment above [`custom_label`]).
fn monitor_display(value: &str) -> String {
    match MONITOR_VALUES.iter().position(|&v| v == value) {
        Some(i) => MONITOR_LABELS[i].to_string(),
        None => custom_label(value),
    }
}

/// Parses a combo selection back into a monitor value: one of
/// [`MONITOR_VALUES`] for a preset label, the wrapped text for a
/// `"Custom (...)"` label, or `None` for a blank/unrecognized selection so
/// callers can fall back to the original config value.
fn label_to_monitor(label: &str) -> Option<String> {
    if let Some(i) = MONITOR_LABELS.iter().position(|&l| l == label) {
        return Some(MONITOR_VALUES[i].to_string());
    }
    parse_custom_label(label).map(|s| s.to_string())
}

/// Adds `display` to `combo` as an extra entry and selects it, but only if
/// it isn't already one of the combo's fixed preset labels (already added by
/// the caller) -- used by the three "custom value" combos above so an
/// off-preset value gets its own honest entry instead of colliding with or
/// hiding behind a preset.
fn combo_select_with_custom(combo: HWND, presets: &[&str], display: &str) {
    if !presets.contains(&display) {
        combo_add(combo, display);
    }
    combo_select(combo, display);
}

/// #403: the three [`RequireTickFor`] labels shown in the Settings combo, in
/// display order. Plain words, not the config's internal `all`/`sensitive`/
/// `none` tokens (rule 11's sibling concern, #341: Settings speaks in
/// internals).
const REQUIRE_TICK_FOR_LABELS: [&str; 3] = ["All fields", "Only sensitive fields", "Never"];

/// The label shown for a given [`RequireTickFor`] value. Inverse of
/// [`label_to_require_tick_for`].
fn require_tick_for_label(value: RequireTickFor) -> &'static str {
    match value {
        RequireTickFor::All => REQUIRE_TICK_FOR_LABELS[0],
        RequireTickFor::Sensitive => REQUIRE_TICK_FOR_LABELS[1],
        RequireTickFor::None => REQUIRE_TICK_FOR_LABELS[2],
    }
}

/// Parses a combo selection back into a [`RequireTickFor`]. `None` for a
/// blank or unrecognized selection, so callers can fall back to the
/// original config value rather than silently picking a default.
fn label_to_require_tick_for(label: &str) -> Option<RequireTickFor> {
    match label {
        "All fields" => Some(RequireTickFor::All),
        "Only sensitive fields" => Some(RequireTickFor::Sensitive),
        "Never" => Some(RequireTickFor::None),
        _ => None,
    }
}

/// #498: the five [`CardCorner`] labels in the Settings combo, in display
/// order. Plain words, not the config's `bottom_right` tokens.
const CARD_CORNER_LABELS: [&str; 5] = [
    "Bottom right",
    "Bottom left",
    "Top right",
    "Top left",
    "Next to the mouse",
];

/// The label shown for a given [`CardCorner`]. Inverse of
/// [`label_to_card_corner`].
fn card_corner_label(value: CardCorner) -> &'static str {
    match value {
        CardCorner::BottomRight => CARD_CORNER_LABELS[0],
        CardCorner::BottomLeft => CARD_CORNER_LABELS[1],
        CardCorner::TopRight => CARD_CORNER_LABELS[2],
        CardCorner::TopLeft => CARD_CORNER_LABELS[3],
        CardCorner::FollowMouse => CARD_CORNER_LABELS[4],
    }
}

/// Parses a combo selection back into a [`CardCorner`]. `None` for a blank
/// or unrecognized selection, so callers keep the original config value.
fn label_to_card_corner(label: &str) -> Option<CardCorner> {
    [
        CardCorner::BottomRight,
        CardCorner::BottomLeft,
        CardCorner::TopRight,
        CardCorner::TopLeft,
        CardCorner::FollowMouse,
    ]
    .into_iter()
    .find(|&c| card_corner_label(c) == label)
}

const CARD_MONITOR_CURSOR_LABEL: &str = "Where the mouse is";
const CARD_MONITOR_PRIMARY_LABEL: &str = "The main display";

fn card_monitor_numbered_label(n: usize) -> String {
    format!("Display {n} from the left")
}

/// #498: the card-monitor choices as `(label, config value)` pairs, for a
/// machine with `count` monitors. "Display n" entries only appear with two
/// or more monitors, and only up to `count`, so a choice never points at a
/// monitor that is not there.
fn card_monitor_options(count: usize) -> Vec<(String, String)> {
    let mut out = vec![
        (CARD_MONITOR_CURSOR_LABEL.to_string(), "cursor".to_string()),
        (
            CARD_MONITOR_PRIMARY_LABEL.to_string(),
            "primary".to_string(),
        ),
    ];
    if count >= 2 {
        out.extend((1..=count).map(|n| (card_monitor_numbered_label(n), n.to_string())));
    }
    out
}

/// The label for a stored `ui.card_monitor` value: the matching entry of
/// [`card_monitor_options`], or `"Custom (<value>)"` for anything else (a
/// hand-edited `"Primary"`, or `"3"` after a monitor was unplugged), so an
/// untouched Save writes the exact original text back.
fn card_monitor_display(value: &str, count: usize) -> String {
    card_monitor_options(count)
        .into_iter()
        .find(|(_, v)| v == value)
        .map(|(label, _)| label)
        .unwrap_or_else(|| custom_label(value))
}

/// Parses a combo selection back into a `ui.card_monitor` value. `None` for
/// a blank or unrecognized selection, so callers keep the original value.
/// Needs no monitor count: a numbered label carries its own number.
fn label_to_card_monitor(label: &str) -> Option<String> {
    match label {
        CARD_MONITOR_CURSOR_LABEL => return Some("cursor".to_string()),
        CARD_MONITOR_PRIMARY_LABEL => return Some("primary".to_string()),
        _ => {}
    }
    if let Some(n) = label
        .strip_prefix("Display ")
        .and_then(|s| s.strip_suffix(" from the left"))
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| card_monitor_numbered_label(n) == label)
    {
        return Some(n.to_string());
    }
    parse_custom_label(label).map(str::to_string)
}

/// #344: a numeric Settings field whose text does not parse to a value
/// `build_config` would actually accept, found by [`find_invalid_numeric_field`].
/// Distinct from "blank", which `build_config` already treats as "leave the
/// original value alone" -- this only fires for text a user actually typed
/// that isn't a valid whole number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InvalidField {
    MaxEdge,
    CardSeconds,
}

impl InvalidField {
    /// The control to focus so the user can fix it immediately.
    fn control_id(self) -> i32 {
        match self {
            InvalidField::MaxEdge => ID_MAX_EDGE,
            InvalidField::CardSeconds => ID_CARD_SECONDS,
        }
    }

    /// The inline message shown next to Save (rule 7: never a dialog box).
    /// No em dash (rule 11).
    fn message(self) -> &'static str {
        match self {
            InvalidField::MaxEdge => "Max edge must be a whole number greater than 0.",
            InvalidField::CardSeconds => "Auto-dismiss must be a whole number of seconds.",
        }
    }
}

/// Checks the two free-typed numeric fields for text that would silently be
/// discarded by [`build_config`]'s fallback (#344: the bug was that garbage
/// input like "2ooo" saved as if nothing had been typed, with no sign
/// anything was wrong). A blank field is not an error here -- it is the
/// established "leave this alone" convention `build_config` already
/// implements and several tests already rely on. Only text that is present
/// but does not parse to a value `build_config` would actually use counts as
/// invalid. Returns the first invalid field found (max edge before card
/// seconds), since only one inline message is shown at a time.
fn find_invalid_numeric_field(raw: &RawForm) -> Option<InvalidField> {
    let max_edge = raw.max_edge_text.trim();
    if !max_edge.is_empty() {
        match max_edge.parse::<u32>() {
            Ok(v) if v > 0 => {}
            _ => return Some(InvalidField::MaxEdge),
        }
    }

    let card_seconds = raw.card_seconds_text.trim();
    if !card_seconds.is_empty() && card_seconds.parse::<u32>().is_err() {
        return Some(InvalidField::CardSeconds);
    }

    None
}

/// Clamps to the range `Card::set_text_scale` (see `ui/card.rs`) accepts;
/// also guards against NaN/infinite input, which `f32::clamp` propagates
/// rather than resolving.
fn clamp_text_scale(v: f32) -> f32 {
    if !v.is_finite() {
        1.0
    } else {
        v.clamp(0.5, 2.0)
    }
}

/// The number of trailing characters of a stored key Settings ever shows
/// (#2). A key no longer than this is short enough that showing it in full
/// is the same thing as showing "the last four characters".
const KEY_VISIBLE_TAIL: usize = 4;

/// Masks all but the last [`KEY_VISIBLE_TAIL`] characters of `key` with
/// `*`. An empty key masks to an empty string, so an unset key still shows
/// as a genuinely empty field rather than a row of stars.
fn mask_key(key: &str) -> String {
    let len = key.chars().count();
    if len <= KEY_VISIBLE_TAIL {
        return key.to_string();
    }
    let tail: String = key.chars().skip(len - KEY_VISIBLE_TAIL).collect();
    format!("{}{tail}", "*".repeat(len - KEY_VISIBLE_TAIL))
}

/// Shown in a key field instead of [`mask_key`]'s stars when the stored
/// credential is [`crate::config::UNREADABLE_KEY_MARKER`] (#175): masking
/// the marker's own control characters would render as garbage, and
/// stars-only would look identical to a real key nobody can tell apart from
/// "everything is fine". No em dash (rule 11).
const UNREADABLE_KEY_PLACEHOLDER_TEXT: &str = "(stored key unreadable: retype to replace)";

/// What a key field should actually show. [`mask_key`] for a real key (or an
/// empty one); [`UNREADABLE_KEY_PLACEHOLDER_TEXT`] for the unreadable
/// marker, so the raw marker's control characters never reach a visible
/// Win32 control. `resolve_key_field` is this function's inverse for Save.
fn key_field_display(key: &str) -> String {
    if key == crate::config::UNREADABLE_KEY_MARKER {
        UNREADABLE_KEY_PLACEHOLDER_TEXT.to_string()
    } else {
        mask_key(key)
    }
}

/// Resolves what a key field should become on Save. The field is populated
/// with `mask_key(original)`, never the real key (#2's masking), so if the
/// user never touched it the text on Save is still exactly that mask and
/// the real key must be carried through unchanged. Any other text --
/// including empty, which clears the key -- is what the user actually
/// typed and becomes the new key.
///
/// Known limitation: if a user's real *new* key happens to be typed exactly
/// as `mask_key(original)` (a run of `*` followed by 4 characters that
/// happen to match), it is indistinguishable from "untouched" and the old
/// key survives instead. Not worth a dirty-flag/EN_CHANGE tracker for how
/// unlikely that string is to be a real key.
///
/// #175: when `original` is [`crate::config::UNREADABLE_KEY_MARKER`], the
/// field was shown as [`UNREADABLE_KEY_PLACEHOLDER_TEXT`]
/// ([`key_field_display`]), not `mask_key(original)` -- so the untouched
/// check compares against that placeholder instead. Left untouched, the
/// marker survives to `push_secrets_to_store`, which leaves the unreadable
/// credential alone; typed over (including cleared to empty, a deliberate
/// removal), it resolves like any other field.
fn resolve_key_field(original: &str, form_text: &str) -> String {
    if original == crate::config::UNREADABLE_KEY_MARKER {
        if form_text == UNREADABLE_KEY_PLACEHOLDER_TEXT {
            return original.to_string();
        }
        return form_text.to_string();
    }
    if form_text == mask_key(original) {
        original.to_string()
    } else {
        form_text.to_string()
    }
}

/// Merges parsed form values onto a clone of `original`. Any field the form
/// could not produce a sane value for (a blank combo selection, unparsable
/// numeric text) falls back to the value already in `original` rather than
/// zeroing or erroring.
fn build_config(original: &Config, raw: &RawForm) -> Config {
    let mut cfg = original.clone();

    cfg.providers.order = merge_provider_order(&original.providers.order, raw.provider_choice);

    let ollama_model = strip_vision_badge(raw.ollama_model.trim());
    if !ollama_model.is_empty() {
        cfg.providers.ollama.model = ollama_model.to_string();
    }

    cfg.providers.openai.api_key =
        resolve_key_field(&original.providers.openai.api_key, &raw.openai_key);
    if !raw.openai_model.trim().is_empty() {
        cfg.providers.openai.model = raw.openai_model.clone();
    }
    if let Some(v) = label_to_effort(raw.openai_effort.trim()) {
        cfg.providers.openai.effort = v;
    }

    cfg.providers.anthropic.api_key =
        resolve_key_field(&original.providers.anthropic.api_key, &raw.anthropic_key);
    if !raw.anthropic_model.trim().is_empty() {
        cfg.providers.anthropic.model = raw.anthropic_model.clone();
    }
    if let Some(v) = label_to_effort(raw.anthropic_effort.trim()) {
        cfg.providers.anthropic.effort = v;
    }

    cfg.providers.gemini.api_key =
        resolve_key_field(&original.providers.gemini.api_key, &raw.gemini_key);
    if !raw.gemini_model.trim().is_empty() {
        cfg.providers.gemini.model = raw.gemini_model.clone();
    }
    if let Some(v) = label_to_effort(raw.gemini_effort.trim()) {
        cfg.providers.gemini.effort = v;
    }

    cfg.capture.max_edge = parse_max_edge_or(&raw.max_edge_text, original.capture.max_edge);
    cfg.capture.redact = raw.redact;
    if let Some(v) = label_to_monitor(raw.monitor.trim()) {
        cfg.capture.monitor = v;
    }

    cfg.ui.card_seconds = parse_u32_or(&raw.card_seconds_text, original.ui.card_seconds);
    cfg.ui.text_scale = clamp_text_scale(raw.text_scale_raw);
    cfg.ui.prompt = if raw.prompt.trim().is_empty() {
        original.ui.prompt.clone()
    } else {
        raw.prompt.clone()
    };

    cfg.ui.card_corner = label_to_card_corner(&raw.card_corner).unwrap_or(original.ui.card_corner);
    if let Some(v) = label_to_card_monitor(&raw.card_monitor) {
        cfg.ui.card_monitor = v;
    }

    cfg.forms.require_tick_for =
        label_to_require_tick_for(&raw.require_tick_for).unwrap_or(original.forms.require_tick_for);

    cfg
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// Put `text` on the clipboard. Returns false if the clipboard is unavailable
/// (another process holds it), so tests can skip rather than fail spuriously.
#[cfg(test)]
fn put_on_clipboard(owner: HWND, text: &str) -> bool {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(Some(owner)).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let bytes = wide.len() * 2;
        let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            return false;
        };
        let dst = GlobalLock(h) as *mut u16;
        if dst.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr(), dst, wide.len());
        let _ = GlobalUnlock(h);
        // CF_UNICODETEXT == 13
        let ok = SetClipboardData(13, Some(HANDLE(h.0))).is_ok();
        let _ = CloseClipboard();
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test-only window class (rule 9): same `wndproc`, never the production
    /// class name.
    const TEST_CLASS_NAME: &str = "Wingman.Settings.Test.Window.7f3a91c2";

    fn ensure_test_class(instance: HINSTANCE) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            assert!(unsafe { register_class_named(instance, TEST_CLASS_NAME) });
        });
    }

    // -- control ids -----------------------------------------------------

    #[test]
    fn control_ids_are_pairwise_unique() {
        for (i, (name_a, id_a)) in ALL_CONTROL_IDS.iter().enumerate() {
            for (name_b, id_b) in ALL_CONTROL_IDS.iter().skip(i + 1) {
                assert_ne!(
                    id_a, id_b,
                    "{name_a} and {name_b} share control id {id_a} -- \
                     GetDlgItem/WM_COMMAND cannot tell them apart"
                );
            }
        }
    }

    // -- font_face_exists (#346) -------------------------------------------

    #[test]
    fn font_face_exists_finds_a_font_present_on_every_windows_install() {
        // Arial ships with every supported Windows version; if this is ever
        // false, the check itself is broken, not the machine.
        assert!(font_face_exists("Arial"));
    }

    #[test]
    fn font_face_exists_is_false_for_a_made_up_name() {
        assert!(!font_face_exists(
            "Definitely Not A Real Font XYZ123 Wingman Test"
        ));
    }

    // -- ollama_status_line (#14, #15) ------------------------------------

    #[test]
    fn ollama_status_line_reports_not_running_when_nothing_listens() {
        // Deterministic and network-free: bind our own ephemeral port, then
        // close it immediately, guaranteeing nothing is listening there --
        // mirrors `ollama_admin`'s own health-check smoke test rather than
        // depending on whether this machine happens to have Ollama up.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);

        let cfg = OllamaConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            model: "gemma3:4b".to_string(),
            effort: "low".to_string(),
        };
        assert_eq!(
            ollama_status_line(&cfg, true),
            ("Ollama is not running.".to_string(), false)
        );
    }

    // -- ollama_section (#343, pure) ---------------------------------------

    #[test]
    fn ollama_section_not_installed_offers_the_download() {
        let sec = ollama_section("Ollama is not running.".into(), false, false, false);
        assert!(sec.text.starts_with("Ollama runs AI models on this PC"));
        assert_eq!(
            sec.link,
            Some(("Get Ollama", "https://ollama.com/download"))
        );
    }

    #[test]
    fn ollama_section_installed_but_stopped_says_to_start_it_and_has_no_link() {
        let sec = ollama_section("Ollama is not running.".into(), false, true, true);
        assert_eq!(
            sec.text,
            "Ollama is installed but not running. Start Ollama, then reopen Settings."
        );
        assert_eq!(sec.link, None);
    }

    #[test]
    fn ollama_section_running_links_to_the_local_models_docs() {
        let sec = ollama_section("Ollama is running.".into(), true, true, false);
        assert_eq!(sec.text, "Ollama is running. To use it, see Local models.");
        let (label, url) = sec.link.expect("a link");
        assert_eq!(label, "Local models");
        assert!(url.ends_with("docs/providers.md#ollama"), "{url}");
        // Already set up as a provider: no nudge, the link stays.
        let sec = ollama_section("Ollama is running.".into(), true, true, true);
        assert_eq!(sec.text, "Ollama is running.");
        assert!(sec.link.is_some());
    }

    #[test]
    fn ollama_section_text_has_no_em_dash() {
        for (l, i, c) in [
            (false, false, false),
            (false, true, true),
            (true, true, false),
        ] {
            assert!(!ollama_section("x".into(), l, i, c)
                .text
                .contains('\u{2014}'));
        }
    }

    #[test]
    fn ollama_install_candidates_cover_the_default_folder_and_path() {
        let c = ollama_install_candidates(Some(r"C:\Users\me\AppData\Local"), Some(r"C:\a;;D:\b"));
        let s: Vec<String> = c
            .iter()
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .collect();
        assert_eq!(
            s,
            vec![
                r"C:\Users\me\AppData\Local\Programs\Ollama\ollama.exe".to_string(),
                r"C:\a\ollama.exe".to_string(),
                r"D:\b\ollama.exe".to_string(),
            ]
        );
        assert!(ollama_install_candidates(None, None).is_empty());
    }

    // -- #527: status text fits, PATH entries unquoted, probe off-thread ---

    #[test]
    fn ollama_install_candidates_unquote_path_entries() {
        let c = ollama_install_candidates(None, Some(r#""C:\Program Files\Ollama";D:\b"#));
        let s: Vec<String> = c
            .iter()
            .map(|p| p.to_string_lossy().replace('/', "\\"))
            .collect();
        assert_eq!(
            s,
            vec![
                r"C:\Program Files\Ollama\ollama.exe".to_string(),
                r"D:\b\ollama.exe".to_string(),
            ]
        );
    }

    #[test]
    fn shorten_middle_keeps_short_text_and_trims_long_text_in_the_middle() {
        assert_eq!(shorten_middle("abc", 10), "abc");
        let long = r"C:\Users\someone\AppData\Local\Programs\Ollama\a\very\long\folder\ollama.exe";
        let short = shorten_middle(long, 40);
        assert_eq!(short.chars().count(), 40, "{short}");
        assert!(short.starts_with(r"C:\Users"), "{short}");
        assert!(short.ends_with(r"ollama.exe"), "{short}");
        assert!(short.contains("..."), "{short}");
        // Multi-byte text is cut on character boundaries, never panics.
        let _ = shorten_middle(&"\u{e9}".repeat(100), 21);
        assert_eq!(shorten_middle("abcdef", 0), "");
    }

    #[test]
    fn join_within_keeps_the_first_part_and_drops_parts_that_do_not_fit() {
        assert_eq!(join_within(10, &["aaaa", "bbbb", "cccc"]), "aaaa bbbb");
        assert_eq!(join_within(3, &["aaaa", "bbbb"]), "aaaa");
        assert_eq!(join_within(100, &["a", "", "b"]), "a b");
        // A part that does not fit does not stop a shorter later one.
        assert_eq!(join_within(8, &["aaaa", "bbbbbbbb", "cc"]), "aaaa cc");
    }

    #[test]
    fn the_longest_ollama_status_text_fits_the_two_row_label() {
        let path = r"C:\Users\a-rather-long-user-name\AppData\Local\Programs\Ollama\some\deep\folder\ollama.exe";
        let health = OllamaHealth::Listening {
            pid: 1,
            image_path: path.to_string(),
            kind: ListenerKind::Other,
        };
        let msg = health_message(&health);
        assert!(msg.ends_with("ollama.exe."), "{msg}");
        let text = join_within(
            OLLAMA_TEXT_BUDGET,
            &[
                &msg,
                "some-long-model-name:latest-q4 is loaded on GPU.",
                "12 of 12 local models support vision.",
            ],
        );
        assert!(text.chars().count() <= OLLAMA_TEXT_BUDGET, "{text}");
        // The stock tray app's warning plus the Local models hint was the
        // overflowing case: the hint is dropped, the warning is kept whole.
        let warning = listening_stock_tray().message();
        let stock = ollama_section(warning.clone(), true, true, false);
        assert_eq!(stock.text, warning);
        assert!(stock.link.is_some());
    }

    /// #527 / #568: the Settings window is built without asking Ollama
    /// anything. The status says it is checking, and the link button exists
    /// but is hidden until the worker reports. A UI-thread probe would
    /// instead have filled in a real answer here.
    #[test]
    fn build_ui_shows_checking_and_hides_the_ollama_link_until_the_probe_reports() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_STYLE, WS_VISIBLE};

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();
        let title = wide_z("Wingman settings (ollama checking test)");
        let class_name = wide_z(TEST_CLASS_NAME);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        let font = build_font(dpi);
        build_ui(hwnd, instance, dpi, font, &Config::default(), WIN_H_DP);

        let status = get_dlg_item(hwnd, ID_OLLAMA_STATUS).expect("status label");
        assert_eq!(get_text(status), OLLAMA_CHECKING_TEXT);
        let link = get_dlg_item(hwnd, ID_OLLAMA_LINK).expect("link button exists");
        let style = unsafe { GetWindowLongPtrW(link, GWL_STYLE) } as u32;
        assert_eq!(style & WS_VISIBLE.0, 0, "the link starts hidden");

        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// #527 / #568 "wired to nothing" check: the worker's result reaches the
    /// real window through `WM_OLLAMA_STATUS`. Nothing listens on the port
    /// used here, so the answer is one of the two "not running" sections.
    /// If the worker were never spawned, or the message had no arm, the text
    /// would stay at "Checking Ollama...".
    #[test]
    fn the_ollama_probe_result_reaches_the_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetWindowLongPtrW, PeekMessageW, TranslateMessage, GWL_STYLE, MSG,
            PM_REMOVE, WS_VISIBLE,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (ollama probe test)");
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU,
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");
        struct Guard(HWND, *mut SettingsInner);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                    drop(Box::from_raw(self.1));
                }
            }
        }
        let _guard = Guard(hwnd, raw);

        let font = build_font(96);
        let client_h_dp = client_size(hwnd).1;
        let built = build_ui_recorded(hwnd, instance, 96, font, &Config::default(), client_h_dp);
        let slot = {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
            inner.ollama_status.clone()
        };
        let alive = unsafe { &*raw }.alive.clone();

        // A port nothing listens on: bind, note it, release it.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("local_addr").port();
        drop(listener);
        let cfg = OllamaConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            model: "gemma3:4b".to_string(),
            effort: "low".to_string(),
        };
        spawn_ollama_status_probe(hwnd, cfg, false, slot, alive);

        let status = get_dlg_item(hwnd, ID_OLLAMA_STATUS).expect("status label");
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while get_text(status) == OLLAMA_CHECKING_TEXT && std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let text = get_text(status);
        assert_ne!(text, OLLAMA_CHECKING_TEXT, "the probe never reported");
        assert!(
            text.starts_with("Ollama is installed but not running")
                || text.starts_with("Ollama runs AI models"),
            "{text}"
        );
        let link = get_dlg_item(hwnd, ID_OLLAMA_LINK).expect("link button");
        let visible = unsafe { GetWindowLongPtrW(link, GWL_STYLE) } as u32 & WS_VISIBLE.0 != 0;
        assert_eq!(
            visible,
            text.starts_with("Ollama runs AI models"),
            "the button shows exactly when there is something to open"
        );
        assert_eq!(
            unsafe { &*raw }.ollama_link_url.is_some(),
            visible,
            "the click target follows the button"
        );

        unsafe {
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    // -- should_query_ollama_details (#188, pure) -------------------------

    fn listening_other() -> OllamaHealth {
        OllamaHealth::Listening {
            pid: 1,
            image_path: String::new(),
            kind: ListenerKind::Other,
        }
    }

    fn listening_stock_tray() -> OllamaHealth {
        OllamaHealth::Listening {
            pid: 1,
            image_path: String::new(),
            kind: ListenerKind::StockTrayServer,
        }
    }

    #[test]
    fn should_query_ollama_details_is_false_when_not_configured_even_if_listening() {
        assert!(!should_query_ollama_details(false, &listening_other()));
    }

    #[test]
    fn should_query_ollama_details_is_true_when_configured_and_something_other_is_listening() {
        assert!(should_query_ollama_details(true, &listening_other()));
    }

    #[test]
    fn should_query_ollama_details_is_false_when_nothing_is_listening() {
        assert!(!should_query_ollama_details(
            true,
            &OllamaHealth::NotListening
        ));
    }

    #[test]
    fn should_query_ollama_details_is_false_for_the_stock_tray_server_even_if_configured() {
        assert!(!should_query_ollama_details(true, &listening_stock_tray()));
    }

    // -- #188: no HTTP call when Ollama isn't configured -------------------

    #[test]
    fn ollama_status_line_makes_no_http_call_when_not_configured_even_if_something_listens() {
        // A real listener that is NOT the stock tray app and IS reachable --
        // exactly the case #188 is about: the Win32-only health check will
        // report `Listening { kind: Other, .. }`, so the old code would go
        // on to call `/api/ps` and `/api/tags`. With `ollama_configured =
        // false` neither must ever be attempted.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
        listener.set_nonblocking(true).expect("set_nonblocking");
        let port = listener.local_addr().expect("local_addr").port();

        let cfg = OllamaConfig {
            base_url: format!("http://127.0.0.1:{port}"),
            model: "gemma3:4b".to_string(),
            effort: "low".to_string(),
        };

        let (status, _) = ollama_status_line(&cfg, false);
        // The Win32-only health check still runs (it's cheap) and is all
        // this status line shows when not configured -- no GPU/CPU or
        // vision-count sentence appended, which is what the HTTP calls
        // would have produced.
        assert!(status.starts_with("Ollama is running"), "{status}");
        assert!(
            !status.contains("is loaded") && !status.contains("support vision"),
            "no GPU/CPU or vision-count detail when not configured: {status}"
        );

        // Poll briefly rather than asserting instantaneously: any HTTP
        // attempt would already have connected synchronously before
        // `ollama_status_line` returned above, so this is only guarding
        // against the connection having not yet reached the accept queue,
        // never against one arriving later.
        let mut saw_connection = false;
        for _ in 0..20 {
            match listener.accept() {
                Ok(_) => {
                    saw_connection = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        assert!(
            !saw_connection,
            "#188: no /api/ps or /api/tags request when Ollama isn't in providers.order"
        );
    }

    // -- parse_u32_or / parse_max_edge_or --------------------------------

    #[test]
    fn parse_u32_or_accepts_valid_numbers() {
        assert_eq!(parse_u32_or("42", 7), 42);
        assert_eq!(parse_u32_or("  42  ", 7), 42);
        assert_eq!(parse_u32_or("0", 7), 0); // 0 is meaningful for card_seconds
    }

    #[test]
    fn parse_u32_or_falls_back_on_garbage() {
        assert_eq!(parse_u32_or("", 7), 7);
        assert_eq!(parse_u32_or("not a number", 7), 7);
        assert_eq!(parse_u32_or("-5", 7), 7);
        assert_eq!(parse_u32_or("3.5", 7), 7);
    }

    #[test]
    fn parse_max_edge_or_rejects_zero_unlike_parse_u32_or() {
        assert_eq!(parse_max_edge_or("0", 1568), 1568);
        assert_eq!(parse_max_edge_or("2048", 1568), 2048);
        assert_eq!(parse_max_edge_or("bogus", 1568), 1568);
    }

    // -- find_invalid_numeric_field (#344) ---------------------------------

    fn raw_with_numbers(max_edge_text: &str, card_seconds_text: &str) -> RawForm {
        let mut raw = raw_from(&Config::default());
        raw.max_edge_text = max_edge_text.to_string();
        raw.card_seconds_text = card_seconds_text.to_string();
        raw
    }

    #[test]
    fn find_invalid_numeric_field_accepts_valid_values() {
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("2048", "30")),
            None
        );
    }

    #[test]
    fn find_invalid_numeric_field_treats_blank_fields_as_fine() {
        // Blank is "leave the original value alone" (build_config's existing
        // fallback contract), not an error.
        assert_eq!(find_invalid_numeric_field(&raw_with_numbers("", "")), None);
    }

    #[test]
    fn find_invalid_numeric_field_catches_letters_in_card_seconds() {
        // #344's exact repro: typing "2ooo" into Auto-dismiss.
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("2048", "2ooo")),
            Some(InvalidField::CardSeconds)
        );
    }

    #[test]
    fn find_invalid_numeric_field_catches_letters_in_max_edge() {
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("wide", "30")),
            Some(InvalidField::MaxEdge)
        );
    }

    #[test]
    fn find_invalid_numeric_field_catches_a_typed_zero_max_edge() {
        // Zero max edge is silently rejected by `build_config`'s fallback;
        // Save must not pretend that succeeded.
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("0", "30")),
            Some(InvalidField::MaxEdge)
        );
    }

    #[test]
    fn find_invalid_numeric_field_catches_a_negative_card_seconds() {
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("2048", "-5")),
            Some(InvalidField::CardSeconds)
        );
    }

    #[test]
    fn find_invalid_numeric_field_reports_max_edge_before_card_seconds() {
        assert_eq!(
            find_invalid_numeric_field(&raw_with_numbers("bogus", "also bogus")),
            Some(InvalidField::MaxEdge)
        );
    }

    #[test]
    fn invalid_field_messages_have_no_em_dash() {
        for field in [InvalidField::MaxEdge, InvalidField::CardSeconds] {
            assert!(!field.message().contains('\u{2014}'));
        }
    }

    // -- clamp_text_scale --------------------------------------------------

    #[test]
    fn clamp_text_scale_within_range_is_unchanged() {
        assert!((clamp_text_scale(1.25) - 1.25).abs() < f32::EPSILON);
    }

    #[test]
    fn clamp_text_scale_clamps_out_of_range() {
        assert!((clamp_text_scale(0.1) - 0.5).abs() < f32::EPSILON);
        assert!((clamp_text_scale(9.0) - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn clamp_text_scale_rejects_non_finite() {
        assert!((clamp_text_scale(f32::NAN) - 1.0).abs() < f32::EPSILON);
        assert!((clamp_text_scale(f32::INFINITY) - 1.0).abs() < f32::EPSILON);
    }

    // -- provider order <-> choice round trip -------------------------

    #[test]
    fn choice_zero_is_openai_first() {
        let order = order_from_choice(0);
        assert_eq!(order, vec!["openai".to_string(), "anthropic".to_string()]);
        assert_eq!(choice_from_order(&order), 0);
    }

    #[test]
    fn choice_one_is_anthropic_first() {
        let order = order_from_choice(1);
        assert_eq!(order, vec!["anthropic".to_string(), "openai".to_string()]);
        assert_eq!(choice_from_order(&order), 1);
    }

    #[test]
    fn choice_from_order_defaults_to_zero_for_unknown_or_empty() {
        assert_eq!(choice_from_order(&[]), 0);
        assert_eq!(choice_from_order(&["bogus".to_string()]), 0);
    }

    // -- merge_provider_order (#194: Settings save must not drop entries
    // beyond openai/anthropic) -------------------------------------------
    //
    // Rule: `choice` only decides the relative order of openai and
    // anthropic. Every other entry (including duplicates and unrecognized
    // names) keeps its exact position. When both are already present, they
    // simply swap array slots if the current relative order disagrees with
    // `choice`; nothing else moves. When one is missing, it is inserted
    // immediately adjacent to the one that is present, on the correct side.
    // When neither is present, both are appended, in `choice`'s order.

    fn names(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn merge_order_both_present_already_matching_choice_is_unchanged() {
        assert_eq!(
            merge_provider_order(&names(&["openai", "anthropic"]), 0),
            names(&["openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&names(&["anthropic", "openai"]), 1),
            names(&["anthropic", "openai"])
        );
    }

    #[test]
    fn merge_order_both_present_mismatched_choice_swaps_only_those_two_slots() {
        assert_eq!(
            merge_provider_order(&names(&["openai", "anthropic"]), 1),
            names(&["anthropic", "openai"])
        );
        assert_eq!(
            merge_provider_order(&names(&["anthropic", "openai"]), 0),
            names(&["openai", "anthropic"])
        );
    }

    #[test]
    fn merge_order_ollama_first_is_left_in_place_when_ollama_stays_chosen() {
        // The combo now shows Ollama (choice 2) for an Ollama-led order, so an
        // untouched save is choice 2 and changes nothing.
        let configured = names(&["ollama", "anthropic", "openai"]);
        assert_eq!(merge_provider_order(&configured, 2), configured);
    }

    #[test]
    fn merge_order_gemini_in_the_middle_stays_in_the_middle() {
        let configured = names(&["openai", "gemini", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "gemini", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "gemini", "openai"])
        );
    }

    #[test]
    fn merge_order_duplicate_entries_only_the_first_occurrence_participates() {
        let configured = names(&["openai", "openai", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "openai", "openai"])
        );
    }

    #[test]
    fn merge_order_unknown_provider_names_are_untouched() {
        let configured = names(&["mystery-provider", "openai", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["mystery-provider", "anthropic", "openai"])
        );
    }

    #[test]
    fn merge_order_missing_anthropic_inserts_it_adjacent_to_openai() {
        let configured = names(&["ollama", "openai"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "ollama", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "ollama", "openai"])
        );
    }

    #[test]
    fn merge_order_missing_openai_inserts_it_adjacent_to_anthropic() {
        let configured = names(&["anthropic", "ollama"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "anthropic", "ollama"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "openai", "ollama"])
        );
    }

    #[test]
    fn merge_order_neither_present_appends_both_in_choice_order() {
        let configured = names(&["ollama"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "ollama", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "ollama", "openai"])
        );
    }

    #[test]
    fn merge_order_empty_configured_matches_old_order_from_choice_behavior() {
        assert_eq!(
            merge_provider_order(&[], 0),
            names(&["openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&[], 1),
            names(&["anthropic", "openai"])
        );
    }

    // -- #183: Ollama as the active provider ----------------------------

    #[test]
    fn choice_two_is_ollama_first() {
        assert_eq!(choice_from_order(&names(&["ollama", "openai"])), 2);
        assert_eq!(choice_from_order(&names(&["openai", "ollama"])), 0);
    }

    #[test]
    fn merge_order_choice_two_moves_ollama_to_the_front() {
        assert_eq!(
            merge_provider_order(&names(&["openai", "anthropic"]), 2),
            names(&["ollama", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&names(&["anthropic", "openai", "ollama"]), 2),
            names(&["ollama", "anthropic", "openai"])
        );
        assert_eq!(
            merge_provider_order(&names(&["ollama", "openai", "anthropic"]), 2),
            names(&["ollama", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&[], 2),
            names(&["ollama", "openai", "anthropic"])
        );
    }

    #[test]
    fn merge_order_leaving_ollama_puts_the_chosen_cloud_provider_first() {
        let configured = names(&["ollama", "openai", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "ollama", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "ollama", "openai"])
        );
    }

    #[test]
    fn vision_badge_round_trips_through_the_combo_text() {
        assert_eq!(strip_vision_badge("llava:7b (vision)"), "llava:7b");
        assert_eq!(strip_vision_badge("gemma3:4b"), "gemma3:4b");
    }

    fn tag(name: &str, caps: &[&str]) -> ollama_admin::TagsModel {
        ollama_admin::TagsModel {
            name: name.to_string(),
            size: 0,
            details: Default::default(),
            capabilities: caps.iter().map(|c| c.to_string()).collect(),
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cloud_entries_show_a_new_model_and_select_the_current_one() {
        let fetched = strings(&["brand-new", "model-b", "model-a"]);
        let configured = strings(&["model-a", "model-b"]);
        let (labels, sel) = cloud_model_entries(&fetched, &configured, "model-b");
        assert_eq!(labels, ["brand-new", "model-b", "model-a"]);
        assert_eq!(sel, 1);
    }

    #[test]
    fn cloud_entries_keep_a_current_model_the_vendor_did_not_list() {
        let fetched = strings(&["brand-new"]);
        let configured = strings(&["old-listed"]);
        let (labels, sel) = cloud_model_entries(&fetched, &configured, "hand-typed");
        assert_eq!(labels, ["hand-typed", "brand-new", "old-listed"]);
        assert_eq!(sel, 0);
    }

    #[test]
    fn cloud_entries_with_no_current_model_select_the_first() {
        let (labels, sel) = cloud_model_entries(&strings(&["a", "b"]), &[], "");
        assert_eq!(labels, ["a", "b"]);
        assert_eq!(sel, 0);
    }

    #[test]
    fn a_late_post_is_dropped_once_the_window_is_closed() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let alive = AtomicBool::new(true);
        let mut posted = 0;
        assert!(post_while_open(&alive, || posted += 1));
        alive.store(false, Ordering::SeqCst);
        assert!(!post_while_open(&alive, || posted += 1));
        assert_eq!(posted, 1, "the closure must not run after close");
    }

    #[test]
    fn ollama_entries_badge_vision_models_and_select_the_current_one() {
        let models = [
            tag("qwen3:4b", &["completion"]),
            tag("gemma3:4b", &["completion", "vision"]),
        ];
        let (labels, sel) = ollama_model_entries(&models, "gemma3:4b");
        assert_eq!(labels, vec!["qwen3:4b", "gemma3:4b (vision)"]);
        assert_eq!(sel, 1);
    }

    #[test]
    fn ollama_entries_keep_a_current_model_that_is_not_pulled() {
        let models = [tag("qwen3:4b", &["completion"])];
        let (labels, sel) = ollama_model_entries(&models, "gemma3:4b");
        assert_eq!(labels, vec!["gemma3:4b", "qwen3:4b"]);
        assert_eq!(sel, 0);
    }

    #[test]
    fn ollama_entries_with_nothing_pulled_show_only_the_current_model() {
        let (labels, sel) = ollama_model_entries(&[], "gemma3:4b");
        assert_eq!(labels, vec!["gemma3:4b"]);
        assert_eq!(sel, 0);
    }

    #[test]
    fn build_config_applies_the_chosen_ollama_model_without_its_badge() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.ollama_model = "llava:7b (vision)".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.ollama.model, "llava:7b");
    }

    #[test]
    fn build_config_keeps_the_ollama_model_when_the_combo_is_blank() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.ollama_model = "  ".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.ollama.model, original.providers.ollama.model);
    }

    #[test]
    fn build_config_saving_ollama_as_active_puts_it_first_in_the_order() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.provider_choice = 2;
        let cfg = build_config(&original, &raw);
        assert_eq!(
            cfg.providers.order.first().map(String::as_str),
            Some("ollama")
        );
    }

    // -- #195: Gemini fields and active-provider choice ------------------

    #[test]
    fn choice_three_is_gemini_first() {
        assert_eq!(choice_from_order(&names(&["gemini", "openai"])), 3);
        assert_eq!(choice_from_order(&names(&["openai", "gemini"])), 0);
    }

    #[test]
    fn merge_order_choice_three_moves_gemini_to_the_front_and_keeps_the_rest() {
        assert_eq!(
            merge_provider_order(&names(&["openai", "anthropic"]), 3),
            names(&["gemini", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&names(&["anthropic", "gemini", "openai"]), 3),
            names(&["gemini", "anthropic", "openai"])
        );
        assert_eq!(
            merge_provider_order(&[], 3),
            names(&["gemini", "openai", "anthropic"])
        );
    }

    #[test]
    fn merge_order_leaving_gemini_puts_the_chosen_provider_first() {
        let configured = names(&["gemini", "openai", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["openai", "gemini", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["anthropic", "gemini", "openai"])
        );
        assert_eq!(
            merge_provider_order(&configured, 2),
            names(&["ollama", "gemini", "openai", "anthropic"])
        );
    }

    #[test]
    fn an_untouched_save_keeps_a_gemini_led_order() {
        let mut original = Config::default();
        original.providers.order = names(&["gemini", "openai", "anthropic"]);
        let cfg = build_config(&original, &raw_from(&original));
        assert_eq!(cfg.providers.order, original.providers.order);
    }

    #[test]
    fn build_config_applies_an_edited_gemini_model_and_effort() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.gemini_model = "gemini-2.5-pro".to_string();
        raw.gemini_effort = "Thorough".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.gemini.model, "gemini-2.5-pro");
        assert_eq!(cfg.providers.gemini.effort, "high");
    }

    #[test]
    fn build_config_keeps_the_stored_gemini_key_when_the_field_still_shows_the_mask() {
        let mut original = Config::default();
        original.providers.gemini.api_key = "AIza-real-test-key-0000".to_string();
        let mut raw = raw_from(&original);
        raw.gemini_key = key_field_display(&original.providers.gemini.api_key);
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.gemini.api_key, "AIza-real-test-key-0000");
    }

    #[test]
    fn build_config_takes_a_newly_pasted_gemini_key() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.gemini_key = "AIza-pasted-test-key".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.gemini.api_key, "AIza-pasted-test-key");
    }

    // -- mask_key / resolve_key_field (#2 settings masking) ---------------

    #[test]
    fn mask_key_shows_only_the_last_four_characters() {
        assert_eq!(mask_key("sk-abcdefgh1234"), "***********1234");
    }

    #[test]
    fn mask_key_of_empty_key_is_empty() {
        assert_eq!(mask_key(""), "");
    }

    #[test]
    fn mask_key_shorter_than_the_visible_tail_is_shown_in_full() {
        assert_eq!(mask_key("ab"), "ab");
        assert_eq!(mask_key("abcd"), "abcd"); // exactly four
    }

    #[test]
    fn mask_key_just_over_the_visible_tail_masks_one_character() {
        assert_eq!(mask_key("abcde"), "*bcde");
    }

    #[test]
    fn resolve_key_field_keeps_the_real_key_when_the_form_still_shows_the_mask() {
        let original = "sk-abcdefgh1234";
        let form_text = mask_key(original);
        assert_eq!(resolve_key_field(original, &form_text), original);
    }

    #[test]
    fn resolve_key_field_takes_the_typed_value_when_the_form_differs_from_the_mask() {
        let original = "sk-old-key-value";
        assert_eq!(resolve_key_field(original, "sk-new-key"), "sk-new-key");
    }

    #[test]
    fn resolve_key_field_treats_a_cleared_field_as_a_new_empty_key() {
        let original = "sk-old-key-value";
        assert_eq!(resolve_key_field(original, ""), "");
    }

    #[test]
    fn resolve_key_field_on_an_untouched_empty_key_stays_empty() {
        // original == "" -> mask_key("") == "" -> the field's untouched
        // text is also "", which must resolve to "", not be mistaken for
        // an intentional clear of a key that was never set.
        assert_eq!(resolve_key_field("", ""), "");
    }

    // -- #175: the unreadable-credential marker in the settings field -------

    #[test]
    fn key_field_display_of_an_unreadable_marker_is_the_placeholder_not_stars() {
        let shown = key_field_display(crate::config::UNREADABLE_KEY_MARKER);
        assert_eq!(shown, UNREADABLE_KEY_PLACEHOLDER_TEXT);
        assert!(
            !shown.contains('\u{1}'),
            "the raw marker must never reach a visible control: {shown}"
        );
    }

    #[test]
    fn key_field_display_of_a_real_key_still_masks() {
        assert_eq!(
            key_field_display("sk-abcdefgh1234"),
            mask_key("sk-abcdefgh1234")
        );
    }

    #[test]
    fn key_field_display_of_an_empty_key_is_empty() {
        assert_eq!(key_field_display(""), "");
    }

    #[test]
    fn resolve_key_field_keeps_the_marker_when_the_placeholder_is_left_untouched() {
        let original = crate::config::UNREADABLE_KEY_MARKER;
        assert_eq!(
            resolve_key_field(original, UNREADABLE_KEY_PLACEHOLDER_TEXT),
            original,
            "leaving the placeholder alone must not turn into a delete"
        );
    }

    #[test]
    fn resolve_key_field_replaces_the_marker_when_the_user_types_a_new_key() {
        let original = crate::config::UNREADABLE_KEY_MARKER;
        assert_eq!(resolve_key_field(original, "sk-brand-new"), "sk-brand-new");
    }

    #[test]
    fn resolve_key_field_clears_the_marker_when_the_user_empties_the_field() {
        let original = crate::config::UNREADABLE_KEY_MARKER;
        assert_eq!(
            resolve_key_field(original, ""),
            "",
            "an explicit clear over an unreadable marker must still delete the credential"
        );
    }

    // -- build_config --------------------------------------------------

    fn raw_from(original: &Config) -> RawForm {
        RawForm {
            // What the combo would show for this config, so an untouched
            // form is an untouched save.
            provider_choice: choice_from_order(&original.providers.order),
            ollama_model: original.providers.ollama.model.clone(),
            openai_key: original.providers.openai.api_key.clone(),
            openai_model: original.providers.openai.model.clone(),
            openai_effort: effort_display(&original.providers.openai.effort),
            anthropic_key: original.providers.anthropic.api_key.clone(),
            anthropic_model: original.providers.anthropic.model.clone(),
            anthropic_effort: effort_display(&original.providers.anthropic.effort),
            gemini_key: original.providers.gemini.api_key.clone(),
            gemini_model: original.providers.gemini.model.clone(),
            gemini_effort: effort_display(&original.providers.gemini.effort),
            max_edge_text: original.capture.max_edge.to_string(),
            monitor: monitor_display(&original.capture.monitor),
            card_seconds_text: original.ui.card_seconds.to_string(),
            text_scale_raw: original.ui.text_scale,
            prompt: original.ui.prompt.clone(),
            require_tick_for: require_tick_for_label(original.forms.require_tick_for).to_string(),
            card_corner: card_corner_label(original.ui.card_corner).to_string(),
            card_monitor: card_monitor_display(&original.ui.card_monitor, 2),
            redact: original.capture.redact,
        }
    }

    #[test]
    fn build_config_round_trips_unchanged_form() {
        let original = Config::default();
        let raw = raw_from(&original);
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.order, original.providers.order);
        assert_eq!(cfg.providers.openai.model, original.providers.openai.model);
        assert_eq!(cfg.capture.max_edge, original.capture.max_edge);
        assert_eq!(cfg.ui.card_seconds, original.ui.card_seconds);
        assert_eq!(cfg.ui.prompt, original.ui.prompt);
        // Fields the UI never touches must be carried through untouched.
        assert_eq!(cfg.hotkeys, original.hotkeys);
        assert_eq!(
            cfg.providers.openai.models,
            original.providers.openai.models
        );
        assert_eq!(
            cfg.providers.anthropic.models,
            original.providers.anthropic.models
        );
    }

    // -- plain-word label mappings (#341) ----------------------------------

    #[test]
    fn custom_label_round_trips_through_parse_custom_label() {
        assert_eq!(parse_custom_label(&custom_label("999")), Some("999"));
        assert_eq!(
            parse_custom_label(&custom_label("extreme")),
            Some("extreme")
        );
    }

    #[test]
    fn parse_custom_label_rejects_a_plain_preset_label() {
        assert_eq!(parse_custom_label("Fast"), None);
        assert_eq!(parse_custom_label(""), None);
    }

    #[test]
    fn effort_display_round_trips_for_all_presets() {
        for value in EFFORT_VALUES {
            let label = effort_display(value);
            assert_eq!(label_to_effort(&label), Some(value.to_string()));
        }
    }

    // Review of #341: an off-preset value (a hand-edited config.toml) must
    // never be silently rounded to a preset -- an untouched Save has to
    // write back the exact original value.
    #[test]
    fn effort_display_of_an_unrecognized_value_is_an_honest_custom_label() {
        assert_eq!(effort_display("extreme"), "Custom (extreme)");
        assert_eq!(
            label_to_effort("Custom (extreme)"),
            Some("extreme".to_string())
        );
    }

    #[test]
    fn label_to_effort_rejects_unknown_text() {
        assert_eq!(label_to_effort(""), None);
        assert_eq!(label_to_effort("low"), None); // the internal token itself is not a label
    }

    #[test]
    fn max_edge_display_round_trips_for_all_presets() {
        for value in MAX_EDGE_VALUES {
            let label = max_edge_display(value);
            assert_eq!(label_to_max_edge(&label), Some(value));
        }
    }

    #[test]
    fn max_edge_display_of_an_unrecognized_value_is_an_honest_custom_label() {
        assert_eq!(max_edge_display(999), "Custom (999)");
        assert_eq!(label_to_max_edge("Custom (999)"), Some(999));
        assert_eq!(max_edge_display(3000), "Custom (3000)");
        assert_eq!(label_to_max_edge("Custom (3000)"), Some(3000));
    }

    #[test]
    fn label_to_max_edge_rejects_unknown_text() {
        assert_eq!(label_to_max_edge(""), None);
        assert_eq!(label_to_max_edge("2048"), None); // the raw number is not a label
        assert_eq!(label_to_max_edge("Custom (not a number)"), None);
    }

    #[test]
    fn monitor_display_round_trips_for_all_presets() {
        for value in MONITOR_VALUES {
            let label = monitor_display(value);
            assert_eq!(label_to_monitor(&label), Some(value.to_string()));
        }
    }

    #[test]
    fn monitor_display_of_an_unrecognized_value_is_an_honest_custom_label() {
        assert_eq!(monitor_display("laptop-lid"), "Custom (laptop-lid)");
        assert_eq!(
            label_to_monitor("Custom (laptop-lid)"),
            Some("laptop-lid".to_string())
        );
    }

    #[test]
    fn label_to_monitor_rejects_unknown_text() {
        assert_eq!(label_to_monitor(""), None);
        assert_eq!(label_to_monitor("active"), None); // the internal token itself is not a label
    }

    #[test]
    fn build_config_round_trips_effort_max_edge_and_monitor_labels() {
        let mut original = Config::default();
        original.providers.openai.effort = "high".to_string();
        original.providers.anthropic.effort = "low".to_string();
        original.capture.max_edge = 2048;
        original.capture.monitor = "primary".to_string();

        let raw = raw_from(&original);
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.openai.effort, "high");
        assert_eq!(cfg.providers.anthropic.effort, "low");
        assert_eq!(cfg.capture.max_edge, 2048);
        assert_eq!(cfg.capture.monitor, "primary");
    }

    // Review of #341 (data loss): with an off-preset value in all three
    // fields, an untouched Save (raw built straight from `raw_from`, as
    // `read_raw_form` would from the combo's own selected "Custom (...)"
    // entry) must reproduce the exact original values, not the nearest or
    // default preset.
    #[test]
    fn build_config_round_trips_exact_off_preset_values_untouched() {
        let mut original = Config::default();
        original.providers.openai.effort = "extreme".to_string();
        original.providers.anthropic.effort = "barely".to_string();
        original.capture.max_edge = 999;
        original.capture.monitor = "laptop-lid".to_string();

        let raw = raw_from(&original);
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.openai.effort, "extreme");
        assert_eq!(cfg.providers.anthropic.effort, "barely");
        assert_eq!(cfg.capture.max_edge, 999);
        assert_eq!(cfg.capture.monitor, "laptop-lid");
    }

    // Review of #341: picking an actual preset instead must still change
    // the off-preset value to that exact preset (this is not a "never
    // change a custom value" rule -- choosing a preset is a real edit).
    #[test]
    fn build_config_applies_a_chosen_preset_over_an_off_preset_original() {
        let mut original = Config::default();
        original.providers.openai.effort = "extreme".to_string();
        original.capture.max_edge = 999;
        original.capture.monitor = "laptop-lid".to_string();

        let mut raw = raw_from(&original);
        raw.openai_effort = "Thorough".to_string();
        raw.max_edge_text = MAX_EDGE_VALUES[3].to_string(); // "Maximum" preset
        raw.monitor = "The main display".to_string();

        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.openai.effort, "high");
        assert_eq!(cfg.capture.max_edge, 2048);
        assert_eq!(cfg.capture.monitor, "primary");
    }

    // -- capture.redact checkbox (#509) -------------------------------------

    #[test]
    fn build_config_applies_the_redact_checkbox_both_ways() {
        let mut original = Config::default();
        assert!(original.capture.redact);
        let mut raw = raw_from(&original);
        raw.redact = false;
        assert!(!build_config(&original, &raw).capture.redact);
        original.capture.redact = false;
        let mut raw = raw_from(&original);
        raw.redact = true;
        assert!(build_config(&original, &raw).capture.redact);
    }

    #[test]
    fn build_config_round_trips_redact_and_leaves_hand_edited_capture_values() {
        for on in [true, false] {
            let mut original = Config::default();
            original.capture.redact = on;
            original.capture.ocr_language = "de".to_string();
            original.capture.extra_password_managers = vec!["vault.exe".to_string()];
            let cfg = build_config(&original, &raw_from(&original));
            assert_eq!(cfg.capture.redact, on);
            assert_eq!(cfg.capture.ocr_language, "de");
            assert_eq!(cfg.capture.extra_password_managers, vec!["vault.exe"]);
        }
    }

    #[test]
    fn redact_labels_say_the_three_things_without_em_dashes() {
        let all = format!("{REDACT_LABEL} {REDACT_HINT}").to_lowercase();
        assert!(!all.contains('\u{2014}') && !all.contains('\u{2013}'));
        for word in ["screenshot", "text", "password", "answer"] {
            assert!(all.contains(word), "missing {word}: {all}");
        }
    }

    // -- card corner and monitor (#498) -------------------------------------

    const ALL_CORNERS: [CardCorner; 5] = [
        CardCorner::BottomRight,
        CardCorner::BottomLeft,
        CardCorner::TopRight,
        CardCorner::TopLeft,
        CardCorner::FollowMouse,
    ];

    #[test]
    fn card_corner_label_round_trips_for_all_variants() {
        for value in ALL_CORNERS {
            assert_eq!(label_to_card_corner(card_corner_label(value)), Some(value));
        }
        let labels: std::collections::HashSet<_> =
            ALL_CORNERS.iter().map(|&c| card_corner_label(c)).collect();
        assert_eq!(labels.len(), 5, "every corner needs its own label");
    }

    #[test]
    fn label_to_card_corner_rejects_unknown_text() {
        assert_eq!(label_to_card_corner(""), None);
        assert_eq!(label_to_card_corner("garbage"), None);
    }

    #[test]
    fn card_monitor_options_depend_on_the_monitor_count() {
        let values = |n| {
            card_monitor_options(n)
                .into_iter()
                .map(|(_, v)| v)
                .collect::<Vec<_>>()
        };
        assert_eq!(values(0), ["cursor", "primary"]);
        assert_eq!(values(1), ["cursor", "primary"]);
        assert_eq!(values(3), ["cursor", "primary", "1", "2", "3"]);
        let labels: std::collections::HashSet<_> = card_monitor_options(3)
            .into_iter()
            .map(|(l, _)| l)
            .collect();
        assert_eq!(labels.len(), 5, "labels must be distinct");
    }

    #[test]
    fn card_monitor_display_round_trips_every_option() {
        for count in [1, 2, 4] {
            for (label, value) in card_monitor_options(count) {
                assert_eq!(card_monitor_display(&value, count), label);
                assert_eq!(label_to_card_monitor(&label), Some(value));
            }
        }
    }

    #[test]
    fn card_monitor_display_keeps_a_hand_edited_value_honest() {
        // Not an exact preset, or a monitor that is not connected: shown as
        // Custom, and parsed back to the very same text.
        for odd in ["Primary", " 2", "7", "0", "typo", ""] {
            let label = card_monitor_display(odd, 2);
            assert_eq!(label, format!("Custom ({odd})"));
            assert_eq!(label_to_card_monitor(&label), Some(odd.to_string()));
        }
    }

    #[test]
    fn label_to_card_monitor_rejects_unknown_text() {
        assert_eq!(label_to_card_monitor(""), None);
        assert_eq!(label_to_card_monitor("garbage"), None);
    }

    #[test]
    fn build_config_round_trips_card_corner_and_monitor_choices() {
        for corner in ALL_CORNERS {
            let mut raw = raw_from(&Config::default());
            raw.card_corner = card_corner_label(corner).to_string();
            let cfg = build_config(&Config::default(), &raw);
            assert_eq!(cfg.ui.card_corner, corner);
        }
        let mut raw = raw_from(&Config::default());
        raw.card_monitor = card_monitor_display("primary", 2);
        assert_eq!(
            build_config(&Config::default(), &raw).ui.card_monitor,
            "primary"
        );
    }

    #[test]
    fn untouched_save_keeps_a_hand_edited_card_monitor_and_corner() {
        let mut original = Config::default();
        original.ui.card_monitor = "Primary".to_string();
        original.ui.card_corner = CardCorner::TopLeft;
        let cfg = build_config(&original, &raw_from(&original));
        assert_eq!(cfg.ui.card_monitor, "Primary");
        assert_eq!(cfg.ui.card_corner, CardCorner::TopLeft);
    }

    #[test]
    fn blank_card_selections_fall_back_to_the_original() {
        let mut original = Config::default();
        original.ui.card_monitor = "2".to_string();
        original.ui.card_corner = CardCorner::TopRight;
        let mut raw = raw_from(&original);
        raw.card_corner = String::new();
        raw.card_monitor = String::new();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.ui.card_monitor, "2");
        assert_eq!(cfg.ui.card_corner, CardCorner::TopRight);
    }

    // -- require_tick_for (#403) ------------------------------------------

    #[test]
    fn require_tick_for_label_round_trips_for_all_variants() {
        for value in [
            RequireTickFor::All,
            RequireTickFor::Sensitive,
            RequireTickFor::None,
        ] {
            let label = require_tick_for_label(value);
            assert_eq!(label_to_require_tick_for(label), Some(value));
        }
    }

    #[test]
    fn label_to_require_tick_for_rejects_unknown_text() {
        assert_eq!(label_to_require_tick_for(""), None);
        assert_eq!(label_to_require_tick_for("garbage"), None);
    }

    #[test]
    fn build_config_round_trips_require_tick_for_through_all_three_variants() {
        for value in [
            RequireTickFor::All,
            RequireTickFor::Sensitive,
            RequireTickFor::None,
        ] {
            let mut original = Config::default();
            original.forms.require_tick_for = value;
            let raw = raw_from(&original);
            let cfg = build_config(&original, &raw);
            assert_eq!(cfg.forms.require_tick_for, value);
        }
    }

    #[test]
    fn build_config_falls_back_to_original_require_tick_for_on_blank_selection() {
        let mut original = Config::default();
        original.forms.require_tick_for = RequireTickFor::All;
        let mut raw = raw_from(&original);
        raw.require_tick_for = String::new();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.forms.require_tick_for, RequireTickFor::All);
    }

    #[test]
    fn build_config_applies_edited_fields() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.provider_choice = 1;
        raw.openai_key = "sk-new".to_string();
        raw.max_edge_text = "2048".to_string();
        raw.card_seconds_text = "30".to_string();
        raw.text_scale_raw = 1.5;
        raw.prompt = "custom prompt".to_string();

        let cfg = build_config(&original, &raw);
        assert_eq!(
            cfg.providers.order,
            vec!["anthropic".to_string(), "openai".to_string()]
        );
        assert_eq!(cfg.providers.openai.api_key, "sk-new");
        assert_eq!(cfg.capture.max_edge, 2048);
        assert_eq!(cfg.ui.card_seconds, 30);
        assert!((cfg.ui.text_scale - 1.5).abs() < f32::EPSILON);
        assert_eq!(cfg.ui.prompt, "custom prompt");
    }

    #[test]
    fn build_config_preserves_a_hand_added_ollama_entry_on_an_untouched_save() {
        // #194: opening Settings and saving (even a change unrelated to
        // providers) must not silently delete a provider.order entry the
        // user added by hand -- which is also how Ollama is opted into Auto
        // mode (see the comment on `Providers::default`).
        let mut original = Config::default();
        original.providers.order = vec![
            "ollama".to_string(),
            "openai".to_string(),
            "anthropic".to_string(),
        ];
        let raw = raw_from(&original); // provider_choice: 0, nothing else touched
        let cfg = build_config(&original, &raw);
        assert_eq!(
            cfg.providers.order,
            vec![
                "ollama".to_string(),
                "openai".to_string(),
                "anthropic".to_string()
            ],
            "ollama must survive an untouched Settings save"
        );
    }

    #[test]
    fn build_config_falls_back_on_invalid_numeric_input() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.max_edge_text = "not a number".to_string();
        raw.card_seconds_text = "".to_string();

        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.capture.max_edge, original.capture.max_edge);
        assert_eq!(cfg.ui.card_seconds, original.ui.card_seconds);
    }

    #[test]
    fn build_config_falls_back_on_zero_max_edge() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.max_edge_text = "0".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.capture.max_edge, original.capture.max_edge);
    }

    #[test]
    fn build_config_keeps_original_prompt_when_blank() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.prompt = "   ".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.ui.prompt, original.ui.prompt);
    }

    #[test]
    fn build_config_keeps_original_model_and_effort_when_blank() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.openai_model = "".to_string();
        raw.openai_effort = "".to_string();
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.openai.model, original.providers.openai.model);
        assert_eq!(
            cfg.providers.openai.effort,
            original.providers.openai.effort
        );
    }

    // -- to_px --------------------------------------------------------

    #[test]
    fn to_px_is_identity_at_96_dpi() {
        assert_eq!(to_px(100, 96), 100);
    }

    #[test]
    fn to_px_scales_up_at_250_percent() {
        assert_eq!(to_px(100, 240), 250);
    }

    // -- prompt_footer_layout (issue #340) -------------------------------
    // The bug: Save/Cancel are placed from `win_h_dp` alone while the
    // Prompt group's top comes from the fixed content above it, so a
    // shrunk `win_h_dp` (high DPI, small monitor) can put the footer
    // above the Prompt box's actual bottom. These assert the invariant
    // directly: the reset button (the Prompt group's lowest control) must
    // always end above the Save/Cancel row, at both 96dpi's un-shrunk
    // window and 144dpi's (150%) shrunk one.

    /// The y where the Prompt group starts on a real Settings window: the
    /// bottom of the last fixed group (Forms, #403) plus `GROUP_GAP`.
    /// Hand-computed from the constants that drive `build_ui`'s
    /// Providers/Ollama/General/Capture/Card sections with `Config::default()`
    /// (5 + 0 + 4 + 1 + 2 rows respectively, giving 640 -- see git blame for
    /// that derivation), plus the #403 Forms group added after Card and
    /// before Prompt: one row, so its own height is
    /// `GROUP_LABEL_TOP + (ROW_H + ROW_GAP) + 4` (the same `label + rows +
    /// bottom padding` shape every other group in `build_ui` uses) `=
    /// 20 + 30 + 4 = 54`, plus the `GROUP_GAP` (14) that already separated
    /// Card from whatever came next `= 640 + 54 + 14 = 708`. #341 then added
    /// one more row to the *Capture* group itself (the "higher reads small
    /// text better, costs more" hint under Screenshot detail/Screen), which
    /// shifts every group below it, including Forms, down by one
    /// `ROW_H + ROW_GAP = 30`: `708 + 30 = 738`. Kept here as a literal, not
    /// derived, so a change to those sections has to update this test
    /// deliberately rather than silently keep passing against a moving
    /// target.
    const CONTENT_TOP_DP: i32 = 738;

    /// #481 replaced the #340 invariant. The footer no longer has to stay
    /// clear of the Prompt group by arithmetic: it is pinned to the client
    /// bottom and the group lives in the scrolling body, so what matters is
    /// that the body's content is tall enough to hold the whole group (the
    /// scroll range reaches its last control) at any window height.
    fn prompt_group_is_inside_the_scrolling_content(win_h_dp: i32) -> bool {
        let footer = prompt_footer_layout(CONTENT_TOP_DP, win_h_dp);
        let reset_btn_bottom = footer.reset_btn_y + BTN_H;
        reset_btn_bottom <= footer.prompt_bottom && footer.prompt_top == CONTENT_TOP_DP
    }

    #[test]
    fn prompt_group_is_inside_the_scrolling_content_at_96_dpi_full_height() {
        assert!(prompt_group_is_inside_the_scrolling_content(WIN_H_DP));
    }

    #[test]
    fn prompt_group_is_inside_the_scrolling_content_at_144_dpi_shrunk_height() {
        // 144dpi (150%) on a 1080p screen: the fitted window is ~645dp of
        // client height, well below `CONTENT_TOP_DP`: issue #340's and
        // #481's screenshots. The content is taller than the view, so it
        // scrolls instead of being cut off.
        assert!(prompt_group_is_inside_the_scrolling_content(645));
        let f = prompt_footer_layout(CONTENT_TOP_DP, 645);
        assert!(f.content_h() > 645 - FOOTER_H_DP);
    }

    #[test]
    fn prompt_group_is_inside_the_scrolling_content_at_the_minimum_height() {
        assert!(prompt_group_is_inside_the_scrolling_content(MIN_WIN_H_DP));
    }

    #[test]
    fn footer_sits_flush_with_the_window_bottom_when_there_is_room() {
        let roomy_win_h_dp = 950;
        let footer = prompt_footer_layout(CONTENT_TOP_DP, roomy_win_h_dp);
        assert_eq!(footer.buttons_y, roomy_win_h_dp - BTN_H - MARGIN);
    }

    // -- collapsed Advanced group (issue #342) ---------------------------
    // The Check my work prompt is hidden behind a toggle, so the default
    // window is shorter (also helps #481). These cover the pure layout.

    #[test]
    fn collapsed_layout_has_no_edit_box_and_is_shorter_than_expanded() {
        let c = footer_layout(CONTENT_TOP_DP, 0, false);
        let e = footer_layout(CONTENT_TOP_DP, 0, true);
        assert_eq!(c.prompt_edit_h, 0);
        assert!(c.prompt_bottom - c.prompt_top < e.prompt_bottom - e.prompt_top);
        assert!(c.content_h() < e.content_h());
    }

    #[test]
    fn collapsed_toggle_row_sits_inside_the_group_and_above_the_footer() {
        let c = footer_layout(CONTENT_TOP_DP, WIN_H_DP, false);
        assert!(c.reset_btn_y >= c.prompt_top + GROUP_LABEL_TOP);
        assert!(c.reset_btn_y + BTN_H <= c.prompt_bottom);
        assert!(c.content_h() <= WIN_H_DP - FOOTER_H_DP);
    }

    #[test]
    fn expanded_footer_layout_is_the_original_prompt_layout() {
        for h in [MIN_WIN_H_DP, 645, WIN_H_DP, 1200] {
            let a = footer_layout(CONTENT_TOP_DP, h, true);
            let b = prompt_footer_layout(CONTENT_TOP_DP, h);
            assert_eq!(
                (a.prompt_bottom, a.prompt_edit_h, a.reset_btn_y, a.buttons_y),
                (b.prompt_bottom, b.prompt_edit_h, b.reset_btn_y, b.buttons_y)
            );
        }
    }

    #[test]
    fn collapsed_window_needs_less_height_than_the_expanded_prompt_group() {
        // Compare what each state needs, not the fitted window: the
        // expanded group's minimum already ran past `WIN_H_DP` (#481).
        let nc = 40;
        let collapsed = window_height_dp(CONTENT_TOP_DP, false, 5000, nc);
        let e = footer_layout(CONTENT_TOP_DP, 0, true);
        let expanded_needed = e.content_h() + FOOTER_H_DP + nc;
        assert!(collapsed + MIN_PROMPT_EDIT_H <= expanded_needed);
        assert_eq!(
            window_height_dp(CONTENT_TOP_DP, true, WIN_H_DP, nc),
            WIN_H_DP
        );
    }

    #[test]
    fn enter_presses_the_focused_button_and_saves_elsewhere() {
        assert_eq!(enter_command(ID_PROMPT_TOGGLE), ID_PROMPT_TOGGLE);
        assert_eq!(enter_command(ID_RESET_PROMPT), ID_RESET_PROMPT);
        assert_eq!(enter_command(ID_CANCEL), ID_CANCEL);
        assert_eq!(enter_command(ID_OLLAMA_LINK), ID_OLLAMA_LINK);
        assert_eq!(enter_command(ID_SAVE), ID_SAVE);
        assert_eq!(enter_command(ID_CARD_SECONDS), ID_SAVE);
        assert_eq!(enter_command(0), ID_SAVE);
    }

    #[test]
    fn center_in_work_centres_and_never_leaves_the_work_area() {
        let w = windows::Win32::Foundation::RECT {
            left: 0,
            top: 20,
            right: 1920,
            bottom: 1060,
        };
        assert_eq!(center_in_work(&w, 620, 840), (650, 120));
        // Taller than the work area: pinned to the top-left, not above it.
        assert_eq!(center_in_work(&w, 2000, 2000), (0, 20));
    }

    /// Tab order follows creation order: edit, then toggle, then reset, the
    /// same as the visual order when expanded (#342).
    #[test]
    fn prompt_controls_are_created_in_visual_tab_order() {
        let src = include_str!("settings.rs").replace('\r', "");
        let pos = |needle: &str| {
            src.find(needle)
                .unwrap_or_else(|| panic!("missing {needle}"))
        };
        let edit = pos("    let prompt_edit = ctx.create(");
        let toggle = pos("    let toggle_btn = ctx.create(");
        let reset = pos("    let reset_btn = ctx.create(");
        assert!(edit < toggle && toggle < reset);
    }

    #[test]
    fn collapsed_window_never_exceeds_the_screen_fit() {
        assert_eq!(window_height_dp(CONTENT_TOP_DP, false, 500, 40), 500);
    }

    #[test]
    fn collapsed_window_height_holds_exactly_the_body_and_the_footer() {
        let nc = 40;
        let h = window_height_dp(CONTENT_TOP_DP, false, 5000, nc);
        let f = footer_layout(CONTENT_TOP_DP, h - nc, false);
        // No scroll range when it fits: the content is exactly the view.
        assert_eq!(f.content_h() + FOOTER_H_DP, h - nc);
        assert_eq!(f.buttons_y + BTN_H + MARGIN, h - nc);
    }

    // -- scrolling body (issue #481) ---------------------------------------

    #[test]
    fn max_scroll_is_the_overflow_and_never_negative() {
        assert_eq!(max_scroll(900, 600), 300);
        assert_eq!(max_scroll(600, 600), 0);
        assert_eq!(max_scroll(300, 600), 0);
    }

    #[test]
    fn clamp_scroll_keeps_the_position_inside_the_range() {
        assert_eq!(clamp_scroll(-5, 900, 600), 0);
        assert_eq!(clamp_scroll(120, 900, 600), 120);
        assert_eq!(clamp_scroll(999, 900, 600), 300);
        assert_eq!(clamp_scroll(50, 300, 600), 0);
    }

    #[test]
    fn scroll_into_view_leaves_a_visible_rect_alone() {
        assert_eq!(scroll_into_view(100, 1000, 400, 150, 200), 100);
    }

    #[test]
    fn scroll_into_view_brings_a_rect_below_the_view_up_to_the_bottom_edge() {
        assert_eq!(scroll_into_view(0, 1000, 400, 500, 530), 130);
    }

    #[test]
    fn scroll_into_view_brings_a_rect_above_the_view_down_to_the_top_edge() {
        assert_eq!(scroll_into_view(300, 1000, 400, 120, 150), 120);
    }

    #[test]
    fn scroll_into_view_never_leaves_the_range() {
        // Focus on the last control: the target is the very bottom.
        assert_eq!(scroll_into_view(0, 1000, 400, 980, 1000), 600);
        // The first control: the very top.
        assert_eq!(scroll_into_view(500, 1000, 400, 0, 22), 0);
    }

    #[test]
    fn scroll_into_view_aligns_the_top_of_a_rect_taller_than_the_view() {
        assert_eq!(scroll_into_view(0, 2000, 300, 500, 1000), 500);
    }

    #[test]
    fn wheel_scrolls_three_rows_per_notch_against_the_wheel_direction() {
        let row = to_px(ROW_H, 96);
        assert_eq!(wheel_scroll_px(120, 96), -3 * row);
        assert_eq!(wheel_scroll_px(-120, 96), 3 * row);
        assert_eq!(wheel_scroll_px(60, 96), -3 * row / 2);
        assert_eq!(wheel_scroll_px(0, 96), 0);
        // Rows are bigger at 150%.
        assert_eq!(wheel_scroll_px(-120, 144), 3 * to_px(ROW_H, 144));
    }

    #[test]
    fn mouse_messages_are_recognised_and_keys_are_not() {
        assert!(is_mouse_message(0x0201)); // WM_LBUTTONDOWN
        assert!(is_mouse_message(0x0202)); // WM_LBUTTONUP
        assert!(is_mouse_message(0x00A1)); // WM_NCLBUTTONDOWN
        assert!(!is_mouse_message(0x0100)); // WM_KEYDOWN
        assert!(!is_mouse_message(0x020A)); // WM_MOUSEWHEEL
    }

    #[test]
    fn scrollbar_requests_map_to_positions() {
        // (code, pos, track, line, page, max)
        let t = |code, pos| scroll_request_target(code, pos, 77, 20, 300, 500);
        assert_eq!(t(SB_LINEUP, 100), 80);
        assert_eq!(t(SB_LINEDOWN, 100), 120);
        assert_eq!(t(SB_PAGEUP, 100), 0);
        assert_eq!(t(SB_PAGEDOWN, 100), 400);
        assert_eq!(t(SB_THUMBTRACK, 100), 77);
        assert_eq!(t(SB_THUMBPOSITION, 100), 77);
        assert_eq!(t(SB_TOP, 100), 0);
        assert_eq!(t(SB_BOTTOM, 100), 500);
        assert_eq!(t(SB_LINEDOWN, 490), 500);
        assert_eq!(t(SB_LINEUP, 5), 0);
        assert_eq!(t(99, 100), 100);
    }

    #[test]
    fn the_window_never_asks_for_more_height_than_the_work_area_holds() {
        // 1080p at 150%: the 1040px work area is ~693dp, less the title bar
        // allowance. This is the screen in the issue.
        let h = fitted_height_for_work_px(1040, 144);
        assert!(to_px(h, 144) <= 1040, "{h}dp does not fit");
        // A short, very scaled screen must still fit: no floor above it.
        let h = fitted_height_for_work_px(740, 240);
        assert!(to_px(h, 240) <= 740, "{h}dp does not fit");
    }

    #[test]
    fn the_footer_is_pinned_to_the_bottom_whatever_the_content() {
        for h in [300, 645, WIN_H_DP, 1500] {
            for expanded in [false, true] {
                let f = footer_layout(CONTENT_TOP_DP, h, expanded);
                assert_eq!(f.buttons_y, h - BTN_H - MARGIN, "h={h} expanded={expanded}");
            }
        }
    }

    #[test]
    fn content_is_taller_than_a_short_view_and_fills_a_tall_one_exactly() {
        let short = footer_layout(CONTENT_TOP_DP, 600, true);
        assert!(short.content_h() > 600 - FOOTER_H_DP, "must scroll");
        let tall = footer_layout(CONTENT_TOP_DP, 1500, true);
        assert_eq!(tall.content_h(), 1500 - FOOTER_H_DP, "no needless scroll");
    }

    // -- window smoke test ----------------------------------------------
    // Exercises real Win32 window + control creation/destruction, not just
    // compilation, modeled on card.rs's own smoke test. No window is ever
    // shown interactively and no message loop is pumped, so this is safe
    // and fast to run under `cargo test`.
    #[test]
    fn create_and_destroy_settings_window_smoke_test() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);

        ensure_test_class(instance);
        ensure_common_controls();

        let config = Config::default();
        let inner = Box::new(SettingsInner::new(config.clone()));
        let raw = Box::into_raw(inner);

        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (test)");
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0), // not visible
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");

        assert!(!hwnd.0.is_null());

        let inner_ref = unsafe { &mut *raw };
        inner_ref.hwnd = hwnd;
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        inner_ref.font = build_font(dpi);
        assert!(!inner_ref.font.0.is_null());

        let prompt_edit = build_ui(hwnd, instance, dpi, inner_ref.font, &config, WIN_H_DP);
        assert!(!prompt_edit.0.is_null());
        assert_eq!(get_text(prompt_edit), config.ui.prompt);

        // Exercise a value round trip through the real controls.
        set_text(prompt_edit, "hello from a test");
        assert_eq!(get_text(prompt_edit), "hello from a test");

        assert!(get_dlg_item(hwnd, ID_SAVE).is_some());
        assert!(get_dlg_item(hwnd, ID_CANCEL).is_some());

        // Pasting a full-length API key must not be truncated.
        //
        // This has to go through the real input path (clipboard + WM_PASTE).
        // `SetWindowTextW` bypasses the length enforcement that a single-line
        // edit applies to user input, which is exactly how the missing
        // ES_AUTOHSCROLL shipped: the programmatic round-trip below passed
        // while real pasting silently dropped the tail of the key.
        {
            let key = format!("sk-ant-api03-{}", "A".repeat(95));
            let field = get_dlg_item(hwnd, ID_ANTHROPIC_KEY).expect("anthropic key field");
            set_text(field, "");
            if put_on_clipboard(hwnd, &key) {
                unsafe {
                    SendMessageW(field, WM_PASTE, None, None);
                }
                let got = get_text(field);
                assert_eq!(
                    got.chars().count(),
                    key.chars().count(),
                    "pasted key was truncated: {} of {} chars survived",
                    got.chars().count(),
                    key.chars().count()
                );
                assert_eq!(got, key);
            }
            set_text(field, "");
        }

        // Typing an API key into each field and saving must actually persist
        // both. This is the whole point of the window, and it is the one path
        // that cannot be checked by looking at a screenshot.
        let openai_edit = get_dlg_item(hwnd, ID_OPENAI_KEY).expect("openai key field exists");
        let anthropic_edit =
            get_dlg_item(hwnd, ID_ANTHROPIC_KEY).expect("anthropic key field exists");
        set_text(openai_edit, "sk-openai-typed");
        set_text(anthropic_edit, "sk-ant-typed");
        assert_eq!(get_text(openai_edit), "sk-openai-typed");
        assert_eq!(get_text(anthropic_edit), "sk-ant-typed");

        let saved = read_form(inner_ref);
        assert_eq!(
            saved.providers.openai.api_key, "sk-openai-typed",
            "the OpenAI key typed into the form must reach the config"
        );
        assert_eq!(
            saved.providers.anthropic.api_key, "sk-ant-typed",
            "the Anthropic key typed into the form must reach the config"
        );

        // Dropping/destroying must not panic (exercises WM_DESTROY/WM_NCDESTROY).
        unsafe {
            let _ = DestroyWindow(hwnd);
        }
        let inner = unsafe { Box::from_raw(raw) };
        drop(inner);
    }

    /// #2: the real Win32 control must never be populated with the live
    /// key. This is the "wired to nothing" check for `mask_key` -- a unit
    /// test on the pure function proves the math, but only building the
    /// real control and reading its text back proves `build_ui` actually
    /// calls it instead of the raw field.
    #[test]
    fn build_ui_populates_the_key_field_with_the_masked_value_not_the_real_key() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let mut config = Config::default();
        config.providers.openai.api_key = "sk-live-secret-should-not-appear-1234".to_string();

        let title = wide_z("Wingman settings (mask test)");
        let class_name = wide_z(TEST_CLASS_NAME);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");

        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        let font = build_font(dpi);
        build_ui(hwnd, instance, dpi, font, &config, WIN_H_DP);

        let openai_edit = get_dlg_item(hwnd, ID_OPENAI_KEY).expect("openai key field exists");
        let shown = get_text(openai_edit);
        assert_eq!(shown, mask_key(&config.providers.openai.api_key));
        assert!(
            !shown.contains("sk-live-secret-should-not-appear-1234"),
            "the real key must never reach the control's text: {shown}"
        );
        assert!(
            shown.ends_with("1234"),
            "the last four characters must still be visible: {shown}"
        );

        // #302: this window has no SettingsInner, so WM_DESTROY never frees
        // the font; do it here.
        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// #498's "wired to nothing" check: the two card combos exist in a real
    /// window, show the stored values (a hand-edited one as Custom), and
    /// read back through `combo_selected_text`.
    #[test]
    fn build_ui_shows_the_card_corner_and_display_from_the_config() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let mut config = Config::default();
        config.ui.card_corner = CardCorner::TopLeft;
        config.ui.card_monitor = "Primary".to_string();

        let title = wide_z("Wingman settings (card position test)");
        let class_name = wide_z(TEST_CLASS_NAME);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");

        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        let font = build_font(dpi);
        build_ui(hwnd, instance, dpi, font, &config, WIN_H_DP);

        assert_eq!(combo_selected_text(hwnd, ID_CARD_CORNER), "Top left");
        assert_eq!(
            combo_selected_text(hwnd, ID_CARD_MONITOR),
            "Custom (Primary)"
        );

        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// #175's "wired to nothing" check for `key_field_display`: the real
    /// control must show the human-readable placeholder, never the raw
    /// marker (whose control characters would render as garbage, or worse,
    /// silently vanish and leave the field looking empty).
    #[test]
    fn build_ui_shows_the_unreadable_placeholder_not_the_raw_marker() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let mut config = Config::default();
        config.providers.openai.api_key = crate::config::UNREADABLE_KEY_MARKER.to_string();

        let title = wide_z("Wingman settings (unreadable-marker test)");
        let class_name = wide_z(TEST_CLASS_NAME);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");

        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        let font = build_font(dpi);
        build_ui(hwnd, instance, dpi, font, &config, WIN_H_DP);

        let openai_edit = get_dlg_item(hwnd, ID_OPENAI_KEY).expect("openai key field exists");
        let shown = get_text(openai_edit);
        assert_eq!(shown, UNREADABLE_KEY_PLACEHOLDER_TEXT);
        assert!(
            !shown.contains('\u{1}'),
            "the raw marker's control characters must never reach the real control: {shown:?}"
        );

        // #302: this window has no SettingsInner, so WM_DESTROY never frees
        // the font; do it here.
        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    // -- WM_DPICHANGED (#233) ---------------------------------------------

    #[test]
    fn dpi_from_wparam_takes_the_low_word_only() {
        assert_eq!(dpi_from_wparam(144 | (144 << 16)), 144);
        assert_eq!(dpi_from_wparam(192 | (96 << 16)), 192);
    }

    #[test]
    fn dpi_from_wparam_never_returns_zero() {
        assert_eq!(dpi_from_wparam(0), 1);
        assert_eq!(dpi_from_wparam(0xFFFF_0000), 1);
    }

    #[test]
    fn suggested_pos_is_origin_and_extent_of_the_rect() {
        let r = windows::Win32::Foundation::RECT {
            left: -1920,
            top: 40,
            right: -1920 + 1550,
            bottom: 40 + 2295,
        };
        assert_eq!(suggested_pos(&r), (-1920, 40, 1550, 2295));
    }

    #[test]
    fn suggested_pos_floors_a_degenerate_rect_at_one_pixel() {
        let r = windows::Win32::Foundation::RECT {
            left: 10,
            top: 10,
            right: 10,
            bottom: 5,
        };
        assert_eq!(suggested_pos(&r), (10, 10, 1, 1));
    }

    fn child(x: i32, y: i32, w: i32, h: i32, anchor: Anchor) -> ChildLayout {
        ChildLayout {
            hwnd: HWND(std::ptr::null_mut()),
            x,
            y,
            w,
            h,
            anchor,
        }
    }

    #[test]
    fn child_rect_px_scales_a_fixed_child_with_the_dpi() {
        let l = prompt_footer_layout(700, 918);
        let c = child(16, 40, 100, 22, Anchor::Fixed);
        assert_eq!(child_rect_px(&c, &l, &l, 96), (16, 40, 100, 22));
        assert_eq!(child_rect_px(&c, &l, &l, 192), (32, 80, 200, 44));
        assert_eq!(child_rect_px(&c, &l, &l, 144), (24, 60, 150, 33));
    }

    #[test]
    fn child_rect_px_moves_the_footer_with_a_shorter_window() {
        let old = prompt_footer_layout(700, 918);
        let new = prompt_footer_layout(700, 800);
        let c = child(500, old.buttons_y, 100, 28, Anchor::Footer);
        let (_, y, _, h) = child_rect_px(&c, &old, &new, 96);
        assert_eq!(y, new.buttons_y);
        assert_eq!(h, 28);
    }

    #[test]
    fn child_rect_px_stretches_the_prompt_group_edit_and_reset_row() {
        let old = prompt_footer_layout(700, 918);
        let new = prompt_footer_layout(700, 1000);
        let group = child(
            16,
            old.prompt_top,
            588,
            old.prompt_bottom - old.prompt_top,
            Anchor::PromptGroup,
        );
        let edit = child(32, 720, 556, old.prompt_edit_h, Anchor::PromptEdit);
        let reset = child(32, old.reset_btn_y, 200, 28, Anchor::ResetBtn);
        assert_eq!(
            child_rect_px(&group, &old, &new, 96).3,
            new.prompt_bottom - new.prompt_top
        );
        assert_eq!(child_rect_px(&edit, &old, &new, 96).3, new.prompt_edit_h);
        assert_eq!(child_rect_px(&reset, &old, &new, 96).1, new.reset_btn_y);
    }

    fn font_height_of(hwnd: HWND) -> i32 {
        use windows::Win32::Graphics::Gdi::GetObjectW;
        use windows::Win32::UI::WindowsAndMessaging::WM_GETFONT;
        let f = unsafe { SendMessageW(hwnd, WM_GETFONT, None, None) }.0;
        let mut lf = LOGFONTW::default();
        let got = unsafe {
            GetObjectW(
                HGDIOBJ(f as *mut c_void),
                std::mem::size_of::<LOGFONTW>() as i32,
                Some(&mut lf as *mut LOGFONTW as *mut c_void),
            )
        };
        assert!(got > 0, "the control has no font");
        lf.lfHeight.abs()
    }

    fn client_size(hwnd: HWND) -> (i32, i32) {
        let mut rc = windows::Win32::Foundation::RECT::default();
        unsafe { GetClientRect(hwnd, &mut rc) }.expect("GetClientRect");
        (rc.right - rc.left, rc.bottom - rc.top)
    }

    /// #233's wired-to-nothing check, with a real window: a test-only class
    /// over the production `wndproc`, the real `build_ui_recorded`, then a
    /// real `WM_DPICHANGED` message. If the arm were missing or wired to
    /// nothing, the Save button and the fonts would keep their 96dpi size.
    #[test]
    fn wm_dpichanged_rescales_fonts_and_children_of_a_real_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (dpichanged test)");
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU,
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");

        // Cleanup runs even if an assertion below fails.
        struct Guard(HWND, *mut SettingsInner);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                    drop(Box::from_raw(self.1));
                }
            }
        }
        let _guard = Guard(hwnd, raw);

        // Short-lived access only: the wndproc mutates the same struct
        // during SendMessageW, so no `&mut` is held across a send.
        let client_h_dp = client_size(hwnd).1;
        let font = build_font(96);
        let built = build_ui_recorded(hwnd, instance, 96, font, &Config::default(), client_h_dp);
        {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
        }
        let child_count = unsafe { &*raw }.children.len();
        assert_children_split_between_footer_and_body(hwnd, raw, child_count);

        let save = get_dlg_item(hwnd, ID_SAVE).expect("Save button");
        assert_eq!(client_size(save), (BTN_W, BTN_H));
        let font_96 = font_height_of(save);

        // Moving to a 200% monitor: the system suggests a window twice the
        // size at the same spot.
        let mut wr = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(hwnd, &mut wr) }.expect("GetWindowRect");
        let suggested = windows::Win32::Foundation::RECT {
            left: wr.left,
            top: wr.top,
            right: wr.left + 2 * (wr.right - wr.left),
            bottom: wr.top + 2 * (wr.bottom - wr.top),
        };
        unsafe {
            SendMessageW(
                hwnd,
                WM_DPICHANGED,
                Some(WPARAM(192 | (192 << 16))),
                Some(LPARAM(&suggested as *const _ as isize)),
            );
        }

        assert_eq!(client_size(save), (to_px(BTN_W, 192), to_px(BTN_H, 192)));
        let prompt = get_dlg_item(hwnd, ID_PROMPT_EDIT).expect("prompt edit");
        assert_eq!(font_height_of(prompt), font_height_of(save));
        assert!(
            font_height_of(save) > font_96 * 3 / 2,
            "font must grow with the dpi: {font_96} -> {}",
            font_height_of(save)
        );
        // The window may have been clamped to this machine's work area, so
        // only the width is compared to the suggestion, and Windows also caps
        // any window at the max track size (about the screen width).
        // MEASURED 2026-10-02: the CI runner's ~1024px screen capped the
        // doubled 1240px suggestion at 1044.
        let mut wr2 = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(hwnd, &mut wr2) }.expect("GetWindowRect");
        let max_track_w = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CXMAXTRACK,
            )
        };
        assert_eq!(
            wr2.right - wr2.left,
            (suggested.right - suggested.left).min(max_track_w)
        );
        assert_eq!(unsafe { &*raw }.dpi, 192);
        assert_eq!(unsafe { &*raw }.children.len(), child_count);
        assert_children_split_between_footer_and_body(hwnd, raw, child_count);

        // Save's top edge sits where the footer layout for the new client
        // height puts it, in dp at the new dpi.
        let client_dp = (client_size(hwnd).1 * 96 / 192).max(1);
        let footer = footer_layout(unsafe { &*raw }.content_top_dp, client_dp, false);
        assert_eq!(top_in_parent_px(hwnd, save), to_px(footer.buttons_y, 192));

        // And back to 100%.
        let back = wr;
        unsafe {
            SendMessageW(
                hwnd,
                WM_DPICHANGED,
                Some(WPARAM(96 | (96 << 16))),
                Some(LPARAM(&back as *const _ as isize)),
            );
        }
        assert_eq!(client_size(save), (BTN_W, BTN_H));
        assert_eq!(font_height_of(save), font_96);
    }

    /// #481's wired-to-nothing check, with a real window: one far shorter than
    /// its content. If the body were not scrolling (no range, no `WM_VSCROLL`
    /// arm, children not moved) the Prompt toggle would stay below the view
    /// after the scroll message; if the footer were inside the body Save
    /// would move with it.
    #[test]
    fn a_short_window_scrolls_its_body_and_keeps_the_footer_pinned() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (scroll test)");
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(400, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");
        struct Guard(HWND, *mut SettingsInner);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                    drop(Box::from_raw(self.1));
                }
            }
        }
        let _guard = Guard(hwnd, raw);

        let client_h_px = client_size(hwnd).1;
        let font = build_font(96);
        let built = build_ui_recorded(
            hwnd,
            instance,
            96,
            font,
            &Config::default(),
            client_h_px.max(1),
        );
        {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
            unsafe { apply_dpi(inner) };
        }
        let body = unsafe { &*raw }.body;
        let view_px = client_h_px - to_px(FOOTER_H_DP, 96);

        // The range covers the content; the body fills the client above the
        // footer and Save sits below it, inside the window.
        let si = unsafe { body_scroll_info(body) };
        assert_eq!(si.nPage as i32, view_px);
        assert!(
            si.nMax + 1 > view_px,
            "content {} must overflow the {view_px}px view",
            si.nMax + 1
        );
        let save = get_dlg_item(hwnd, ID_SAVE).expect("Save");
        let save_y = top_in_parent_px(hwnd, save);
        assert!(save_y >= view_px, "Save {save_y} overlaps the body");
        assert!(save_y + to_px(BTN_H, 96) <= client_h_px, "Save is cut off");

        // The Advanced toggle starts below the view and a scroll to the
        // bottom (what the scrollbar's End does) brings it into the view.
        let toggle = get_dlg_item(hwnd, ID_PROMPT_TOGGLE).expect("toggle");
        assert!(top_in_parent_px(body, toggle) >= view_px, "starts in view");
        unsafe {
            SendMessageW(
                body,
                WM_VSCROLL,
                Some(WPARAM(SB_BOTTOM as usize)),
                Some(LPARAM(0)),
            );
        }
        let t = top_in_parent_px(body, toggle);
        assert!(t >= 0 && t + to_px(BTN_H, 96) <= view_px, "toggle at {t}");
        assert_eq!(top_in_parent_px(hwnd, save), save_y, "the footer moved");
        assert_eq!(
            unsafe { body_scroll_info(body) }.nPos,
            si.nMax + 1 - view_px
        );

        // The wheel (as the message loop passes it) scrolls back up.
        let wheel = |delta: i16| MSG {
            hwnd: toggle,
            message: WM_MOUSEWHEEL,
            wParam: WPARAM((delta as u16 as usize) << 16),
            ..Default::default()
        };
        let before = unsafe { body_scroll_info(body) }.nPos;
        assert!(unsafe { scroll_with_wheel(hwnd, &wheel(120), HWND(std::ptr::null_mut())) });
        let after = unsafe { body_scroll_info(body) }.nPos;
        assert_eq!(after, before + wheel_scroll_px(120, 96));
        // ...but the prompt box keeps its own wheel.
        let prompt = get_dlg_item(hwnd, ID_PROMPT_EDIT).expect("prompt");
        // The prompt is judged by the window under the cursor, not the
        // focused one (parallel tests overlap windows, so the pointer's
        // window is passed in here rather than found with WindowFromPoint).
        assert!(wheel_is_over_prompt(prompt, prompt));
        assert!(!wheel_is_over_prompt(toggle, prompt));
        // An open combo keeps the wheel; a closed one does not.
        let combo = get_dlg_item(hwnd, ID_ACTIVE_PROVIDER).expect("combo");
        assert!(!unsafe { wheel_over_open_combo(combo) });
        unsafe { SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(1)), None) };
        assert!(unsafe { wheel_over_open_combo(combo) });
        unsafe { SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(0)), None) };

        // Focus on the first control scrolls the top back into view, and a
        // control already in view does not move it.
        let first = get_dlg_item(hwnd, ID_ACTIVE_PROVIDER).expect("first field");
        unsafe { follow_focus(hwnd, first) };
        assert_eq!(unsafe { body_scroll_info(body) }.nPos, 0);
        let first_y = top_in_parent_px(body, first);
        unsafe { follow_focus(hwnd, first) };
        assert_eq!(top_in_parent_px(body, first), first_y);
        // And the last one scrolls down again. A combo's inner edit counts as
        // the combo.
        unsafe { follow_focus(hwnd, toggle) };
        let t = top_in_parent_px(body, toggle);
        assert!(t >= 0 && t + to_px(BTN_H, 96) <= view_px, "toggle at {t}");
    }

    /// #342's wired-to-nothing check, with a real window: the prompt box is
    /// hidden until the toggle is clicked (a real `WM_COMMAND` through the
    /// production `wndproc`), clicking again hides it, and its text survives
    /// both, so Save would still read it. If the toggle arm were missing the
    /// edit would stay hidden after the first click.
    #[test]
    fn prompt_toggle_shows_and_hides_the_prompt_of_a_real_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (prompt toggle test)");
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");
        struct Guard(HWND, *mut SettingsInner);
        impl Drop for Guard {
            fn drop(&mut self) {
                unsafe {
                    let _ = DestroyWindow(self.0);
                    drop(Box::from_raw(self.1));
                }
            }
        }
        let _guard = Guard(hwnd, raw);

        let client_h_dp = client_size(hwnd).1;
        let font = build_font(96);
        let built = build_ui_recorded(hwnd, instance, 96, font, &Config::default(), client_h_dp);
        {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
        }
        let prompt = get_dlg_item(hwnd, ID_PROMPT_EDIT).expect("prompt edit");
        let reset = get_dlg_item(hwnd, ID_RESET_PROMPT).expect("reset button");
        let toggle = get_dlg_item(hwnd, ID_PROMPT_TOGGLE).expect("toggle button");
        let save = get_dlg_item(hwnd, ID_SAVE).expect("save");
        let visible = |w: HWND| unsafe { IsWindowVisible(w) }.as_bool();
        let click = || unsafe {
            SendMessageW(
                hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PROMPT_TOGGLE as usize)),
                Some(LPARAM(0)),
            );
        };

        // Collapsed at start: no prompt text on screen, the toggle is.
        unsafe { show_prompt_state(&mut *raw) };
        assert!(!visible(prompt) && !visible(reset));
        assert!(visible(toggle));
        let collapsed_save_y = top_in_parent_px(hwnd, save);
        let body = unsafe { &*raw }.body;
        let content = || unsafe { body_scroll_info(body) }.nMax;
        let collapsed_content = content();

        click();
        assert!(visible(prompt) && visible(reset));
        assert_eq!(get_text(toggle), "Hide Check my work &prompt");
        // #481: Save is pinned to the bottom, so the window may not grow on
        // a short screen; the scrolling content is what grows.
        assert!(top_in_parent_px(hwnd, save) >= collapsed_save_y);
        assert!(content() > collapsed_content, "the body did not grow");
        assert_eq!(get_text(prompt), Config::default().ui.prompt);

        click();
        assert!(!visible(prompt) && !visible(reset));
        assert_eq!(get_text(toggle), "Show Check my work &prompt");
        assert_eq!(top_in_parent_px(hwnd, save), collapsed_save_y);
        assert_eq!(content(), collapsed_content);
        // Hidden text is still readable, so Save keeps it.
        assert_eq!(get_text(prompt), Config::default().ui.prompt);
    }

    /// #481: the window's direct children are the pinned footer controls and
    /// the one body; every other recorded control lives inside the body.
    fn assert_children_split_between_footer_and_body(
        hwnd: HWND,
        raw: *mut SettingsInner,
        child_count: usize,
    ) {
        let inner = unsafe { &*raw };
        let footer = inner
            .children
            .iter()
            .filter(|c| c.anchor == Anchor::Footer)
            .count();
        assert!(footer >= 3, "Save, Cancel and the validation line");
        assert!(!inner.body.0.is_null(), "no scrolling body");
        assert_eq!(direct_child_count(hwnd), footer + 1);
        assert_eq!(direct_child_count(inner.body), child_count - footer);
    }

    fn direct_child_count(hwnd: HWND) -> usize {
        use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_CHILD, GW_HWNDNEXT};
        let mut n = 0;
        let mut cur = unsafe { GetWindow(hwnd, GW_CHILD) }.ok();
        while let Some(c) = cur {
            n += 1;
            cur = unsafe { GetWindow(c, GW_HWNDNEXT) }.ok();
        }
        n
    }

    fn top_in_parent_px(parent: HWND, child: HWND) -> i32 {
        use windows::Win32::Graphics::Gdi::ScreenToClient;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut r = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(child, &mut r) }.expect("GetWindowRect");
        let mut pt = windows::Win32::Foundation::POINT {
            x: r.left,
            y: r.top,
        };
        assert!(unsafe { ScreenToClient(parent, &mut pt) }.as_bool());
        pt.y
    }

    // -- clamp_suggested_to_work (review of #233) -------------------------

    fn work(top: i32, bottom: i32) -> windows::Win32::Foundation::RECT {
        windows::Win32::Foundation::RECT {
            left: 0,
            top,
            right: 1920,
            bottom,
        }
    }

    #[test]
    fn clamp_leaves_a_suggestion_that_fits_untouched() {
        // 100% 1440p-ish work area is tall: nothing to clamp.
        let pos = (100, 100, 620, 900);
        assert_eq!(clamp_suggested_to_work(pos, &work(0, 1400), 96), pos);
    }

    #[test]
    fn clamp_shrinks_a_tall_suggestion_to_a_200_percent_1080p_work_area() {
        // 1040px work area at 200%: 1040*96/192 - 48 = 472dp -> 944px.
        let (x, y, w, h) = clamp_suggested_to_work((50, 0, 1240, 1836), &work(0, 1040), 192);
        assert_eq!((x, w), (50, 1240));
        assert_eq!(h, to_px(472, 192));
        assert!(y + h <= 1040, "bottom {} is below the work area", y + h);
    }

    #[test]
    fn clamp_slides_the_window_up_when_its_bottom_would_leave_the_work_area() {
        let (_, y, _, h) = clamp_suggested_to_work((0, 900, 620, 600), &work(0, 1040), 96);
        assert_eq!(h, 600);
        assert_eq!(y, 1040 - 600);
    }

    #[test]
    fn clamp_never_moves_above_the_work_area_top() {
        let (_, y, _, _) = clamp_suggested_to_work((0, -500, 620, 600), &work(0, 1040), 96);
        assert!(y >= -500);
        let (_, y2, _, h2) = clamp_suggested_to_work((0, 30, 620, 5000), &work(20, 1060), 96);
        assert!(y2 >= 20 && y2 + h2 <= 1060);
    }

    #[test]
    fn clamp_ignores_an_empty_work_area() {
        let pos = (1, 2, 3, 4);
        assert_eq!(clamp_suggested_to_work(pos, &work(0, 0), 96), pos);
    }

    #[test]
    fn second_show_modal_call_while_one_is_open_returns_none() {
        // Simulate "already open" without actually creating a window: set
        // the guard directly, exactly like a real open would, and confirm
        // show_modal's early-return path is taken (no window is created,
        // no panic, no hang).
        let prev = OPEN_HWND.swap(1, Ordering::SeqCst);
        let h = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
            .expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let result = show_modal(instance, &Config::default());
        assert!(result.is_none());
        OPEN_HWND.store(prev, Ordering::SeqCst);
    }

    // -- #345: Change buttons for the two shortcuts ------------------------

    fn chord(vk: u32, ctrl: bool, shift: bool, alt: bool, win: bool) -> Chord {
        Chord {
            vk,
            ctrl,
            shift,
            alt,
            win,
        }
    }

    #[test]
    fn learned_free_chord_applies() {
        let h = Hotkeys::default();
        let c = chord(b'W' as u32, true, false, true, false);
        assert_eq!(
            judge_learned(&h, None, HK_PRIMARY, c),
            LearnedVerdict::Apply
        );
    }

    #[test]
    fn learned_bare_escape_cancels() {
        let h = Hotkeys::default();
        let esc = chord(0x1B, false, false, false, false);
        assert_eq!(
            judge_learned(&h, None, HK_PRIMARY, esc),
            LearnedVerdict::Cancel
        );
        // Escape with a modifier is an ordinary chord, not a cancel.
        let ctrl_esc = chord(0x1B, true, false, false, false);
        assert_ne!(
            judge_learned(&h, None, HK_PRIMARY, ctrl_esc),
            LearnedVerdict::Cancel
        );
    }

    #[test]
    fn learned_bare_typing_key_is_refused_but_a_bare_function_key_is_fine() {
        let h = Hotkeys::default();
        for vk in [b'A' as u32, 0x0D, 0x20] {
            assert!(
                matches!(
                    judge_learned(&h, None, HK_PRIMARY, chord(vk, false, false, false, false)),
                    LearnedVerdict::Refuse(_)
                ),
                "vk {vk:#x}"
            );
        }
        // Shift alone with a letter would swallow capitals everywhere.
        assert!(matches!(
            judge_learned(
                &h,
                None,
                HK_PRIMARY,
                chord(b'A' as u32, false, true, false, false)
            ),
            LearnedVerdict::Refuse(_)
        ));
        assert_eq!(
            judge_learned(
                &h,
                None,
                HK_PRIMARY,
                chord(0x7A, false, false, false, false)
            ),
            LearnedVerdict::Apply
        );
    }

    #[test]
    fn learned_chord_already_on_the_other_slot_is_refused_inline() {
        let h = Hotkeys::default();
        match judge_learned(&h, None, HK_PRIMARY, h.secondary) {
            LearnedVerdict::Refuse(text) => {
                assert!(text.contains("secondary key"), "{text}");
                assert!(!text.contains('\u{2014}'), "no em dash: {text}");
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        match judge_learned(&h, None, HK_SECONDARY, h.primary) {
            LearnedVerdict::Refuse(text) => assert!(text.contains("Copilot key"), "{text}"),
            other => panic!("expected Refuse, got {other:?}"),
        }
    }

    #[test]
    fn learned_chord_on_the_pause_or_palette_slot_is_refused() {
        let mut h = Hotkeys::default();
        let p = chord(0x13, true, false, false, false);
        let q = chord(b'Q' as u32, true, false, true, false);
        h.pause = Some(p);
        h.palette = Some(q);
        assert!(matches!(
            judge_learned(&h, None, HK_PRIMARY, p),
            LearnedVerdict::Refuse(t) if t.contains("pause key")
        ));
        assert!(matches!(
            judge_learned(&h, None, HK_SECONDARY, q),
            LearnedVerdict::Refuse(t) if t.contains("palette key")
        ));
    }

    #[test]
    fn learning_a_slots_own_current_chord_is_not_a_clash() {
        let h = Hotkeys::default();
        assert_eq!(
            judge_learned(&h, None, HK_PRIMARY, h.primary),
            LearnedVerdict::Apply
        );
    }

    #[test]
    fn learned_system_chord_warns_first_and_applies_on_the_second_press() {
        let h = Hotkeys::default();
        let win_d = chord(b'D' as u32, false, false, false, true);
        let first = judge_learned(&h, None, HK_PRIMARY, win_d);
        match first {
            LearnedVerdict::Warn(ref text) => {
                assert!(text.contains("Show desktop"), "{text}");
                assert!(!text.contains('\u{2014}'), "no em dash: {text}");
            }
            ref other => panic!("expected Warn, got {other:?}"),
        }
        assert_eq!(
            judge_learned(&h, Some((HK_PRIMARY, win_d)), HK_PRIMARY, win_d),
            LearnedVerdict::Apply
        );
        // A different slot's warning does not confirm this one.
        assert!(matches!(
            judge_learned(&h, Some((HK_SECONDARY, win_d)), HK_PRIMARY, win_d),
            LearnedVerdict::Warn(_)
        ));
    }

    #[test]
    fn learning_is_refused_with_a_reason_when_it_cannot_work() {
        assert!(learn_unavailable_reason(false, true).is_none());
        assert!(learn_unavailable_reason(true, true)
            .unwrap()
            .contains("paused"));
        assert!(learn_unavailable_reason(false, false)
            .unwrap()
            .contains("keyboard hook"));
    }

    #[test]
    fn save_carries_the_staged_hotkeys_and_a_fresh_window_stages_the_originals() {
        let original = Config::default();
        let mut inner = SettingsInner::new(original.clone());
        assert_eq!(inner.hotkeys, original.hotkeys);
        let new = chord(b'W' as u32, true, false, true, false);
        apply_learned(&mut inner.hotkeys, HK_PRIMARY, new);
        let raw = raw_from(&original);
        let saved = finalize_config(&inner, &raw);
        assert_eq!(saved.hotkeys.primary, new);
        assert_eq!(saved.hotkeys.secondary, original.hotkeys.secondary);
    }

    /// The real controls: a Change button per shortcut, the labels show the
    /// chords, the old "rebind from the tray" hint is gone, and updating the
    /// labels from a staged change reaches the label controls.
    #[test]
    fn build_ui_has_change_buttons_and_labels_follow_the_staged_hotkeys() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let config = Config::default();
        let title = wide_z("Wingman settings (hotkey buttons test)");
        let class_name = wide_z(TEST_CLASS_NAME);
        let hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                to_px(WIN_W_DP, 96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");
        let dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        let font = build_font(dpi);
        build_ui(hwnd, instance, dpi, font, &config, WIN_H_DP);

        for id in [ID_CHANGE_PRIMARY, ID_CHANGE_SECONDARY] {
            assert!(get_dlg_item(hwnd, id).is_some(), "Change button {id}");
        }
        let primary = get_dlg_item(hwnd, ID_PRIMARY_LABEL).expect("primary label");
        assert!(get_text(primary).starts_with("Copilot key: "));
        let status = get_dlg_item(hwnd, ID_HOTKEY_STATUS).expect("status line");
        assert!(
            !get_text(status).contains("tray menu"),
            "the old hint must be gone"
        );

        let mut staged = config.hotkeys.clone();
        apply_learned(
            &mut staged,
            HK_PRIMARY,
            chord(b'W' as u32, true, false, true, false),
        );
        set_hotkey_labels(hwnd, &staged);
        assert_eq!(
            get_text(primary),
            format!("Copilot key: {}", chord_to_string(&staged.primary))
        );
        assert!(get_text(primary).contains("W"));

        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }
}
