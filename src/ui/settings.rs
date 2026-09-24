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
//! - `hotkeys.primary` / `hotkeys.secondary` are shown read-only (via
//!   `chord_to_string`); rebinding stays in the tray's existing learn mode
//!   per the spec for this module.

use std::ffi::c_void;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateFontIndirectW, DeleteObject, FW_NORMAL, HFONT, HGDIOBJ, LOGFONTW,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::Controls::{InitCommonControlsEx, ICC_BAR_CLASSES, INITCOMMONCONTROLSEX};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::GetClientRect;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgItem, GetMessageW,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, IsChild, IsDialogMessageW,
    LoadCursorW, PostMessageW, RegisterClassExW, SetForegroundWindow, SetWindowLongPtrW,
    SetWindowPos, SetWindowTextW, ShowWindow, SystemParametersInfoW, TranslateMessage,
    CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, HWND_TOP, IDC_ARROW, MSG,
    NONCLIENTMETRICSW, SPI_GETNONCLIENTMETRICS, SWP_NOMOVE, SWP_NOZORDER, SW_SHOW,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_CLOSE, WM_COMMAND, WM_DESTROY, WM_HSCROLL, WM_KEYDOWN,
    WM_NCCREATE, WM_NCDESTROY, WM_SETFONT, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_DISABLED,
    WS_EX_CONTROLPARENT, WS_EX_DLGMODALFRAME, WS_GROUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    WS_VSCROLL,
};

use crate::config::{Config, OllamaConfig, RequireTickFor};
use crate::hotkey::chord_to_string;
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

    let inner = Box::new(SettingsInner {
        hwnd: HWND(std::ptr::null_mut()),
        font: HFONT(std::ptr::null_mut()),
        prompt_edit: HWND(std::ptr::null_mut()),
        original: config.clone(),
        result: None,
        should_close: false,
    });
    let raw = Box::into_raw(inner);

    let class_name = wide_z(CLASS_NAME);
    let title = wide_z("Wingman settings");

    // 96dpi guess for the very first CreateWindowExW call, before we have an
    // HWND to ask GetDpiForWindow about -- corrected immediately below, same
    // two-step approach card.rs uses for its own fonts.
    let w0 = to_px(WIN_W_DP, 96);
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
            to_px(WIN_W_DP, dpi),
            to_px(fitted_h, dpi),
            SWP_NOMOVE | SWP_NOZORDER,
        );
    }

    // The layout positions the button row from the bottom, so it needs the
    // CLIENT height, not the window height -- the caption and borders are
    // ~100px at 250% scaling, which is enough to push Save and Cancel below
    // the visible area. Ask the window what it actually got.
    //
    // Issue #340 MEASURED 2026-09-24: this used to clamp the result up to
    // at least `MIN_WIN_H_DP` (`.max(MIN_WIN_H_DP)`). On a small screen at
    // high DPI (reproduced here at 1280x800 @ 240dpi/250%) the real client
    // area can be smaller than `MIN_WIN_H_DP`, and clamping up made
    // `build_ui` believe it had more room than it actually did -- the
    // Prompt group and the Save/Cancel footer both got positioned using a
    // taller height than the real client rect, landing partly or fully
    // below the visible window. `prompt_footer_layout` already keeps the
    // footer below the Prompt group for *any* `win_h_dp`, however small, so
    // this only needs the true measured value, floored at 1 so a later
    // subtraction never goes negative.
    let client_h_dp = {
        let mut rc = windows::Win32::Foundation::RECT::default();
        if unsafe { GetClientRect(hwnd, &mut rc) }.is_ok() {
            ((rc.bottom - rc.top) * 96 / dpi.max(1) as i32).max(1)
        } else {
            fitted_h
        }
    };

    inner_ref.prompt_edit = build_ui(hwnd, instance, dpi, inner_ref.font, config, client_h_dp);

    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }

    run_message_loop(hwnd, inner_ref.prompt_edit);

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

fn run_message_loop(hwnd: HWND, prompt_edit: HWND) {
    loop {
        let mut msg = MSG::default();
        let ret = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if ret.0 <= 0 {
            // WM_QUIT (0) or an error (-1). We never post WM_QUIT ourselves,
            // but bail cleanly either way rather than spin.
            break;
        }

        let belongs_to_us = msg.hwnd == hwnd || unsafe { IsChild(hwnd, msg.hwnd) }.as_bool();

        if belongs_to_us && msg.message == WM_KEYDOWN {
            let vk = msg.wParam.0 as u32;
            if vk == VK_ESCAPE.0 as u32 {
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
            // ES_WANTRETURN means Enter should insert a newline instead.
            if vk == VK_RETURN.0 as u32 && msg.hwnd != prompt_edit {
                unsafe {
                    let _ =
                        PostMessageW(Some(hwnd), WM_COMMAND, WPARAM(ID_SAVE as usize), LPARAM(0));
                }
                continue;
            }
        }

        unsafe {
            if !IsDialogMessageW(hwnd, &msg).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
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
    prompt_edit: HWND,
    /// The config this window was opened with -- fields not touched by the
    /// UI (e.g. model lists, hotkeys) are carried through from here.
    original: Config,
    /// Set on Save; left `None` on Cancel/close.
    result: Option<Config>,
    should_close: bool,
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
        WM_DESTROY => {
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
            inner.result = Some(build_config(&inner.original, &raw));
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
        ID_RESET_PROMPT => {
            set_text(inner.prompt_edit, DEFAULT_PROMPT);
        }
        ID_OPENAI_SHOW_KEY if notify == BN_CLICKED => {
            toggle_password(inner.hwnd, ID_OPENAI_SHOW_KEY, ID_OPENAI_KEY);
        }
        ID_ANTHROPIC_SHOW_KEY if notify == BN_CLICKED => {
            toggle_password(inner.hwnd, ID_ANTHROPIC_SHOW_KEY, ID_ANTHROPIC_KEY);
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
const ID_SHOW_DIFFICULTY: i32 = 112;
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
    ("ID_SHOW_DIFFICULTY", ID_SHOW_DIFFICULTY),
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
const CB_SETCURSEL: u32 = 0x014E;
const CB_GETCURSEL: u32 = 0x0147;
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

use windows::Win32::Graphics::Gdi::InvalidateRect;
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
    if work_px <= 0 {
        return WIN_H_DP;
    }
    // Leave room for the title bar and a little breathing space.
    let avail_dp = (work_px * 96 / dpi.max(1) as i32) - 48;
    WIN_H_DP.min(avail_dp).max(MIN_WIN_H_DP)
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
const WIN_H_DP: i32 = 918;
/// Below this the prompt box stops being usable; scroll rather than shrink
/// further (not yet built -- see `prompt_footer_layout`'s doc comment).
///
/// This is only a floor against a failed/absurd monitor query (see its use
/// in `fitted_height_dp` and the `client_h_dp` fallback in `show_modal`),
/// not a guarantee that content never gets clipped: that guarantee is
/// `prompt_footer_layout`'s job now (issue #340), and it holds for whatever
/// `win_h_dp` the window actually ends up with, however small. MEASURED
/// 2026-09-24: raising this constant to try to force enough room for the
/// Prompt group is a trap -- `show_modal`'s `client_h_dp` clamps the real,
/// measured client height *up* to at least `MIN_WIN_H_DP`
/// (`.max(MIN_WIN_H_DP)`), so a large `MIN_WIN_H_DP` makes `build_ui` believe
/// the window is taller than it actually is on a small screen, which pushes
/// the footer below the real visible area instead of fixing anything.
const MIN_WIN_H_DP: i32 = 420;

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

/// Rects for the Prompt group box and the Save/Cancel footer, computed
/// purely from where the fixed-height groups above it end (`content_top`,
/// in dp) and how tall the window's client area actually is (`win_h_dp`).
///
/// Issue #340: the previous code placed the footer at a fixed offset from
/// `win_h_dp` (`win_h_dp - buttons_h`) while the Prompt group's *top* came
/// from `content_top`, a value that does not shrink with the window. At
/// high DPI on a small monitor `fitted_height_dp` shrinks `win_h_dp` well
/// below `content_top`, so the footer's fixed offset from the (now much
/// smaller) `win_h_dp` landed *above* `content_top`, drawing Save/Cancel on
/// top of the Prompt box. MEASURED 2026-09-24 (issue #340 screenshot, 150%
/// scaling).
///
/// This function makes the invariant structural instead of incidental: the
/// footer is always placed below the Prompt group's actual bottom, never
/// derived from `win_h_dp` alone. When there's enough room the footer still
/// sits flush with the bottom of the window (unchanged from before); when
/// there isn't, the Prompt group takes its minimum height and the footer is
/// pushed down to clear it, which can push the footer below the visible
/// window on a very short client area rather than overlapping it -- clipping
/// instead of overlap. `fitted_height_dp`'s `MIN_WIN_H_DP` is sized so that
/// case does not occur on a 1080p screen at up to 200% scaling (see its own
/// comment).
struct PromptFooterLayout {
    prompt_top: i32,
    prompt_bottom: i32,
    prompt_edit_h: i32,
    reset_btn_y: i32,
    buttons_y: i32,
}

fn prompt_footer_layout(content_top: i32, win_h_dp: i32) -> PromptFooterLayout {
    let buttons_h = BTN_H + MARGIN * 2;
    let reset_btn_h = BTN_H;
    let prompt_top = content_top;
    let prompt_label_bottom = prompt_top + GROUP_LABEL_TOP;

    // The smallest the Prompt group can be and still hold a usable edit box
    // and the reset button: label, minimum edit height, the gap before the
    // reset button, the reset button itself, and a bottom margin before the
    // group's own border.
    let min_prompt_group_h = GROUP_LABEL_TOP + MIN_PROMPT_EDIT_H + ROW_GAP + reset_btn_h + MARGIN;

    let desired_prompt_bottom = win_h_dp - buttons_h;
    let prompt_bottom = desired_prompt_bottom.max(prompt_top + min_prompt_group_h);

    let prompt_edit_h = ((prompt_bottom - MARGIN) - prompt_label_bottom - reset_btn_h - ROW_GAP)
        .max(MIN_PROMPT_EDIT_H);
    let reset_btn_y = prompt_label_bottom + prompt_edit_h + ROW_GAP;

    // Flush with the window bottom when there's room (matches the original
    // layout exactly); otherwise pinned just below the Prompt group's own
    // bottom, which by construction is always >= where the reset button
    // ends, so Save/Cancel can never land on the Prompt box.
    let buttons_y = (prompt_bottom + GROUP_GAP).max(win_h_dp - buttons_h + MARGIN);

    PromptFooterLayout {
        prompt_top,
        prompt_bottom,
        prompt_edit_h,
        reset_btn_y,
        buttons_y,
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

fn get_dlg_item(hwnd: HWND, id: i32) -> Option<HWND> {
    unsafe { GetDlgItem(Some(hwnd), id).ok() }
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
            for (i, u) in "Segoe UI".encode_utf16().enumerate() {
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
    parent: HWND,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
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
                Some(self.parent),
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
            hwnd
        } else {
            HWND(std::ptr::null_mut())
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

/// Combines #15's health check with #14's GPU/CPU indicator into the one
/// status line Settings shows for Ollama. Computed once, synchronously,
/// when Settings opens (AGENTS.md rule 5: discovery happens on demand,
/// never on a timer): the health check is Win32-only (no network at all)
/// and always runs; the two HTTP calls it can make (`/api/ps`, `/api/tags`)
/// only run when [`should_query_ollama_details`] says so (#188), and are
/// bounded by [`OLLAMA_STATUS_HTTP_BUDGET`] in total when they do.
fn ollama_status_line(cfg: &OllamaConfig, ollama_configured: bool) -> String {
    let port = ollama_admin::port_from_base_url(&cfg.base_url).unwrap_or(11434);
    let health = ollama_admin::query_ollama_health(port);
    let health_msg = health.message();

    if !should_query_ollama_details(ollama_configured, &health) {
        return health_msg;
    }

    let budget_start = std::time::Instant::now();
    let gpu_line = match ollama_admin::ps_with_timeout(&cfg.base_url, OLLAMA_STATUS_HTTP_BUDGET) {
        Ok(entries) => match ollama_admin::gpu_status_for(&entries, &cfg.model) {
            GpuStatus::Gpu => format!("{} is loaded on GPU.", cfg.model),
            GpuStatus::Cpu => format!("{} is loaded on CPU.", cfg.model),
            GpuStatus::NotLoaded => format!("{} is not loaded yet.", cfg.model),
        },
        Err(_) => return health_msg,
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
                    " {vision_count} of {} local models support vision.",
                    models.len()
                )
            }
            _ => String::new(),
        }
    };

    format!("{health_msg} {gpu_line}{vision_line}")
}

/// Builds every child control and returns the prompt edit's `HWND` (the
/// caller needs it to exempt Enter-as-newline from the Enter-submits rule).
fn build_ui(
    hwnd: HWND,
    instance: HINSTANCE,
    dpi: u32,
    font: HFONT,
    config: &Config,
    win_h_dp: i32,
) -> HWND {
    let ctx = Ctx {
        parent: hwnd,
        instance,
        dpi,
        font,
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
    let active_is_anthropic =
        config.providers.order.first().map(|s| s.as_str()) == Some("anthropic");
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
    combo_select_index(active_combo, if active_is_anthropic { 1 } else { 0 });
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
    // Read-only by design: editing `base_url`/`model`/enabling Ollama as
    // an active provider here would need a model combo and an "enabled"
    // control the same way OpenAI/Anthropic have above, which is a bigger
    // Win32 UI addition than fits this pass -- filed as a follow-up
    // referencing #51's upcoming WebView2 settings window instead of
    // built here.
    let ollama_top = y;
    y += GROUP_LABEL_TOP;
    let ollama_configured = config.providers.order.iter().any(|p| p == "ollama");
    let ollama_status = ollama_status_line(&config.providers.ollama, ollama_configured);
    ctx.create(
        WC_STATIC,
        &ollama_status,
        0,
        0,
        content_x + MARGIN,
        y,
        content_w - 2 * MARGIN,
        ROW_H * 2,
        ID_OLLAMA_STATUS,
    );
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
    ctx.create(
        WC_STATIC,
        &format!("Copilot key: {}", chord_to_string(&config.hotkeys.primary)),
        0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        0,
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
        r.w,
        ROW_H,
        0,
    );
    r.advance();
    ctx.create(
        WC_STATIC,
        "Rebind from the tray menu (right-click the tray icon).",
        WS_DISABLED.0,
        0,
        r.x,
        r.y,
        r.w,
        ROW_H,
        0,
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
    let show_diff = ctx.create(
        WC_BUTTON,
        "Show difficulty rating",
        BS_AUTOCHECKBOX as u32,
        0,
        r.x + 260 + 50 + 12,
        r.y,
        r.w - (260 + 50 + 12),
        ROW_H,
        ID_SHOW_DIFFICULTY,
    );
    set_checkbox(show_diff, config.ui.show_difficulty);
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
    let footer = prompt_footer_layout(y, win_h_dp);
    let prompt_top = footer.prompt_top;
    let prompt_bottom = footer.prompt_bottom;
    y = prompt_top + GROUP_LABEL_TOP;

    let reset_btn_h = BTN_H;
    let prompt_edit_h = footer.prompt_edit_h;
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

    ctx.create(
        WC_BUTTON,
        "Reset prompt to default",
        BS_PUSHBUTTON as u32,
        0,
        content_x + MARGIN,
        footer.reset_btn_y,
        200,
        reset_btn_h,
        ID_RESET_PROMPT,
    );

    ctx.create(
        WC_BUTTON,
        "Prompt",
        BS_GROUPBOX as u32,
        0,
        content_x,
        prompt_top,
        content_w,
        prompt_bottom - prompt_top,
        0,
    );

    // -- Save / Cancel ------------------------------------------------
    let buttons_y = footer.buttons_y;
    // #344: inline "that field is wrong" message, blank until a Save
    // attempt fails validation. Sits to the left of the Save/Cancel row so
    // it never overlaps either the Prompt group or the buttons.
    ctx.create(
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
    ctx.create(
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
    ctx.create(
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

    // Tab order / grouping: give the first control of each visual group
    // WS_GROUP so arrow-key navigation and Tab-between-groups behave.
    for id in [
        ID_ACTIVE_PROVIDER,
        ID_MAX_EDGE,
        ID_CARD_SECONDS,
        ID_PROMPT_EDIT,
        ID_SAVE,
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
        ID_MAX_EDGE,
        ID_MONITOR,
        ID_CARD_SECONDS,
        ID_SHOW_DIFFICULTY,
        ID_TEXT_SCALE_TRACK,
        ID_REQUIRE_TICK_FOR,
        ID_PROMPT_EDIT,
        ID_RESET_PROMPT,
        ID_SAVE,
        ID_CANCEL,
    ] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            add_style(h, WS_TABSTOP.0);
        }
    }

    prompt_edit
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
    openai_key: String,
    openai_model: String,
    openai_effort: String,
    anthropic_key: String,
    anthropic_model: String,
    anthropic_effort: String,
    max_edge_text: String,
    monitor: String,
    card_seconds_text: String,
    show_difficulty: bool,
    text_scale_raw: f32,
    prompt: String,
    require_tick_for: String,
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
        show_difficulty: checkbox_checked(hwnd, ID_SHOW_DIFFICULTY),
        text_scale_raw: get_dlg_item(hwnd, ID_TEXT_SCALE_TRACK)
            .map(|h| unsafe { SendMessageW(h, TBM_GETPOS, None, None).0 as f32 / 100.0 })
            .unwrap_or(inner.original.ui.text_scale),
        prompt: get_dlg_item(hwnd, ID_PROMPT_EDIT)
            .map(get_text)
            .unwrap_or_default(),
        require_tick_for: combo_selected_text(hwnd, ID_REQUIRE_TICK_FOR),
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

#[allow(dead_code)] // exercised by tests; #194 replaced its build_config call site with merge_provider_order
fn order_from_choice(choice: usize) -> Vec<String> {
    if choice == 1 {
        vec!["anthropic".to_string(), "openai".to_string()]
    } else {
        vec!["openai".to_string(), "anthropic".to_string()]
    }
}

#[allow(dead_code)] // exercised by tests; kept for symmetry with order_from_choice
fn choice_from_order(order: &[String]) -> usize {
    match order.first().map(|s| s.as_str()) {
        Some("anthropic") => 1,
        _ => 0,
    }
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
fn merge_provider_order(configured: &[String], choice: usize) -> Vec<String> {
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

    cfg.capture.max_edge = parse_max_edge_or(&raw.max_edge_text, original.capture.max_edge);
    if let Some(v) = label_to_monitor(raw.monitor.trim()) {
        cfg.capture.monitor = v;
    }

    cfg.ui.card_seconds = parse_u32_or(&raw.card_seconds_text, original.ui.card_seconds);
    cfg.ui.show_difficulty = raw.show_difficulty;
    cfg.ui.text_scale = clamp_text_scale(raw.text_scale_raw);
    cfg.ui.prompt = if raw.prompt.trim().is_empty() {
        original.ui.prompt.clone()
    } else {
        raw.prompt.clone()
    };

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
        assert_eq!(ollama_status_line(&cfg, true), "Ollama is not running.");
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

        let status = ollama_status_line(&cfg, false);
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
    fn merge_order_ollama_first_is_left_in_place() {
        let configured = names(&["ollama", "openai", "anthropic"]);
        assert_eq!(
            merge_provider_order(&configured, 0),
            names(&["ollama", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["ollama", "anthropic", "openai"])
        );
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
            names(&["ollama", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["ollama", "anthropic", "openai"])
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
            names(&["ollama", "openai", "anthropic"])
        );
        assert_eq!(
            merge_provider_order(&configured, 1),
            names(&["ollama", "anthropic", "openai"])
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
            provider_choice: 0,
            openai_key: original.providers.openai.api_key.clone(),
            openai_model: original.providers.openai.model.clone(),
            openai_effort: effort_display(&original.providers.openai.effort),
            anthropic_key: original.providers.anthropic.api_key.clone(),
            anthropic_model: original.providers.anthropic.model.clone(),
            anthropic_effort: effort_display(&original.providers.anthropic.effort),
            max_edge_text: original.capture.max_edge.to_string(),
            monitor: monitor_display(&original.capture.monitor),
            card_seconds_text: original.ui.card_seconds.to_string(),
            show_difficulty: original.ui.show_difficulty,
            text_scale_raw: original.ui.text_scale,
            prompt: original.ui.prompt.clone(),
            require_tick_for: require_tick_for_label(original.forms.require_tick_for).to_string(),
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
        raw.show_difficulty = false;
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
        assert!(!cfg.ui.show_difficulty);
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

    fn footer_does_not_overlap(win_h_dp: i32) -> bool {
        let footer = prompt_footer_layout(CONTENT_TOP_DP, win_h_dp);
        let reset_btn_bottom = footer.reset_btn_y + BTN_H;
        reset_btn_bottom <= footer.buttons_y && footer.prompt_bottom <= footer.buttons_y
    }

    #[test]
    fn footer_does_not_overlap_prompt_at_96_dpi_full_height() {
        // 96dpi (100%), `fitted_height_dp` leaves the window at its full
        // design height.
        assert!(footer_does_not_overlap(WIN_H_DP));
    }

    #[test]
    fn footer_does_not_overlap_prompt_at_144_dpi_shrunk_height() {
        // 144dpi (150%) on a 1080p screen: `fitted_height_dp` shrinks the
        // window to roughly 645dp of usable client height (MEASURED via
        // the same formula `fitted_height_dp` uses, for a ~1040px-tall work
        // area at 144dpi: (1040*96/144) - 48 ~= 645), well below
        // `CONTENT_TOP_DP`. This is the exact shape of issue #340's
        // screenshot.
        assert!(footer_does_not_overlap(645));
    }

    #[test]
    fn footer_does_not_overlap_prompt_at_the_configured_minimum_height() {
        assert!(footer_does_not_overlap(MIN_WIN_H_DP));
    }

    #[test]
    fn footer_sits_flush_with_the_window_bottom_when_there_is_room() {
        // Unchanged from the pre-#340 behaviour when the window is tall
        // enough: Save/Cancel hug the bottom edge rather than floating
        // higher than necessary. Even `WIN_H_DP` (918, raised by #403's
        // Forms group and #341's Capture hint line) is not tall enough for
        // `CONTENT_TOP_DP` (738) plus the Prompt group's minimum (issue
        // #340's underlying finding: the original design height never
        // actually had room for its own content -- see `MIN_WIN_H_DP`'s
        // comment), so this uses a window tall enough to exercise the
        // "plenty of room" branch specifically. Needs
        // `win_h_dp - buttons_h >= CONTENT_TOP_DP + min_prompt_group_h`
        // (738 + 138 = 876, so > 936) to actually land in the flush branch
        // rather than the minimum-height one; 950 gives headroom.
        let roomy_win_h_dp = 950;
        let footer = prompt_footer_layout(CONTENT_TOP_DP, roomy_win_h_dp);
        let buttons_h = BTN_H + MARGIN * 2;
        assert_eq!(footer.buttons_y, roomy_win_h_dp - buttons_h + MARGIN);
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

        assert!(ensure_class_registered(instance));
        ensure_common_controls();

        let config = Config::default();
        let inner = Box::new(SettingsInner {
            hwnd: HWND(std::ptr::null_mut()),
            font: HFONT(std::ptr::null_mut()),
            prompt_edit: HWND(std::ptr::null_mut()),
            original: config.clone(),
            result: None,
            should_close: false,
        });
        let raw = Box::into_raw(inner);

        let class_name = wide_z(CLASS_NAME);
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
        assert!(ensure_class_registered(instance));
        ensure_common_controls();

        let mut config = Config::default();
        config.providers.openai.api_key = "sk-live-secret-should-not-appear-1234".to_string();

        let title = wide_z("Wingman settings (mask test)");
        let class_name = wide_z(CLASS_NAME);
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

        unsafe {
            let _ = DestroyWindow(hwnd);
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
        assert!(ensure_class_registered(instance));
        ensure_common_controls();

        let mut config = Config::default();
        config.providers.openai.api_key = crate::config::UNREADABLE_KEY_MARKER.to_string();

        let title = wide_z("Wingman settings (unreadable-marker test)");
        let class_name = wide_z(CLASS_NAME);
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

        unsafe {
            let _ = DestroyWindow(hwnd);
        }
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
}
