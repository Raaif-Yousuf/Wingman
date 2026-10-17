//! The settings window: a plain Win32 dialog-style window (real controls,
//! not a text editor) for everything in [`Config`] a user would reasonably
//! change. Replaces the old "Edit settings" tray action that opened
//! `config.toml` in a text editor.
//!
//! # Public API
//!
//! ```ignore
//! pub fn open(instance: HINSTANCE, owner: HWND, config: &Config) -> bool;
//! pub fn pre_dispatch(msg: &MSG) -> bool;
//! ```
//!
//! The window is **modeless** (#871). `open` creates it and returns at once;
//! there is no loop of its own and no `&mut App` held while it is open, so Quit,
//! the tray, the pause key and every worker result are handled as they arrive.
//! `owner` is the main window: the window posts to it when it loses the
//! foreground or closes (`WM_APP_SETTINGS_LEFT`), and the writer's answer
//! returns to it (`WM_APP_CONFIG_WRITTEN`).
//!
//! Save no longer returns a `Config`. It sends only the fields the user changed
//! (`config_writer::diff` of the config the window opened with against the
//! form) to the `settings-writer` thread, which patches them onto the file as it
//! is on disk now, so a hand edit made meanwhile survives, and then closes the
//! window. This module never touches the disk or the network on the UI thread.
//!
//! The window has a left navigation rail and five pages (#872): General,
//! Models, Privacy, Actions and Advanced. Every control is created once, tagged
//! with its page, and the rail shows one page's controls and hides the rest, so
//! a hidden page is out of the Tab order and the accessibility tree. Ctrl+1 to
//! Ctrl+5, Ctrl+PageUp and Ctrl+PageDown change the page from anywhere, F6
//! moves between the rail and the page. Save and Cancel are still pinned under
//! the pages and still send every touched field at once.
//!
//! The General page's Mode row and Pause and Resume buttons act at once, not on
//! Save (#873): they post `WM_APP_SETTINGS_COMMAND` to the owner, which runs the
//! tray's own `set_mode`, `pause_for` and `resume`. The app calls [`sync_live`]
//! after any change, so the controls follow the tray and the pause key too. The
//! pause shortcut is staged and saved like the other two.
//!
//! Notes for the integrating app:
//! - If a settings window is already open, `open` brings it to the front and
//!   returns `true` without opening a second one. The guard is a process-wide
//!   static, not something the caller manages.
//! - The window has no message loop, so the app's own pump must offer every
//!   message to [`pre_dispatch`] first: it takes Enter, Esc and the wheel,
//!   gives the dialog manager Tab and the mnemonics (`IsDialogMessage`), scrolls
//!   a newly focused control into view, and takes a captured shortcut
//!   (`WM_APP_LEARNED`, posted to the main window) while a Change is pending.
//!   It is one atomic load when no window is open.
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
//!   window. [`pre_dispatch`] takes that message off the queue itself
//!   (`take_learned`) while a Change is pending, so the app never sees it as a
//!   rebind. The new chord is only staged in the window and saved with
//!   everything else on Save.

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
use windows::Win32::UI::Input::KeyboardAndMouse::{EnableWindow, GetFocus, VK_ESCAPE, VK_RETURN};
use windows::Win32::UI::WindowsAndMessaging::WindowFromPoint;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetDlgCtrlID, GetDlgItem,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    IsChild, IsDialogMessageW, KillTimer, LoadCursorW, PeekMessageW, PostMessageW,
    RegisterClassExW, SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos,
    SetWindowTextW, ShowWindow, SystemParametersInfoW, TranslateMessage, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, GWLP_USERDATA, HWND_TOP, IDC_ARROW, MSG, NONCLIENTMETRICSW,
    PM_REMOVE, SPI_GETNONCLIENTMETRICS, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SW_HIDE, SW_SHOW, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WM_ACTIVATE, WM_CLOSE, WM_COMMAND,
    WM_DESTROY, WM_DPICHANGED, WM_HSCROLL, WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY, WM_PAINT,
    WM_SETFONT, WM_SETREDRAW, WM_TIMER, WNDCLASSEXW, WS_CAPTION, WS_CHILD, WS_EX_CONTROLPARENT,
    WS_EX_DLGMODALFRAME, WS_GROUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetClientRect, GetParent, GetScrollInfo, GetWindow, IsWindowVisible, CBN_SELCHANGE, GW_CHILD,
    GW_HWNDNEXT, SB_VERT, SCROLLINFO, SIF_DISABLENOSCROLL, SIF_PAGE, SIF_POS, SIF_RANGE,
    SIF_TRACKPOS, SM_CXVSCROLL, WM_CTLCOLORLISTBOX, WM_DRAWITEM, WM_MEASUREITEM, WM_MOUSEWHEEL,
    WM_VSCROLL,
};

use crate::config::{ActiveProvider, CardCorner, Config, Hotkeys, OllamaConfig, RequireTickFor};
use crate::hotkey::{
    apply_learned, arm_learning, chord_to_string, disarm_learning, judge_learned, Chord,
    LearnedVerdict, HK_PAUSE, HK_PRIMARY, HK_SECONDARY, WM_APP_LEARNED,
};
use crate::provider::ollama_admin::{self, GpuStatus, ListenerKind, OllamaHealth};
use crate::provider::ollama_gpu_fix;
use crate::provider::DEFAULT_PROMPT;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

/// Opens the settings window and returns at once (#871): the window lives on
/// the caller's message loop, which must offer every message to
/// [`pre_dispatch`]. `owner` is the main window (see the module docs). Returns
/// `false` only when Windows would not create the window; an already open one
/// is brought to the front and counts as success.
pub fn open(instance: HINSTANCE, owner: HWND, config: &Config) -> bool {
    let existing = OPEN_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        let hwnd = HWND(existing as *mut c_void);
        unsafe {
            let _ = SetForegroundWindow(hwnd);
        }
        return true;
    }

    if !ensure_class_registered(instance) {
        return false;
    }
    open_impl(instance, owner, config, CLASS_NAME, true)
}

/// [`open`] without the "already open" check and the class registration. `live`
/// is false in the tests: the window is not shown or activated and the
/// background probes (Ollama, the cloud model lists) are not started. The
/// tests give their own class name (rule 9).
fn open_impl(instance: HINSTANCE, owner: HWND, config: &Config, class: &str, live: bool) -> bool {
    ensure_common_controls();

    let mut inner = Box::new(SettingsInner::new(config.clone()));
    // The window owns this box from here: `WM_NCDESTROY` frees it.
    inner.owner = owner;
    inner.owns_itself = true;
    let owner_raw = owner.0 as isize;
    // What the FILE held when the window opened, defaults for keys it does not
    // spell out. The running config is not that (a tray change not yet written,
    // a default never written), and comparing with it refuses a save that
    // touched nothing by hand. Tests do not read the real file (rule 9).
    let file_baseline = if live {
        crate::config::Config::path()
            .ok()
            .and_then(|p| crate::config_writer::file_baseline(&p))
    } else {
        None
    };
    inner.save_sink = Some(Box::new(move |original: &Config, edited: &Config| {
        let changes = crate::config_writer::diff(original, edited).map_err(|e| format!("{e:#}"))?;
        // A list changed here replaces the whole list in the file, so it is
        // written only if the file's list is still the one this window opened
        // with; a hand edit of a sibling entry is not silently dropped.
        let changes = crate::config_writer::guard_whole_values(
            changes,
            file_baseline.as_ref().unwrap_or(original),
        )
        .map_err(|e| format!("{e:#}"))?;
        crate::app::post_settings_save(
            owner_raw,
            crate::app::SettingsSave {
                changes,
                original: original.clone(),
                edited: edited.clone(),
            },
        )
    }));
    let raw = Box::into_raw(inner);

    let class_name = wide_z(class);
    let title = wide_z("Wingman settings");
    // See `FREED_BY_WINDOW`: forget a stale answer before creating.
    FREED_BY_WINDOW.with(|c| c.set(0));

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
            // Windows destroys a window whose creation failed after
            // `WM_NCCREATE`, and `WM_NCDESTROY` has already freed the box then.
            // Freeing it again here would be a double free.
            if FREED_BY_WINDOW.with(|c| c.replace(0)) != raw as usize {
                unsafe {
                    drop(Box::from_raw(raw));
                }
            }
            return false;
        }
    };

    OPEN_HWND.store(hwnd.0 as isize, Ordering::SeqCst);

    let inner_ref = unsafe { &mut *raw };
    inner_ref.hwnd = hwnd;

    let dpi = dpi_for(hwnd);
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
            window_width_px(dpi) + nc_width_px(hwnd),
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
    sync_card_display_enabled(hwnd);
    if live {
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
            inner_ref.cloud_sync.clone(),
            inner_ref.alive.clone(),
        );
    }
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

    if live {
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            // #346: without an explicit initial focus, keyboard focus landed
            // wherever it happened to fall (reported as the Max edge box) --
            // pin it to the first field instead, so Tab order starts from the
            // top of the window every time it opens.
            // #872: the rail, the first stop; the page is a Tab away.
            if let Some(first) = get_dlg_item(hwnd, ID_RAIL) {
                let _ = SetFocus(Some(first));
            }
        }
    }

    true
}

/// Asks the open Settings window to close without saving (#673), as its own
/// close box does. A no-op when none is open. Posted, not sent, so it is safe
/// to call from inside the owner window's procedure.
pub fn request_close() {
    let existing = OPEN_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        unsafe {
            let _ = PostMessageW(
                Some(HWND(existing as *mut c_void)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
}

/// Closes the open Settings window at once, without saving. For the app's own
/// shutdown: Settings is a top-level window of its own, so it does not go away
/// with the main window. Must run on the UI thread. A no-op when none is open.
pub fn close_now() {
    let existing = OPEN_HWND.load(Ordering::SeqCst);
    if existing != 0 {
        unsafe {
            let _ = DestroyWindow(HWND(existing as *mut c_void));
        }
    }
}

/// Whether the Settings window exists (#1012), in front or not.
pub fn is_open() -> bool {
    OPEN_HWND.load(Ordering::SeqCst) != 0
}

/// Whether the Settings window is open and is the foreground window. The app
/// holds back anything that would put a card on top of it while this is true.
pub fn is_foreground() -> bool {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    open != 0 && unsafe { GetForegroundWindow() }.0 as isize == open
}

thread_local! {
    /// The address of the `SettingsInner` the window procedure freed in
    /// `WM_NCDESTROY` (0: none). `open_impl` reads it when `CreateWindowExW`
    /// fails, to know whether the window already took the box with it.
    static FREED_BY_WINDOW: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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
        | ID_CHANGE_SECONDARY | ID_CHANGE_PAUSE | ID_PAUSE_BUTTON | ID_RESUME_BUTTON
        | ID_PULL_BUTTON | ID_OPEN_CONFIG => ctrl_id,
        // #635: Enter in the Pull model box starts the pull.
        ID_PULL_NAME => ID_PULL_BUTTON,
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

/// Enter and Esc for the Settings window. Returns true when the key was used
/// and must not be dispatched further.
unsafe fn handle_key(hwnd: HWND, msg: &MSG, prompt_edit: HWND) -> bool {
    let vk = msg.wParam.0 as u32;
    // #776: with a combo list open, Enter and Esc belong to the list (pick or
    // close); they must not save or cancel the window.
    if (vk == VK_ESCAPE.0 as u32 || vk == VK_RETURN.0 as u32) && wheel_over_open_combo(msg.hwnd) {
        let _ = TranslateMessage(msg);
        DispatchMessageW(msg);
        return true;
    }
    if vk == VK_ESCAPE.0 as u32 {
        // #345: while a Change is pending, Esc cancels that, not the whole
        // window. (The hook normally swallows this Esc and reports it as a
        // captured key instead; see `judge_learned`.)
        if !cancel_learning_if_active(hwnd) {
            let _ = PostMessageW(
                Some(hwnd),
                WM_COMMAND,
                WPARAM(ID_CANCEL as usize),
                LPARAM(0),
            );
        }
        return true;
    }
    // #872: Ctrl+1 to Ctrl+5 and Ctrl+PageUp and PageDown change the page from
    // anywhere in the window; F6 swaps focus between the rail and the page;
    // Enter on the rail goes into the page instead of saving.
    {
        use windows::Win32::UI::Input::KeyboardAndMouse::{
            GetKeyState, VK_CONTROL, VK_F6, VK_MENU,
        };
        let ctrl = GetKeyState(VK_CONTROL.0 as i32) < 0;
        let alt = GetKeyState(VK_MENU.0 as i32) < 0;
        let request = match page_key(vk, ctrl, alt) {
            Some(PageKey::Pick(page)) => Some((PAGE_PICK, page.index() as isize)),
            Some(PageKey::Step(by)) => Some((PAGE_STEP, by)),
            None if vk == VK_F6.0 as u32 => Some((PAGE_SWAP_FOCUS, 0)),
            None if vk == VK_RETURN.0 as u32 && GetDlgCtrlID(msg.hwnd) == ID_RAIL => {
                Some((PAGE_ENTER, 0))
            }
            None => None,
        };
        if let Some((what, arg)) = request {
            let _ = PostMessageW(Some(hwnd), WM_SETTINGS_PAGE, WPARAM(what), LPARAM(arg));
            return true;
        }
    }
    // Enter submits, except inside the multiline prompt box, where
    // ES_WANTRETURN means Enter should insert a newline instead, and on a
    // focused push button, where it presses that button (#342).
    if vk == VK_RETURN.0 as u32 && msg.hwnd != prompt_edit {
        let ctrl_id = GetDlgCtrlID(msg.hwnd);
        let _ = PostMessageW(
            Some(hwnd),
            WM_COMMAND,
            WPARAM(enter_command(ctrl_id) as usize),
            LPARAM(0),
        );
        return true;
    }
    false
}

/// The window that had the keyboard focus after the last message this module
/// looked at, to scroll a newly focused control into view without a timer
/// (#481).
static LAST_FOCUS: AtomicIsize = AtomicIsize::new(0);

/// The settings window's share of the app's message pump (#871). Called with
/// every message before it is translated and dispatched; returns true when the
/// message was used here and must not go any further. Messages that are not
/// the open window's, or its children's, pass straight through.
pub fn pre_dispatch(msg: &MSG) -> bool {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    if open == 0 {
        return false;
    }
    let hwnd = HWND(open as *mut c_void);
    unsafe {
        // #345: the hook posts a captured key to the MAIN window, so it is
        // not addressed to us. While a Change is pending it is ours.
        if msg.message == WM_APP_LEARNED && take_learned(hwnd, msg) {
            return true;
        }
        let ours = msg.hwnd == hwnd || IsChild(hwnd, msg.hwnd).as_bool();
        if !ours {
            return false;
        }
        let prompt_edit = prompt_edit_of(hwnd);
        if msg.message == WM_KEYDOWN && handle_key(hwnd, msg, prompt_edit) {
            return true;
        }
        // #481: the wheel scrolls the body wherever the pointer is in the
        // window (a control under it would otherwise eat the notch).
        if scroll_with_wheel(hwnd, msg, prompt_edit) {
            return true;
        }
        let used = IsDialogMessageW(hwnd, msg).as_bool();
        let focus = GetFocus();
        if LAST_FOCUS.swap(focus.0 as isize, Ordering::SeqCst) != focus.0 as isize
            // Only keyboard moves scroll: scrolling between a click's
            // mouse-down and mouse-up would cancel the click.
            && !is_mouse_message(msg.message)
        {
            follow_focus(hwnd, focus);
        }
        used
    }
}

/// The multi-line prompt box of the window `hwnd`, or null.
unsafe fn prompt_edit_of(hwnd: HWND) -> HWND {
    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner;
    if ptr.is_null() {
        HWND(std::ptr::null_mut())
    } else {
        (*ptr).prompt_edit
    }
}

/// Tells the owner window that Settings lost the foreground or closed, so what
/// waited for it can be shown. A window with no owner (the tests') says nothing.
fn tell_owner_left(owner: HWND) {
    if owner.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            Some(owner),
            crate::app::WM_APP_SETTINGS_LEFT,
            WPARAM(0),
            LPARAM(0),
        );
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
    /// #872: the navigation rail and the page it shows.
    rail: HWND,
    page: Page,
    /// Each page's content height in dp, padding included, indexed by
    /// [`Page::index`]. The Advanced entry is for the prompt closed.
    page_content_dp: [i32; 5],
    /// #481: the scrolling body that holds every group above the footer.
    body: HWND,
    /// The dpi the body's scroll position is expressed in, so a dpi change
    /// can rescale it.
    body_dpi: u32,
    /// #343: where the Ollama group's link button goes, if it has one. Set
    /// when the status probe reports (#527).
    ollama_link_url: Option<&'static str>,
    /// Whether the Ollama group's button has something to offer (#343). It is
    /// shown only then, and only while the Models page is.
    ollama_link_shown: bool,
    /// #382: the button is "Fix it" (the Intel GPU fix) rather than a link.
    ollama_fix: bool,
    /// #527: the Ollama status probe's result, filled by its worker.
    ollama_status: OllamaStatusSlot,
    /// #183: the pulled-model list the background fetch reports into.
    ollama_tags: OllamaTags,
    /// #380: the cloud providers' live model lists, filled the same way.
    cloud_models: CloudModels,
    /// #585: per-vendor fetch generation and the key each list was last
    /// fetched with successfully.
    cloud_sync: std::sync::Arc<CloudSync>,
    /// #566: the running pull's progress, filled by its worker. Replaced by a
    /// fresh slot for every pull, so a stopped pull's worker never writes
    /// into the next one.
    pull_slot: PullSlot,
    /// #566: set while a pull is running; setting it true stops the worker.
    /// `None` when idle. The only thing alive for a pull is its one thread.
    pull_cancel: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    /// #566: which pull is current. Each pull's posts carry its number in
    /// `wparam`; a post from an older pull is ignored.
    pull_gen: usize,
    /// #635: the model a finished pull should select once the refreshed list
    /// arrives.
    pull_select: Option<String>,
    /// True while this window is open. Cleared in `WM_DESTROY`; the model
    /// fetch workers check it before posting (#569).
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// #342: whether the Advanced group shows the Check my work prompt.
    /// Starts collapsed on every open.
    prompt_expanded: bool,
    /// The config this window was opened with -- fields not touched by the
    /// UI (e.g. model lists, hotkeys) are carried through from here.
    original: Config,
    /// The main window, told when this one loses the foreground or closes. Null
    /// in the tests' windows.
    owner: HWND,
    /// True for the window `open` creates: it frees its own state in
    /// `WM_NCDESTROY` and owns the `OPEN_HWND` guard. The tests' windows are
    /// reclaimed by the test and leave both alone.
    owns_itself: bool,
    /// Where Save sends the edit: in production the writer thread (see `open`).
    /// `None` in the tests' windows, which only record `result`.
    save_sink: Option<SaveSink>,
    /// #345: the hotkeys as edited in this window. Starts as the original's
    /// and only reaches the returned config on Save.
    hotkeys: Hotkeys,
    /// #345: the slot a pending Change is waiting on a key for.
    learning: Option<usize>,
    /// #345: the `(slot, chord)` last warned about, so pressing the same
    /// conflicting chord again confirms it (same flow as the tray's).
    pending_conflict: Option<(usize, Chord)>,
    /// The config the last Save produced; `None` until then and on
    /// Cancel/close.
    result: Option<Config>,
}

/// Takes the config the window opened with and the one Save built, and hands
/// the difference to whatever persists it. An `Err` is a plain sentence and
/// keeps the window open.
type SaveSink = Box<dyn Fn(&Config, &Config) -> Result<(), String>>;

/// Counts the `SettingsInner` values dropped, so a test can tell a double free
/// from a single one.
#[cfg(test)]
static INNERS_DROPPED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

#[cfg(test)]
impl Drop for SettingsInner {
    fn drop(&mut self) {
        INNERS_DROPPED.fetch_add(1, Ordering::SeqCst);
    }
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
            rail: HWND(std::ptr::null_mut()),
            page: Page::General,
            page_content_dp: [0; 5],
            body: HWND(std::ptr::null_mut()),
            body_dpi: 96,
            ollama_link_url: None,
            ollama_link_shown: false,
            ollama_fix: false,
            ollama_status: Default::default(),
            ollama_tags: Default::default(),
            cloud_models: Default::default(),
            cloud_sync: Default::default(),
            pull_slot: Default::default(),
            pull_cancel: None,
            pull_gen: 0,
            pull_select: None,
            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            prompt_expanded: false,
            hotkeys: original.hotkeys.clone(),
            owner: HWND(std::ptr::null_mut()),
            owns_itself: false,
            save_sink: None,
            learning: None,
            pending_conflict: None,
            original,
            result: None,
        }
    }

    /// Takes over what `build_ui_recorded` built. Call after `dpi` is set:
    /// the controls were laid out for it.
    fn adopt(&mut self, built: BuiltUi) {
        self.prompt_edit = built.prompt_edit;
        self.rail = built.rail;
        self.page_content_dp = built.page_content_dp;
        self.body = built.body;
        self.body_dpi = self.dpi;
        self.children = built.children;
        self.content_top_dp = built.content_top_dp;
        self.win_h_dp = built.win_h_dp;
        // Only the first page shows.
        unsafe { sync_visibility(self) };
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
    if msg == WM_PAUSE_MENU {
        run_pause_menu(hwnd);
        return LRESULT(0);
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
        // #872: the rail draws itself through us.
        WM_DRAWITEM => {
            draw_rail_item(lparam);
            LRESULT(1)
        }
        WM_MEASUREITEM => {
            measure_rail_item(lparam, inner.dpi);
            LRESULT(1)
        }
        // The rail's background is the window's, not the list box's white.
        WM_CTLCOLORLISTBOX => LRESULT(
            windows::Win32::Graphics::Gdi::GetSysColorBrush(
                windows::Win32::Graphics::Gdi::COLOR_3DFACE,
            )
            .0 as isize,
        ),
        WM_SETTINGS_PAGE => {
            handle_page_request(inner, wparam.0, lparam.0);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_footer_divider(hwnd);
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
            // wparam 1: the fetch after a pull failed, so there is no list to
            // select the pulled model in (#635).
            if wparam.0 == 1 {
                inner.pull_select = None;
                return LRESULT(0);
            }
            let models = inner.ollama_tags.lock().ok().and_then(|g| g.clone());
            if let Some(models) = models {
                let prefer = inner
                    .pull_select
                    .take()
                    .map(|name| resolve_pulled_name(&models, &name));
                apply_ollama_models(hwnd, &models, prefer.as_deref());
            }
            LRESULT(0)
        }
        WM_OLLAMA_PULL => {
            if wparam.0 == inner.pull_gen {
                apply_pull_update(inner);
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
                sync_gemini_thinking_enabled(hwnd);
            }
            LRESULT(0)
        }
        // The gallery's simulated DPI stays whatever monitor this is.
        #[cfg(any(test, debug_assertions))]
        WM_DPICHANGED if forced_dpi().is_some() => LRESULT(0),
        WM_DPICHANGED => {
            on_dpi_changed(inner, wparam, lparam);
            LRESULT(0)
        }
        WM_ACTIVATE if (wparam.0 & 0xFFFF) == 0 && inner.learning.is_some() => {
            // Alt-tabbing away mid-Change: the hook is global, so a key typed
            // in another app would otherwise be captured as the new shortcut.
            stop_learning(inner);
            set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
            tell_owner_left(inner.owner);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // #871: losing the foreground releases whatever waited for it (a
        // finished request's card, a press of the Wingman key).
        WM_ACTIVATE if (wparam.0 & 0xFFFF) == 0 => {
            tell_owner_left(inner.owner);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER if wparam.0 == LEARN_TIMER_ID => {
            learn_timed_out(inner);
            LRESULT(0)
        }
        // #934: the text box colours (and the accent in them) are read once
        // per change, not per paint; repaint the boxes with the new ones.
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_SYSCOLORCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_THEMECHANGED => {
            invalidate_edit_style();
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                windows::Win32::Graphics::Gdi::RDW_FRAME
                    | RDW_INVALIDATE
                    | windows::Win32::Graphics::Gdi::RDW_ALLCHILDREN,
            );
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            invalidate_edit_style();
            inner.alive.store(false, Ordering::SeqCst);
            if inner.owns_itself {
                OPEN_HWND.store(0, Ordering::SeqCst);
            }
            // #566: closing the window mid-pull stops the download.
            if let Some(cancel) = inner.pull_cancel.take() {
                cancel.store(true, Ordering::SeqCst);
            }
            if inner.learning.take().is_some() {
                let _ = KillTimer(Some(hwnd), LEARN_TIMER_ID);
                disarm_learning();
            }
            if !inner.font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(inner.font.0));
                inner.font = HFONT(std::ptr::null_mut());
            }
            if inner.owns_itself {
                // #345: a key captured while Save was being pressed may still be
                // queued for the main window, which would take it as a rebind.
                // Free it unused.
                drain_learned_messages();
                tell_owner_left(inner.owner);
            }
            LRESULT(0)
        }
        WM_NCDESTROY => {
            // Detach GWLP_USERDATA so no later message can dereference a
            // pointer whose Box is about to be freed.
            let owned = inner.owns_itself;
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            let result = DefWindowProcW(hwnd, msg, wparam, lparam);
            if owned {
                // `inner` is not used again: this is the window's last message.
                FREED_BY_WINDOW.with(|c| c.set(ptr as usize));
                drop(Box::from_raw(ptr));
            }
            result
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

fn handle_command(inner: &mut SettingsInner, id: i32, notify: u32) {
    // #585: a key typed but not saved yet still fetches its model list once
    // the user leaves the field.
    if let Some(index) = typed_key_vendor(id, notify) {
        fetch_for_typed_key(inner, index);
        return;
    }
    match id {
        ID_SAVE => {
            // #566: Save would close the window and silently stop the pull.
            if inner.pull_cancel.is_some() {
                if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                    set_text(msg, "A pull is running. Stop it first.");
                }
                return;
            }
            let raw = read_raw_form(inner);
            // #344: an invalid numeric field must not be silently kept at
            // its old value -- reject the Save, keep the window open, focus
            // the bad field and say what is wrong inline (rule 7: never a
            // dialog box). `config.toml` is never touched in this branch.
            if let Some(invalid) = find_invalid_numeric_field(&raw) {
                if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                    set_text(msg, invalid.message());
                }
                focus_and_select_field(inner, invalid.control_id());
                return;
            }
            if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                // Back to the standing hint (#778), not blank.
                set_text(msg, &env_key_hint(&env_hint_vars()));
            }

            // Autostart is registry state rather than a Config field, so it is
            // applied here instead of riding along in the returned Config.
            // Only written when it actually changed, to avoid rewriting the
            // Run key on every Save.
            let want = checkbox_checked(inner.hwnd, ID_AUTOSTART);
            if want != crate::autostart::is_enabled() {
                // Rule 7: a failed registry write must not end in "Settings
                // saved". Say so here, show the real state, keep the window.
                if crate::autostart::set_enabled(want).is_err() {
                    if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                        set_text(msg, autostart_failure_message());
                    }
                    if let Some(box_) = get_dlg_item(inner.hwnd, ID_AUTOSTART) {
                        set_checkbox(box_, crate::autostart::is_enabled());
                    }
                    return;
                }
            }
            let edited = finalize_config(inner, &raw);
            // Only the fields that differ from what this window opened with go
            // on, patched onto the file as it is now (#871), so a hand edit
            // made while the window was open (#851's concern) is never undone.
            if let Some(sink) = &inner.save_sink {
                if let Err(reason) = sink(&inner.original, &edited) {
                    // Rule 7: say so inline and keep the window, never a
                    // dialog or a silent no-op.
                    if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
                        set_text(msg, &format!("Could not save: {reason}"));
                    }
                    return;
                }
            }
            inner.result = Some(edited);
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
        // #872: the rail picked a page (a click, an arrow key, Home or End).
        ID_RAIL if notify == LBN_SELCHANGE => {
            let picked = unsafe { SendMessageW(inner.rail, LB_GETCURSEL, None, None).0 };
            if let Some(page) = Page::from_index(picked) {
                unsafe { show_page(inner, page) };
            }
        }
        ID_OLLAMA_LINK if notify == BN_CLICKED && inner.ollama_fix => start_gpu_fix(inner),
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
        ID_OPEN_CONFIG if notify == BN_CLICKED => open_config_file(inner),
        ID_PULL_BUTTON if notify == BN_CLICKED => on_pull_button(inner),
        ID_RESET_PROMPT => {
            set_text(inner.prompt_edit, &prompt_to_edit(DEFAULT_PROMPT));
        }
        ID_CARD_CORNER if notify == CBN_SELCHANGE => sync_card_display_enabled(inner.hwnd),
        ID_GEMINI_MODEL if notify == CBN_SELCHANGE => sync_gemini_thinking_enabled(inner.hwnd),
        // #635: the user picked a model themselves, so a pending "select the
        // pulled model" must not override it.
        ID_OLLAMA_MODEL if notify == CBN_SELCHANGE => inner.pull_select = None,
        ID_CHANGE_PRIMARY if notify == BN_CLICKED => start_change(inner, HK_PRIMARY),
        ID_CHANGE_SECONDARY if notify == BN_CLICKED => start_change(inner, HK_SECONDARY),
        ID_CHANGE_PAUSE if notify == BN_CLICKED => start_change(inner, HK_PAUSE),
        ID_MODE if notify == CBN_SELCHANGE => {
            if let Some(combo) = get_dlg_item(inner.hwnd, ID_MODE) {
                let picked = unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0 };
                if let Some(mode) = mode_at(picked) {
                    send_command(inner, crate::app::SettingsCommand::Mode(mode));
                }
            }
        }
        ID_PAUSE_BUTTON if notify == BN_CLICKED => {
            // A pending Change leaves the keyboard hook capturing, which would
            // eat the menu's arrow keys and Esc as a new shortcut.
            if inner.learning.is_some() {
                stop_learning(inner);
                set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
            }
            unsafe {
                let _ = PostMessageW(Some(inner.hwnd), WM_PAUSE_MENU, WPARAM(0), LPARAM(0));
            }
        }
        ID_RESUME_BUTTON if notify == BN_CLICKED => {
            send_command(inner, crate::app::SettingsCommand::Resume)
        }
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
fn focus_and_select_field(inner: &mut SettingsInner, id: i32) {
    let Some(ctrl) = get_dlg_item(inner.hwnd, id) else {
        return;
    };
    unsafe {
        // A control on a hidden page cannot take focus: show its page first.
        if let Some(page) = inner
            .children
            .iter()
            .find(|c| c.hwnd == ctrl)
            .and_then(|c| c.page)
        {
            show_page(inner, page);
        }
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
/// #597: the "Card display:" caption, greyed with its combo.
const ID_CARD_MONITOR_LABEL: i32 = 139;
/// #566: the Ollama group's Pull row: the model name box, the Pull/Stop
/// button, and the progress or error line under them. Not form fields.
const ID_PULL_NAME: i32 = 140;
const ID_PULL_BUTTON: i32 = 141;
const ID_PULL_STATUS: i32 = 142;
/// #569: the Gemini "Thinking:" caption, greyed with its combo for a model
/// that ignores the setting.
const ID_GEMINI_EFFORT_LABEL: i32 = 143;
const ID_MODEL_LIST_HINT: i32 = 144;
/// The "Open config file" button beside the model hint (#823).
const ID_OPEN_CONFIG: i32 = 145;
/// #972: the two helper lines that had no id, so the hint colour could not
/// find them: the note under the Capture group's two dropdowns and the one
/// under the redact checkbox.
const ID_CAPTURE_DETAIL_HINT: i32 = 146;
const ID_REDACT_HINT: i32 = 147;
/// #872: the navigation rail (a list box), a direct child of the window.
const ID_RAIL: i32 = 148;
/// #872: the Actions page's explanation. That page has no setting yet.
const ID_ACTIONS_NOTE: i32 = 149;
/// #873: the Mode combo, the pause line with its Pause and Resume buttons, and
/// the pause shortcut's label and Change button, all on General. Mode and
/// Pause act at once through the tray's own handlers; they are not form fields.
const ID_MODE: i32 = 150;
const ID_PAUSE_STATUS: i32 = 151;
const ID_PAUSE_BUTTON: i32 = 152;
const ID_RESUME_BUTTON: i32 = 153;
const ID_PAUSE_LABEL: i32 = 154;
const ID_CHANGE_PAUSE: i32 = 155;

/// #872: the five pages of the window, in rail order. Each existing control
/// lives on exactly one (see [`PAGE_CONTROLS`]); the rail shows one page at a
/// time and hides the others.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Page {
    General,
    Models,
    Privacy,
    Actions,
    Advanced,
}

impl Page {
    const ALL: [Page; 5] = [
        Page::General,
        Page::Models,
        Page::Privacy,
        Page::Actions,
        Page::Advanced,
    ];

    /// The rail item's text, which is also the page's name for a screen reader.
    fn label(self) -> &'static str {
        match self {
            Page::General => "General",
            Page::Models => "Models",
            Page::Privacy => "Privacy",
            Page::Actions => "Actions",
            Page::Advanced => "Advanced",
        }
    }

    /// The rail item's Segoe Fluent Icons glyph.
    fn glyph(self) -> char {
        match self {
            Page::General => '\u{E713}',
            Page::Models => '\u{E9D9}',
            Page::Privacy => '\u{E72E}',
            Page::Actions => '\u{E945}',
            Page::Advanced => '\u{E90F}',
        }
    }

    fn index(self) -> usize {
        Page::ALL.iter().position(|p| *p == self).unwrap_or(0)
    }

    /// The page at rail position `index`; `None` for a position past the list
    /// (a list box answers `LB_ERR`, -1, for "nothing selected").
    fn from_index(index: isize) -> Option<Page> {
        usize::try_from(index)
            .ok()
            .and_then(|i| Page::ALL.get(i).copied())
    }

    /// The page after (`+1`) or before (`-1`) this one, wrapping at the ends
    /// (Ctrl+PageDown and Ctrl+PageUp).
    fn step(self, by: isize) -> Page {
        let n = Page::ALL.len() as isize;
        let i = (self.index() as isize + by).rem_euclid(n);
        Page::ALL[i as usize]
    }

    /// The page Ctrl plus this key picks: `1` to `5`, in rail order.
    fn from_digit_key(vk: u32) -> Option<Page> {
        let digit = vk.checked_sub('1' as u32)?;
        Page::ALL.get(digit as usize).copied()
    }
}

/// The rail's pages by name, in order: what the gallery checks it has a
/// picture of every one of.
#[cfg(test)]
pub(crate) fn page_labels() -> Vec<&'static str> {
    Page::ALL.iter().map(|p| p.label()).collect()
}

/// The rail's accessible name.
const RAIL_NAME: &str = "Settings pages";

/// The Actions page's note, in plain words. Import and export still live in
/// the tray's Help group.
const ACTIONS_NOTE: &str = "Actions are what Wingman does once you confirm: reply to an email, fill in a form, add a calendar event. Importing and exporting your own actions is in the tray menu, under Help.";

/// Every control id on each page, for the test that nothing is lost or listed
/// twice. The window builds its pages with the same grouping (`Ctx::page`),
/// and a test compares the two.
#[cfg(test)]
const PAGE_CONTROLS: [(Page, &[i32]); 5] = [
    (
        Page::General,
        &[
            ID_MODE,
            ID_PAUSE_STATUS,
            ID_PAUSE_BUTTON,
            ID_RESUME_BUTTON,
            ID_PRIMARY_LABEL,
            ID_CHANGE_PRIMARY,
            ID_SECONDARY_LABEL,
            ID_CHANGE_SECONDARY,
            ID_PAUSE_LABEL,
            ID_CHANGE_PAUSE,
            ID_HOTKEY_STATUS,
            ID_AUTOSTART,
            ID_CARD_SECONDS,
            ID_TEXT_SCALE_TRACK,
            ID_TEXT_SCALE_LABEL,
            ID_CARD_CORNER,
            ID_CARD_MONITOR_LABEL,
            ID_CARD_MONITOR,
        ],
    ),
    (
        Page::Models,
        &[
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
            ID_GEMINI_EFFORT_LABEL,
            ID_GEMINI_EFFORT,
            ID_MODEL_LIST_HINT,
            ID_OPEN_CONFIG,
            ID_OLLAMA_MODEL,
            ID_OLLAMA_STATUS,
            ID_OLLAMA_LINK,
            ID_PULL_NAME,
            ID_PULL_BUTTON,
            ID_PULL_STATUS,
        ],
    ),
    (
        Page::Privacy,
        &[
            ID_MAX_EDGE,
            ID_MONITOR,
            ID_CAPTURE_DETAIL_HINT,
            ID_REDACT,
            ID_REDACT_HINT,
            ID_REQUIRE_TICK_FOR,
        ],
    ),
    (Page::Actions, &[ID_ACTIONS_NOTE]),
    (
        Page::Advanced,
        &[ID_PROMPT_TOGGLE, ID_PROMPT_EDIT, ID_RESET_PROMPT],
    ),
];

/// The ids that belong to no page: the rail, the scrolling body that holds the
/// pages, and the footer.
#[cfg(test)]
const CHROME_IDS: &[i32] = &[ID_RAIL, ID_BODY, ID_SAVE, ID_CANCEL, ID_VALIDATION_MESSAGE];

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
    ("ID_CARD_MONITOR_LABEL", ID_CARD_MONITOR_LABEL),
    ("ID_PULL_NAME", ID_PULL_NAME),
    ("ID_PULL_BUTTON", ID_PULL_BUTTON),
    ("ID_PULL_STATUS", ID_PULL_STATUS),
    ("ID_GEMINI_EFFORT_LABEL", ID_GEMINI_EFFORT_LABEL),
    ("ID_MODEL_LIST_HINT", ID_MODEL_LIST_HINT),
    ("ID_OPEN_CONFIG", ID_OPEN_CONFIG),
    ("ID_CAPTURE_DETAIL_HINT", ID_CAPTURE_DETAIL_HINT),
    ("ID_REDACT_HINT", ID_REDACT_HINT),
    ("ID_RAIL", ID_RAIL),
    ("ID_ACTIONS_NOTE", ID_ACTIONS_NOTE),
    ("ID_MODE", ID_MODE),
    ("ID_PAUSE_STATUS", ID_PAUSE_STATUS),
    ("ID_PAUSE_BUTTON", ID_PAUSE_BUTTON),
    ("ID_RESUME_BUTTON", ID_RESUME_BUTTON),
    ("ID_PAUSE_LABEL", ID_PAUSE_LABEL),
    ("ID_CHANGE_PAUSE", ID_CHANGE_PAUSE),
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
const EN_KILLFOCUS: u32 = 0x0200;
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

/// The width of one page, in dp: what the single column used to be.
const PAGE_W_DP: i32 = 620;
/// The navigation rail's width, in dp, and the size of its items.
const RAIL_W_DP: i32 = crate::ui::theme::RAIL_W_DP;
const RAIL_ITEM_H_DP: i32 = crate::ui::theme::RAIL_ITEM_H_DP;
const RAIL_TOP_PAD_DP: i32 = crate::ui::theme::RAIL_TOP_PAD_DP;
/// The client width: the rail and one page.
const WIN_W_DP: i32 = RAIL_W_DP + PAGE_W_DP;
/// The tallest the window is made, in dp, when the Check my work prompt is open
/// (#342): the prompt box takes what is left. With it closed the window is as
/// tall as the fullest page (`SettingsInner::tallest_page_dp`), never taller
/// than this or the monitor allows.
const WIN_H_DP: i32 = 760;

/// The one-line hint under the model dropdowns (#427). Plain words, no dash.
const MODEL_LIST_HINT: &str = "Missing a model? Add it in the config file.";

/// #674: shown instead of [`MODEL_LIST_HINT`] when the mode keeps every
/// request on this computer, so the user knows why no live cloud list loads.
const MODEL_LIST_HINT_LOCAL: &str = "Local mode: live cloud model lists are not fetched.";
const MODEL_LIST_HINT_OFFLINE: &str = "Offline mode: live cloud model lists are not fetched.";

/// Whether the mode allows asking a cloud vendor for its model list. Both
/// Local and Offline are loopback only (#674); the saved key rides along.
fn cloud_list_allowed(mode: crate::mode::Mode) -> bool {
    !matches!(mode, crate::mode::Mode::Local | crate::mode::Mode::Offline)
}

/// The hint line under the cloud model dropdowns for `mode`.
fn model_list_hint_for(mode: crate::mode::Mode) -> &'static str {
    match mode {
        crate::mode::Mode::Local => MODEL_LIST_HINT_LOCAL,
        crate::mode::Mode::Offline => MODEL_LIST_HINT_OFFLINE,
        _ => MODEL_LIST_HINT,
    }
}
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
/// Where a page's first group starts, in dp from the top of the body (#872):
/// level with the rail's first item.
const PAGE_TOP_PAD_DP: i32 = crate::ui::theme::SPACE_SM_DP;
const ROW_H: i32 = 22;
const ROW_GAP: i32 = 6;
const GROUP_GAP: i32 = 14;
const GROUP_LABEL_TOP: i32 = 20; // room for the groupbox caption
const LABEL_W: i32 = 140;
const BTN_W: i32 = 100;
const BTN_H: i32 = 28;

/// The DPI the Settings window lays out at: the window's real one, floored
/// at 1. Every layout call goes through here so the UI gallery can simulate
/// another display scale (`--dpi`, issue #745); debug and test builds only.
fn dpi_for(hwnd: HWND) -> u32 {
    #[cfg(any(test, debug_assertions))]
    if let Some(dpi) = forced_dpi() {
        return dpi;
    }
    unsafe { GetDpiForWindow(hwnd) }.max(1)
}

#[cfg(any(test, debug_assertions))]
thread_local! {
    /// The gallery's simulated DPI for this thread's Settings window, if any.
    static FORCED_DPI: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, debug_assertions))]
fn forced_dpi() -> Option<u32> {
    FORCED_DPI.with(std::cell::Cell::get)
}

fn to_px(dp: i32, dpi: u32) -> i32 {
    ((dp as i64 * dpi as i64 + 48) / 96) as i32
}

/// Below this the prompt box stops being usable; scroll rather than shrink
/// further. (`MIN_PROMPT_EDIT_H` in dp, three text rows tall.)
const MIN_PROMPT_EDIT_H: i32 = ROW_H * 3;

/// Width of the Ollama group's link button (#343), in dp.
const OLLAMA_LINK_W: i32 = 120;

/// Width of the Pull / Stop button (#566), in dp.
const OLLAMA_PULL_BTN_W: i32 = 80;
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
/// just tall enough for the fullest page (`tallest_page_dp`, its content with
/// the prompt closed) and the footer, and never taller than the fit; anything
/// beyond the fit scrolls (#481). The height is the same whichever page shows,
/// so switching pages never resizes the window (#872).
fn window_height_dp(tallest_page_dp: i32, expanded: bool, fitted_dp: i32, nc_dp: i32) -> i32 {
    if expanded {
        return fitted_dp;
    }
    (tallest_page_dp + FOOTER_H_DP + nc_dp).min(fitted_dp)
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
    /// The page this control is on, or `None` for the footer, which every page
    /// shares (#872).
    page: Option<Page>,
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
    let content_px = to_px(page_content_dp(inner, &new), dpi);
    let rail_px = to_px(RAIL_W_DP, dpi);
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

    let new_font = build_font(dpi);
    // No repaint between the individual moves: one redraw at the end, so a
    // drag across monitors does not flicker.
    SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(0)), None);
    if have_body {
        let _ = SetWindowPos(
            inner.body,
            None,
            rail_px,
            0,
            (client_w_px - rail_px).max(1),
            view_px,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
    if !inner.rail.0.is_null() {
        place_rail(inner.rail, dpi, new_font);
    }
    for c in &inner.children {
        let (x, y, w, h) = child_rect_px(c, &old, &new, dpi);
        // The footer keeps its design x, so Save and Cancel end where the
        // group boxes end (#625).
        let (x, y) = if c.anchor == Anchor::Footer {
            (x, y)
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

/// The current page's content height in dp, padding included. Advanced follows
/// the prompt's state through `layout`; every other page is fixed.
fn page_content_dp(inner: &SettingsInner, layout: &PromptFooterLayout) -> i32 {
    if inner.page == Page::Advanced {
        layout.content_h()
    } else {
        inner.page_content_dp[inner.page.index()]
    }
}

/// The height of the fullest page in dp, with the prompt closed: what the
/// window is made tall enough for (#872).
fn tallest_page_dp(inner: &SettingsInner) -> i32 {
    inner.page_content_dp.iter().copied().max().unwrap_or(0)
}

/// Whether the child `c` should be visible: it is on the page that shows, and
/// the few controls with a state of their own are in the state that shows them
/// (the Ollama button only has something to offer sometimes, the prompt box is
/// folded away until asked for). The footer is on every page.
unsafe fn child_visible(inner: &SettingsInner, c: &ChildLayout) -> bool {
    let Some(page) = c.page else {
        return true;
    };
    if page != inner.page {
        return false;
    }
    match GetDlgCtrlID(c.hwnd) {
        ID_OLLAMA_LINK => inner.ollama_link_shown,
        ID_PROMPT_EDIT | ID_RESET_PROMPT => inner.prompt_expanded,
        _ => true,
    }
}

/// Shows the controls of the current page and hides every other control, so a
/// hidden page is out of the Tab order and the accessibility tree (#872).
unsafe fn sync_visibility(inner: &SettingsInner) {
    for c in &inner.children {
        let cmd = if child_visible(inner, c) {
            SW_SHOW
        } else {
            SW_HIDE
        };
        let _ = ShowWindow(c.hwnd, cmd);
    }
}

/// #342: makes the window match `inner.prompt_expanded`: shows or hides the
/// prompt box and its reset button, relabels the toggle, resizes the window
/// (kept inside the monitor's work area) and re-lays-out the children.
/// The prompt text stays in the hidden edit, so Save still reads it.
unsafe fn show_prompt_state(inner: &mut SettingsInner) {
    sync_visibility(inner);
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
        window_height_dp(tallest_page_dp(inner), inner.prompt_expanded, fitted, nc_dp),
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

/// The width of the window's frame (outer minus client), so the client area,
/// not the outer rect, is `window_width_px` wide (#625).
fn nc_width_px(hwnd: HWND) -> i32 {
    let mut wr = windows::Win32::Foundation::RECT::default();
    let mut cr = windows::Win32::Foundation::RECT::default();
    unsafe {
        if GetWindowRect(hwnd, &mut wr).is_err() || GetClientRect(hwnd, &mut cr).is_err() {
            return 0;
        }
    }
    ((wr.right - wr.left) - (cr.right - cr.left)).max(0)
}

/// #624: the width of the Card group's first label. Its text is about 262px
/// at 96 dpi; at 260dp it wrapped and the second line was clipped.
const CARD_SECONDS_EDIT_W: i32 = 50;
/// The Card group's first row (#823): a short title, the number box, then
/// what the number means. 0 is explained in words, not as a code.
const CARD_SECONDS_LABEL: &str = "Hide the card after:";
const CARD_SECONDS_UNIT: &str = "seconds (0 keeps it open)";

/// #597: whether the Card display choice has any effect for this Card
/// position label. "Next to the mouse" follows the pointer's own monitor, so
/// the display choice is ignored there.
fn card_display_applies(corner_label: &str) -> bool {
    label_to_card_corner(corner_label) != Some(CardCorner::FollowMouse)
}

/// #597: greys the Card display combo and its caption to match the chosen
/// Card position.
fn sync_card_display_enabled(hwnd: HWND) {
    let on = card_display_applies(&combo_selected_text(hwnd, ID_CARD_CORNER));
    for id in [ID_CARD_MONITOR, ID_CARD_MONITOR_LABEL] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            unsafe {
                let _ = EnableWindow(h, on);
            }
        }
    }
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

/// Where a checkbox's own text starts, in dp from the control's left edge
/// (13 dp box plus a 3 dp gap; MEASURED 2026-10-17 at 250 %, the one gallery scale where the system draws the box at its true size: 40 px). A
/// note under a checkbox starts here so it lines up with the label above it
/// (#972).
const CHECKBOX_TEXT_INDENT_DP: i32 = 16;

/// #972: whether `id` is a helper line: secondary text under or beside a
/// control, drawn in the hint colour. Status lines that carry a result (the
/// Ollama check, the pull progress) are not hints.
fn is_hint_id(id: i32) -> bool {
    matches!(
        id,
        ID_HOTKEY_STATUS | ID_MODEL_LIST_HINT | ID_CAPTURE_DETAIL_HINT | ID_REDACT_HINT
    )
}

/// The text colour of the helper lines (#824, #972). Secondary text, never the
/// disabled grey: `theme::LIGHT.text_dim` on the light button face, and the
/// system's own button text in high contrast (where grey means disabled).
/// Settings is drawn on the system face today; the dark-mode work swaps in
/// the theme's tokens.
fn hint_text_color(high_contrast: bool, btn_text: u32) -> u32 {
    if high_contrast {
        btn_text
    } else {
        crate::ui::theme::LIGHT.text_dim
    }
}

/// #934: the colours of a text box. Tokens in a normal theme, system colours in
/// high contrast. Settings is drawn on the system face and does not follow
/// dark mode yet, so a normal theme here is always the light one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct EditColors {
    fill: u32,
    border: u32,
    text: u32,
    /// The line along the bottom of a box that has the keyboard focus.
    focus_line: u32,
    /// What lies behind the box, seen in the four rounded corners.
    face: u32,
    /// A disabled box: a quieter fill, its own text, and no focus line.
    disabled_fill: u32,
    disabled_border: u32,
    disabled_text: u32,
}

/// How far a disabled box's fill moves from the text box fill toward the
/// face behind it, in percent.
const EDIT_DISABLED_FILL_PCT: u32 = 60;

fn edit_colors(
    high_contrast: bool,
    sys: &crate::ui::theme::SysColors,
    accent: u32,
    face: u32,
) -> EditColors {
    use crate::ui::card_buttons::blend;
    use crate::ui::theme::{menu_text_color, Theme, Tokens};
    let theme = Theme::resolve(true, high_contrast);
    let t = Tokens::for_theme(theme, sys);
    let (disabled_fill, disabled_border) = if high_contrast {
        // No tints in high contrast: the window colour, and GrayText ink.
        (sys.window, sys.gray_text)
    } else {
        (
            blend(t.control_fill, face, EDIT_DISABLED_FILL_PCT),
            t.border,
        )
    };
    EditColors {
        fill: t.control_fill,
        border: t.border,
        text: t.text,
        focus_line: if high_contrast { sys.highlight } else { accent },
        face,
        disabled_fill,
        disabled_border,
        disabled_text: menu_text_color(theme, &t, sys, false, true),
    }
}

#[cfg(test)]
thread_local! {
    /// How many times the colours were rebuilt (a registry read each) on
    /// this thread: painting must not move it.
    static EDIT_STYLE_BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The colours for a text box right now. Reads the registry (the accent), so
/// only [`edit_style`] calls it.
unsafe fn current_edit_colors() -> EditColors {
    use crate::ui::theme::{SysColors, Theme, LIGHT};
    use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_3DFACE};
    let high_contrast = crate::ui::card_buttons::system_high_contrast().is_some();
    edit_colors(
        high_contrast,
        &SysColors::read(),
        crate::ui::card_buttons::system_accent(Theme::Light, LIGHT.surface),
        GetSysColor(COLOR_3DFACE),
    )
}

/// The focus line of a text box `w` by `h` px: along the bottom, between the
/// rounded corners, as thick as Windows 11's 2 dp accent underline. The
/// bottom border row is part of it, so the line reads as the border turning
/// to the accent. `(left, top, right, bottom)` in window px.
fn edit_focus_line_rect(w: i32, h: i32, dpi: u32) -> (i32, i32, i32, i32) {
    let radius = to_px(crate::ui::theme::CONTROL_RADIUS_DP, dpi);
    (
        radius,
        h - to_px(crate::ui::theme::FOCUS_LINE_DP, dpi).max(2),
        w - radius,
        h,
    )
}

const EDIT_SUBCLASS_ID: usize = 0x934;

/// Everything a text box paint needs, built once per theme change: the
/// colours and the two brushes `WM_CTLCOLOREDIT` / `WM_CTLCOLORSTATIC` hand
/// back. Owned here and freed by [`invalidate_edit_style`].
#[derive(Clone, Copy)]
struct EditStyle {
    colors: EditColors,
    brush: windows::Win32::Graphics::Gdi::HBRUSH,
    disabled_brush: windows::Win32::Graphics::Gdi::HBRUSH,
}

thread_local! {
    static EDIT_STYLE: std::cell::Cell<Option<EditStyle>> = const { std::cell::Cell::new(None) };
}

/// The cached style, built on first use after a theme change. Painting and
/// colour messages run on the UI thread, so they never reach the registry.
unsafe fn edit_style() -> EditStyle {
    use windows::Win32::Graphics::Gdi::CreateSolidBrush;
    if let Some(style) = EDIT_STYLE.get() {
        return style;
    }
    #[cfg(test)]
    EDIT_STYLE_BUILDS.set(EDIT_STYLE_BUILDS.get() + 1);
    let colors = current_edit_colors();
    let solid = |c: u32| CreateSolidBrush(windows::Win32::Foundation::COLORREF(c));
    let style = EditStyle {
        colors,
        brush: solid(colors.fill),
        disabled_brush: solid(colors.disabled_fill),
    };
    EDIT_STYLE.set(Some(style));
    style
}

/// Drops the cached style and its brushes; the next paint rebuilds it. Called
/// on a theme, accent or system colour change, and when Settings closes.
unsafe fn invalidate_edit_style() {
    if let Some(style) = EDIT_STYLE.take() {
        let _ = DeleteObject(HGDIOBJ(style.brush.0));
        let _ = DeleteObject(HGDIOBJ(style.disabled_brush.0));
    }
}

/// Whether `hwnd` is a text box (an `Edit`), which a disabled one reports to
/// its parent as a static.
unsafe fn is_edit_control(hwnd: HWND) -> bool {
    let mut name = [0u16; 8];
    let n = windows::Win32::UI::WindowsAndMessaging::GetClassNameW(hwnd, &mut name) as usize;
    String::from_utf16_lossy(&name[..n.min(name.len())]).eq_ignore_ascii_case("Edit")
}

/// Answers a text box's colour request: the ink and fill on `hdc`, and the
/// brush for the box's interior. `disabled` picks the disabled look.
unsafe fn answer_edit_colors(hdc: windows::Win32::Graphics::Gdi::HDC, disabled: bool) -> LRESULT {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{SetBkColor, SetTextColor};
    let s = edit_style();
    let (ink, fill, brush) = if disabled {
        (
            s.colors.disabled_text,
            s.colors.disabled_fill,
            s.disabled_brush,
        )
    } else {
        (s.colors.text, s.colors.fill, s.brush)
    };
    SetTextColor(hdc, COLORREF(ink));
    SetBkColor(hdc, COLORREF(fill));
    LRESULT(brush.0 as isize)
}

/// Draws a text box's frame onto `hdc` (#934): the Windows 11 look of a
/// control, a 1 px border with 4 dp corners on the token fill, and the accent
/// line along the bottom while it has the focus. `w` by `h` is the whole box
/// in px. Only the ring the system reserves for the edge is drawn, so the
/// client area, any scrollbar and the text are the edit's own.
unsafe fn draw_edit_frame(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    w: i32,
    h: i32,
    dpi: u32,
    colors: &EditColors,
    focused: bool,
    enabled: bool,
) {
    use windows::Win32::Graphics::Gdi::{
        CreatePen, CreateSolidBrush, ExcludeClipRect, FillRect, RoundRect, SelectObject,
        PS_INSIDEFRAME,
    };
    let rgb = |c: u32| windows::Win32::Foundation::COLORREF(c);
    let rect = |l, t, r, b| windows::Win32::Foundation::RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    };
    let edge =
        GetSystemMetricsForDpi(windows::Win32::UI::WindowsAndMessaging::SM_CXEDGE, dpi).max(1);
    ExcludeClipRect(hdc, edge, edge, w - edge, h - edge);
    let face = CreateSolidBrush(rgb(colors.face));
    FillRect(hdc, &rect(0, 0, w, h), face);
    let (fill_c, border_c) = if enabled {
        (colors.fill, colors.border)
    } else {
        (colors.disabled_fill, colors.disabled_border)
    };
    let fill = CreateSolidBrush(rgb(fill_c));
    let pen = CreatePen(PS_INSIDEFRAME, 1, rgb(border_c));
    let old_brush = SelectObject(hdc, HGDIOBJ(fill.0));
    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
    let round = 2 * to_px(crate::ui::theme::CONTROL_RADIUS_DP, dpi);
    let _ = RoundRect(hdc, 0, 0, w, h, round, round);
    if focused && enabled {
        let (l, t, r, b) = edit_focus_line_rect(w, h, dpi);
        let line = CreateSolidBrush(rgb(colors.focus_line));
        FillRect(hdc, &rect(l, t, r, b), line);
        let _ = DeleteObject(HGDIOBJ(line.0));
    }
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
    let _ = DeleteObject(HGDIOBJ(pen.0));
    let _ = DeleteObject(HGDIOBJ(fill.0));
    let _ = DeleteObject(HGDIOBJ(face.0));
}

/// Paints `hwnd`'s frame on its own window DC (see [`draw_edit_frame`]).
unsafe fn paint_edit_frame(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{GetWindowDC, ReleaseDC};
    use windows::Win32::UI::Input::KeyboardAndMouse::GetFocus;
    let mut wr = windows::Win32::Foundation::RECT::default();
    if GetWindowRect(hwnd, &mut wr).is_err() {
        return;
    }
    let hdc = GetWindowDC(Some(hwnd));
    if hdc.is_invalid() {
        return;
    }
    draw_edit_frame(
        hdc,
        wr.right - wr.left,
        wr.bottom - wr.top,
        dpi_for(hwnd),
        &edit_style().colors,
        GetFocus() == hwnd,
        windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled(hwnd).as_bool(),
    );
    ReleaseDC(Some(hwnd), hdc);
}

/// Gives every Settings text box the frame of [`paint_edit_frame`] (#934).
unsafe extern "system" fn edit_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWL_STYLE, WM_KILLFOCUS, WM_NCPAINT, WM_SETFOCUS, WS_HSCROLL,
    };
    match msg {
        WM_NCPAINT => {
            // A box with a scrollbar (the prompt) lets the system draw that
            // first; the frame goes on top of the ring either way.
            let scrolls = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & (WS_VSCROLL.0 | WS_HSCROLL.0);
            let result = if scrolls != 0 {
                DefSubclassProc(hwnd, msg, wparam, lparam)
            } else {
                LRESULT(0)
            };
            paint_edit_frame(hwnd);
            result
        }
        // The focus line comes and goes with the focus.
        WM_SETFOCUS | WM_KILLFOCUS => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                windows::Win32::Graphics::Gdi::RDW_FRAME | RDW_INVALIDATE,
            );
            result
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(edit_subclass), EDIT_SUBCLASS_ID);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

/// #935: the divider above the footer, as `(left, top, right, bottom)` in
/// client px. One physical pixel on the footer's first row, directly under the
/// body, so a row the scrolling body cuts off reads as an edge.
fn footer_divider_rect(client_w_px: i32, client_h_px: i32, dpi: u32) -> (i32, i32, i32, i32) {
    let top = client_h_px - to_px(FOOTER_H_DP, dpi);
    // The footer sits under the pages, right of the rail (#872).
    (to_px(RAIL_W_DP, dpi), top, client_w_px, top + 1)
}

/// #935: the divider's colour: the hairline token of the current theme, which
/// is the system's window text in high contrast, where a pale hairline would
/// not show. Settings does not follow dark mode yet, so a normal theme here is
/// the light one (as for the text boxes).
fn footer_divider_color(high_contrast: bool, sys: &crate::ui::theme::SysColors) -> u32 {
    use crate::ui::theme::{Theme, Tokens};
    Tokens::for_theme(Theme::resolve(true, high_contrast), sys).border
}

/// Paints the footer divider (#935). The class background has already filled
/// the client area; only the line is drawn here.
unsafe fn paint_footer_divider(hwnd: HWND) {
    use windows::Win32::Graphics::Gdi::{
        BeginPaint, CreateSolidBrush, EndPaint, FillRect, PAINTSTRUCT,
    };
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = windows::Win32::Foundation::RECT::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        let high_contrast = crate::ui::card_buttons::system_high_contrast().is_some();
        let ink = footer_divider_color(high_contrast, &crate::ui::theme::SysColors::read());
        let (l, t, r, b) =
            footer_divider_rect(rc.right - rc.left, rc.bottom - rc.top, dpi_for(hwnd));
        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(ink));
        let line = windows::Win32::Foundation::RECT {
            left: l,
            top: t,
            right: r,
            bottom: b,
        };
        FillRect(hdc, &line, brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }
    let _ = EndPaint(hwnd, &ps);
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
        // #824, #972: a helper line is secondary text, not the disabled grey.
        // Every other static keeps the default colours.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            if is_hint_id(GetDlgCtrlID(HWND(lparam.0 as *mut c_void))) =>
        {
            use windows::Win32::Graphics::Gdi::{
                GetSysColor, GetSysColorBrush, SetBkMode, SetTextColor, COLOR_3DFACE,
                COLOR_BTNTEXT, TRANSPARENT,
            };
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            let high_contrast = crate::ui::card_buttons::system_high_contrast().is_some();
            let ink = hint_text_color(high_contrast, GetSysColor(COLOR_BTNTEXT));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(ink));
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(GetSysColorBrush(COLOR_3DFACE).0 as isize)
        }
        // #934: a text box is the token fill with the token text on it.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLOREDIT => answer_edit_colors(
            windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
            false,
        ),
        // A disabled or read-only box asks as a static; it gets the
        // disabled look so its interior matches the frame.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            if is_edit_control(HWND(lparam.0 as *mut c_void)) =>
        {
            answer_edit_colors(
                windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
                true,
            )
        }
        WM_VSCROLL => {
            let si = body_scroll_info(hwnd);
            if si.nPage != 0 {
                let dpi = dpi_for(hwnd);
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
    let dpi = dpi_for(body);
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
    scroll_body_to(body, si.nPos + wheel_scroll_px(delta, dpi_for(main)));
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
    /// The page the next control belongs to (#872); `None` for the footer.
    page: std::cell::Cell<Option<Page>>,
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
            if class == WC_EDIT {
                // #934: the frame of a Windows 11 text box.
                unsafe {
                    let _ = windows::Win32::UI::Shell::SetWindowSubclass(
                        hwnd,
                        Some(edit_subclass),
                        EDIT_SUBCLASS_ID,
                        0,
                    );
                }
            }
            self.children.borrow_mut().push(ChildLayout {
                page: self.page.get(),
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
    // Exact match by position: CB_SELECTSTRING matches a prefix, which opened
    // "gpt-5" as "gpt-5.5" (#775). `active` is always in `list` unless empty.
    let index = list.iter().position(|m| *m == active).unwrap_or(0);
    if !list.is_empty() {
        combo_select_index(hwnd, index as i32);
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

/// The name a just-pulled model is listed under (#635): a name typed without
/// a tag is listed by Ollama with `:latest`.
fn resolve_pulled_name(models: &[ollama_admin::TagsModel], name: &str) -> String {
    if models.iter().any(|m| m.name == name) {
        return name.to_string();
    }
    let latest = format!("{name}:latest");
    if !name.contains(':') && models.iter().any(|m| m.name == latest) {
        return latest;
    }
    name.to_string()
}

/// Whether the Gemini Thinking setting does anything for `model` (#569): it
/// is only sent to the 3.x line.
fn gemini_thinking_applies(model: &str) -> bool {
    crate::provider::gemini::supports_thinking(model)
}

/// Greys the Gemini Thinking combo and its caption for a model it does not
/// apply to, so a 2.5 model does not look like it has a working setting.
fn sync_gemini_thinking_enabled(hwnd: HWND) {
    let on = gemini_thinking_applies(combo_selected_text(hwnd, ID_GEMINI_MODEL).trim());
    for id in [ID_GEMINI_EFFORT, ID_GEMINI_EFFORT_LABEL] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            unsafe {
                let _ = EnableWindow(h, on);
            }
        }
    }
}

/// Fills the Ollama model combo from the pulled models, keeping whatever is
/// selected now (#183), or selecting `prefer` (a model just pulled, #635).
/// Called on the UI thread when the background fetch reports in.
fn apply_ollama_models(hwnd: HWND, models: &[ollama_admin::TagsModel], prefer: Option<&str>) {
    let Some(combo) = get_dlg_item(hwnd, ID_OLLAMA_MODEL) else {
        return;
    };
    let shown = combo_selected_text(hwnd, ID_OLLAMA_MODEL);
    let current = prefer.unwrap_or_else(|| strip_vision_badge(shown.trim()));
    let (labels, selected) = ollama_model_entries(models, current);
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
    sync: std::sync::Arc<CloudSync>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    if !cloud_list_allowed(crate::mode::current()) {
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
        spawn_one_cloud_fetch(
            hwnd,
            index,
            key.clone(),
            slots.clone(),
            sync.clone(),
            alive.clone(),
        );
    }
}

/// #585: state shared between Settings and its model-list workers.
#[derive(Default)]
struct CloudSync {
    /// The newest fetch started for each vendor. A worker whose number is no
    /// longer current has been replaced and drops its result.
    generation: [std::sync::atomic::AtomicU64; 3],
    /// The key each list was last fetched with successfully, so leaving a
    /// key field with the same key does not fetch again, and a failed fetch
    /// is retried on the next focus-out.
    fetched_keys: std::sync::Mutex<[String; 3]>,
}

/// Whether a worker holding `mine` is still the newest for its vendor.
fn is_current_generation(newest: u64, mine: u64) -> bool {
    newest == mine
}

/// Which vendor index a command is a key field losing focus for (#585).
fn typed_key_vendor(id: i32, notify: u32) -> Option<usize> {
    if notify != EN_KILLFOCUS {
        return None;
    }
    match id {
        ID_OPENAI_KEY => Some(0),
        ID_ANTHROPIC_KEY => Some(1),
        ID_GEMINI_KEY => Some(2),
        _ => None,
    }
}

/// The key to fetch a model list with after the user leaves a key field, or
/// `None` when there is nothing to do: the field is empty, still shows the
/// mask of the saved key, shows the unreadable-key placeholder, or holds the
/// key the list was already fetched with (#585).
fn key_to_fetch(original: &str, form_text: &str, last_fetched: &str) -> Option<String> {
    let key = resolve_key_field(original, form_text);
    let key = key.trim();
    if key.is_empty() || key == crate::config::UNREADABLE_KEY_MARKER || key == last_fetched.trim() {
        return None;
    }
    Some(key.to_string())
}

/// Whether a typed `key` has the shape of vendor `index`'s keys (#678), so a
/// key pasted into the wrong vendor's field is not sent to that vendor.
/// OpenAI keys start `sk-` (Anthropic's `sk-ant-` do not count), Anthropic's
/// `sk-ant-`, Gemini's `AIza`.
fn key_fits_vendor(index: usize, key: &str) -> bool {
    let key = key.trim();
    match index {
        0 => key.starts_with("sk-") && !key.starts_with("sk-ant-"),
        1 => key.starts_with("sk-ant-"),
        _ => key.starts_with("AIza"),
    }
}

/// #585: the user left a key field. Fetch that vendor's list with what is
/// typed now, saved or not. Nothing runs in Offline mode.
fn fetch_for_typed_key(inner: &mut SettingsInner, index: usize) {
    let (edit_id, original) = {
        let p = &inner.original.providers;
        match index {
            0 => (ID_OPENAI_KEY, p.openai.api_key.as_str()),
            1 => (ID_ANTHROPIC_KEY, p.anthropic.api_key.as_str()),
            _ => (ID_GEMINI_KEY, p.gemini.api_key.as_str()),
        }
    };
    let Some(edit) = get_dlg_item(inner.hwnd, edit_id) else {
        return;
    };
    let last = inner
        .cloud_sync
        .fetched_keys
        .lock()
        .map(|g| g[index].clone())
        .unwrap_or_default();
    let Some(key) = key_to_fetch(original, &get_text(edit), &last) else {
        return;
    };
    if !cloud_list_allowed(crate::mode::current()) {
        return;
    }
    if !key_fits_vendor(index, &key) {
        // No key text: the report is pasted into public issues.
        crate::diagnostics::note_warning(&format!(
            "Live model list skipped for {}: the typed key has an unrecognised format",
            CLOUD_VENDORS[index].tag()
        ));
        return;
    }
    spawn_one_cloud_fetch(
        inner.hwnd,
        index,
        key,
        inner.cloud_models.clone(),
        inner.cloud_sync.clone(),
        inner.alive.clone(),
    );
}

fn spawn_one_cloud_fetch(
    hwnd: HWND,
    index: usize,
    key: String,
    slots: CloudModels,
    sync: std::sync::Arc<CloudSync>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) {
    let vendor = CLOUD_VENDORS[index];
    let mine = sync.generation[index].fetch_add(1, Ordering::SeqCst) + 1;
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
            // A newer fetch for this vendor started while this one ran: its
            // result wins, so this one is dropped. The check and the store
            // happen under one lock, so an older worker cannot pass the check
            // and then store after a newer one finished (#678).
            let Ok(mut guard) = slots.lock() else {
                return;
            };
            if !is_current_generation(sync.generation[index].load(Ordering::SeqCst), mine) {
                return;
            }
            guard[index] = Some(ids);
            // The fetched-key record is written under the same slots lock
            // (always slots first, then keys), so it cannot go stale.
            if let Ok(mut keys) = sync.fetched_keys.lock() {
                keys[index] = key.trim().to_string();
            }
            drop(guard);
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
                // Tell the window (wparam 1) so a pending "select the pulled
                // model" does not wait for a list that is not coming.
                post_while_open(&alive, || unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_raw as *mut c_void)),
                        WM_OLLAMA_MODELS,
                        WPARAM(1),
                        LPARAM(0),
                    );
                });
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
            let (status, listening, stock_tray, gpu) = ollama_status_line(&cfg, ollama_configured);
            // Only searched for when nothing listens: it is not needed
            // otherwise.
            let installed = !listening && ollama_installed();
            let section = ollama_section(status, listening, installed, ollama_configured);
            let section = without_link_for_stock_tray(section, stock_tray);
            let section = with_gpu_fix(
                section,
                gpu_fix_offer(listening, stock_tray, gpu, &cfg.base_url),
            );
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
    inner.ollama_fix = section.fix;
    if let Some(link) = get_dlg_item(inner.hwnd, ID_OLLAMA_LINK) {
        if section.fix {
            set_text(link, "Fix it");
        } else if let Some((label, _)) = section.link {
            set_text(link, label);
        }
    }
    inner.ollama_link_shown = section.link.is_some() || section.fix;
    unsafe { sync_visibility(inner) };
}

/// Posted to the Settings window when the pull worker has stored a new
/// progress line in [`SettingsInner::pull_slot`] (#566).
const WM_OLLAMA_PULL: u32 = 0x8000 + 44; // WM_APP + 44

/// The pull worker's latest line, shared with the window.
#[derive(Default, Clone)]
struct PullUpdate {
    text: String,
    /// The worker is done (it posts nothing after this).
    finished: bool,
    /// Finished and the model is now pulled.
    succeeded: bool,
    /// The model that was pulled, set with `succeeded`.
    model: Option<String>,
}

type PullSlot = std::sync::Arc<std::sync::Mutex<PullUpdate>>;

/// The fewest milliseconds between two progress posts from the worker, so a
/// fast local download does not flood the window with repaints.
const PULL_POST_INTERVAL_MS: u128 = 100;

/// Caps `s` at `max` characters, ending in "..." when it was cut.
fn cap_text(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(3)).collect();
    out.push_str("...");
    out
}

/// What Settings says when a pull ends in an error (#566): a plain sentence,
/// never the raw transport error, capped to fit the status line.
fn pull_error_text(e: &anyhow::Error) -> String {
    let msg = e.to_string();
    let text = if msg.contains("transport error") {
        "Could not reach Ollama. Start it, then try again.".to_string()
    } else if let Some(reason) = msg.strip_prefix("ollama: pull failed: ") {
        format!("Pull failed: {reason}")
    } else {
        format!("Pull failed: {msg}")
    };
    cap_text(&text.replace('\u{2014}', "-"), OLLAMA_TEXT_BUDGET)
}

/// The Pull / Stop button (#566). Idle: checks the typed name and starts the
/// worker. Running: stops it at once; the worker notices at its next line
/// and exits without posting.
fn on_pull_button(inner: &mut SettingsInner) {
    if let Some(cancel) = inner.pull_cancel.take() {
        cancel.store(true, Ordering::SeqCst);
        set_pull_idle(
            inner.hwnd,
            "Pull stopped. Pulling the same name again resumes.",
        );
        return;
    }
    let raw = get_dlg_item(inner.hwnd, ID_PULL_NAME)
        .map(get_text)
        .unwrap_or_default();
    let name = match ollama_admin::validate_pull_name(&raw) {
        Ok(name) => name,
        Err(message) => {
            set_pull_status(inner.hwnd, message);
            return;
        }
    };
    let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    inner.pull_slot = Default::default();
    inner.pull_gen = inner.pull_gen.wrapping_add(1);
    if !spawn_ollama_pull(
        inner.hwnd,
        inner.pull_gen,
        inner.original.providers.ollama.base_url.clone(),
        name,
        inner.pull_slot.clone(),
        cancel.clone(),
        inner.alive.clone(),
    ) {
        set_pull_status(inner.hwnd, "Could not start the download.");
        return;
    }
    inner.pull_cancel = Some(cancel);
    if let Some(button) = get_dlg_item(inner.hwnd, ID_PULL_BUTTON) {
        set_text(button, "Stop");
    }
    if let Some(edit) = get_dlg_item(inner.hwnd, ID_PULL_NAME) {
        unsafe {
            let _ = EnableWindow(edit, false);
        }
    }
    set_pull_status(inner.hwnd, "Starting...");
}

fn set_pull_status(hwnd: HWND, text: &str) {
    if let Some(status) = get_dlg_item(hwnd, ID_PULL_STATUS) {
        set_text(status, text);
    }
}

/// Back to the resting Pull row: the button says Pull, the name box is
/// usable, and the status line says `text`.
fn set_pull_idle(hwnd: HWND, text: &str) {
    if let Some(button) = get_dlg_item(hwnd, ID_PULL_BUTTON) {
        set_text(button, "Pull");
    }
    if let Some(edit) = get_dlg_item(hwnd, ID_PULL_NAME) {
        unsafe {
            let _ = EnableWindow(edit, true);
        }
    }
    set_pull_status(hwnd, text);
}

/// Shows the worker's latest line. When the pull is over, the row goes back
/// to idle and, on success, the model list is fetched again so the new model
/// appears in the combo.
fn apply_pull_update(inner: &mut SettingsInner) {
    // A stopped pull has already reset the row; ignore a late post.
    if inner.pull_cancel.is_none() {
        return;
    }
    let update = inner
        .pull_slot
        .lock()
        .ok()
        .map(|g| g.clone())
        .unwrap_or_default();
    if !update.finished {
        set_pull_status(inner.hwnd, &update.text);
        return;
    }
    inner.pull_cancel = None;
    set_pull_idle(inner.hwnd, &update.text);
    if update.succeeded {
        inner.pull_select = update.model;
        spawn_ollama_models_fetch(
            inner.hwnd,
            inner.original.providers.ollama.base_url.clone(),
            inner.ollama_tags.clone(),
            inner.alive.clone(),
        );
    }
}

/// Pulls `name` on a worker thread (#566): the thread exists only while the
/// download runs. Progress goes into `slot` and wakes the window with
/// [`WM_OLLAMA_PULL`], only while the window is open and the pull has not
/// been stopped (`alive`, `cancel`). Returns false if the thread could not
/// start.
fn spawn_ollama_pull(
    hwnd: HWND,
    generation: usize,
    base_url: String,
    name: String,
    slot: PullSlot,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    alive: std::sync::Arc<std::sync::atomic::AtomicBool>,
) -> bool {
    let hwnd_raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("settings-ollama-pull".into())
        .spawn(move || {
            let publish = |update: PullUpdate| {
                if let Ok(mut guard) = slot.lock() {
                    *guard = update;
                }
                if cancel.load(Ordering::SeqCst) {
                    return;
                }
                post_while_open(&alive, || unsafe {
                    let _ = PostMessageW(
                        Some(HWND(hwnd_raw as *mut c_void)),
                        WM_OLLAMA_PULL,
                        WPARAM(generation),
                        LPARAM(0),
                    );
                });
            };
            let mut last_text = String::new();
            let mut last_post = std::time::Instant::now() - Duration::from_secs(1);
            let result = ollama_admin::pull(&base_url, &name, &cancel, |p| {
                let text = p.describe();
                if text == last_text || last_post.elapsed().as_millis() < PULL_POST_INTERVAL_MS {
                    return;
                }
                last_text = text.clone();
                last_post = std::time::Instant::now();
                publish(PullUpdate {
                    text,
                    finished: false,
                    succeeded: false,
                    model: None,
                });
            });
            if cancel.load(Ordering::SeqCst) {
                return;
            }
            publish(match result {
                Ok(()) => PullUpdate {
                    text: format!(
                        "Pulled {}. It is now in the model list.",
                        cap_text(&name, 80)
                    ),
                    finished: true,
                    succeeded: true,
                    model: Some(name.clone()),
                },
                Err(e) => PullUpdate {
                    text: pull_error_text(&e),
                    finished: true,
                    succeeded: false,
                    model: None,
                },
            });
        })
        .is_ok()
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
    /// #382: the button is "Fix it" (Ollama is on the CPU of an Intel GPU
    /// machine), not a link.
    fix: bool,
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
                fix: false,
            }
        } else {
            OllamaSection {
                text: "Ollama runs AI models on this PC, free and offline.".to_string(),
                link: Some(("Get Ollama", OLLAMA_DOWNLOAD_URL)),
                fix: false,
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
        fix: false,
    }
}

/// #604: when the stock Ollama tray app is the listener, the status text
/// already says what to do (quit it, start Ollama yourself), so the Local
/// models pointer is dropped rather than offering docs for the wrong server.
fn without_link_for_stock_tray(mut section: OllamaSection, stock_tray: bool) -> OllamaSection {
    if stock_tray {
        section.link = None;
    }
    section
}

/// #382: the text of the "Fix it" offer, or `None` when it is not offered:
/// an Intel GPU is present (DXGI vendor id), Ollama is on the CPU, the server
/// is on this machine (the fix stops local processes), no fix is already
/// running, and this is not portable mode (#130 makes no registry writes).
fn gpu_fix_offer(
    listening: bool,
    stock_tray: bool,
    gpu: Option<GpuStatus>,
    base_url: &str,
) -> Option<String> {
    let kind = match (listening, stock_tray) {
        (false, _) => None,
        (true, true) => Some(ListenerKind::StockTrayServer),
        (true, false) => Some(ListenerKind::Other),
    };
    // Cheap checks first: only enumerate adapters when it could matter.
    if !ollama_gpu_fix::should_offer_fix(true, kind, gpu)
        || !ollama_gpu_fix::is_loopback_base_url(base_url)
        || ollama_gpu_fix::is_running()
        || crate::portable::is_portable()
    {
        return None;
    }
    if !ollama_gpu_fix::has_intel_gpu(&ollama_gpu_fix::adapter_vendor_ids()) {
        return None;
    }
    Some(ollama_gpu_fix::offer_text(
        ollama_gpu_fix::current_user_value().as_deref(),
    ))
}

/// #382: swaps the section for the fix offer: the text says what the button
/// will do, and the button is "Fix it" instead of a link.
fn with_gpu_fix(mut section: OllamaSection, offer: Option<String>) -> OllamaSection {
    if let Some(text) = offer {
        section.text = text;
        section.link = None;
        section.fix = true;
    }
    section
}

/// #382: the "Fix it" click. Runs [`ollama_gpu_fix::run_fix`] on a worker
/// (it stops and starts Ollama and can take a minute), showing each step in
/// the status text through the same slot and message the probe uses. The
/// button is hidden while it runs and comes back only if the result says to
/// try again. Not run in tests: it kills real processes.
fn start_gpu_fix(inner: &mut SettingsInner) {
    inner.ollama_fix = false;
    // Hidden first, so a refusal below never leaves a dead "Fix it".
    inner.ollama_link_shown = false;
    unsafe { sync_visibility(inner) };
    let cfg = inner.original.providers.ollama.clone();
    // Refuse before touching anything, and say so inline (rule 7).
    let refusal = if !ollama_gpu_fix::is_loopback_base_url(&cfg.base_url) {
        Some("The fix only works on an Ollama running on this PC.")
    } else {
        None
    };
    let guard = if refusal.is_none() {
        ollama_gpu_fix::try_begin()
    } else {
        None
    };
    if guard.is_none() {
        let msg = refusal.unwrap_or("A fix is already running.");
        if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
            set_text(status, msg);
        }
        return;
    }
    if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
        set_text(status, ollama_gpu_fix::RUNNING_TEXT);
    }
    let slot = inner.ollama_status.clone();
    let alive = inner.alive.clone();
    let hwnd_raw = inner.hwnd.0 as isize;
    let post = move |slot: &OllamaStatusSlot, section: OllamaSection| {
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
    };
    let spawned = std::thread::Builder::new()
        .name("settings-ollama-gpu-fix".into())
        .spawn(move || {
            let _guard = guard;
            let mut sys = ollama_gpu_fix::RealSystem {
                port: ollama_admin::port_from_base_url(&cfg.base_url).unwrap_or(11434),
                base_url: cfg.base_url.clone(),
                model: cfg.model.clone(),
            };
            let mut step = |line: &str| {
                post(
                    &slot,
                    OllamaSection {
                        text: format!("Fixing: {line}"),
                        link: None,
                        fix: false,
                    },
                )
            };
            let outcome = ollama_gpu_fix::run_fix(
                &mut sys,
                ollama_gpu_fix::default_exe(),
                &ollama_gpu_fix::trusted_roots(),
                &mut step,
            );
            post(
                &slot,
                OllamaSection {
                    text: outcome.message(),
                    link: None,
                    fix: outcome.retryable(),
                },
            );
        });
    if spawned.is_err() {
        // Rule 7: say so inline. Nothing was touched.
        if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
            set_text(status, "Could not start the fix. Nothing was changed.");
        }
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

/// The second shortcut's line (#823): named by what it is for, since the
/// Copilot key line above it already says which key is the first.
fn secondary_label(chord: &crate::hotkey::Chord) -> String {
    format!("Shortcut for any keyboard: {}", chord_to_string(chord))
}

/// The Mode combo's rows: the tray submenu's four, in its order, labelled by
/// `Mode::label` so the two cannot drift (#873).
const MODE_CHOICES: [crate::mode::Mode; 4] = [
    crate::mode::Mode::Cloud,
    crate::mode::Mode::Local,
    crate::mode::Mode::Auto,
    crate::mode::Mode::Offline,
];

/// The combo row for `mode`.
fn mode_index(mode: crate::mode::Mode) -> usize {
    MODE_CHOICES.iter().position(|m| *m == mode).unwrap_or(0)
}

/// The mode on combo row `index`; `None` for CB_ERR (-1) or a row past the list.
fn mode_at(index: isize) -> Option<crate::mode::Mode> {
    usize::try_from(index)
        .ok()
        .and_then(|i| MODE_CHOICES.get(i).copied())
}

/// The line beside the Pause buttons (#873). `paused` is the tray's own wording
/// of the state (`pause::tooltip_text`), or `None` while running.
fn pause_status_line(paused: Option<&str>) -> String {
    match paused {
        None => "Wingman is running.".to_string(),
        Some(text) => format!("{text}."),
    }
}

/// Which of (Pause, Resume) can be used: one or the other, as in the tray menu.
fn pause_buttons_enabled(paused: bool) -> (bool, bool) {
    (!paused, paused)
}

/// What the Pause button offers, in the tray's order and words.
const PAUSE_MENU: [(crate::pause::PauseChoice, &str); 3] = [
    (crate::pause::PauseChoice::OneHour, "1 hour"),
    (crate::pause::PauseChoice::UntilTomorrow, "Until tomorrow"),
    (crate::pause::PauseChoice::UntilResumed, "Until resumed"),
];

/// The pause shortcut's line; it may be unset (#873).
fn pause_label(chord: &Option<crate::hotkey::Chord>) -> String {
    match chord {
        Some(c) => format!("Pause shortcut: {}", chord_to_string(c)),
        None => format!(
            "Pause shortcut: {}",
            crate::hotkey::pause_binding_label(None)
        ),
    }
}

/// Shows the app's current mode and pause state in the open Settings window, so
/// it follows a change made from the tray (#873). `paused` is the tray's wording
/// of the pause (`None` while running). A no-op when no window is open. Must run
/// on the UI thread.
pub fn sync_live(mode: crate::mode::Mode, paused: Option<&str>) {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    if open != 0 {
        apply_live(HWND(open as *mut c_void), mode, paused);
    }
}

/// [`sync_live`] on a given window. `CB_SETCURSEL` sends no notification, so this
/// never echoes back to the app as a new choice.
fn apply_live(hwnd: HWND, mode: crate::mode::Mode, paused: Option<&str>) {
    if let Some(combo) = get_dlg_item(hwnd, ID_MODE) {
        unsafe {
            SendMessageW(
                combo,
                CB_SETCURSEL,
                Some(WPARAM(mode_index(mode))),
                Some(LPARAM(0)),
            );
        }
    }
    if let Some(line) = get_dlg_item(hwnd, ID_PAUSE_STATUS) {
        let text = pause_status_line(paused);
        if get_text(line) != text {
            set_text(line, &text);
        }
    }
    let (can_pause, _) = pause_buttons_enabled(paused.is_some());
    let pause = get_dlg_item(hwnd, ID_PAUSE_BUTTON);
    let resume = get_dlg_item(hwnd, ID_RESUME_BUTTON);
    if let (Some(pause), Some(resume)) = (pause, resume) {
        unsafe {
            // A button that is switched off while it has the keyboard focus
            // would strand the focus: hand it to the one that takes over.
            let (off, on) = if can_pause {
                (resume, pause)
            } else {
                (pause, resume)
            };
            let had_focus = GetFocus() == off;
            let _ = EnableWindow(on, true);
            let _ = EnableWindow(off, false);
            if had_focus {
                let _ = SetFocus(Some(on));
            }
        }
    }
}

/// Asks the app to do `command` (#873). The app runs the handler the tray menu
/// item runs. A window with no owner (the tests') asks nobody.
fn send_command(inner: &SettingsInner, command: crate::app::SettingsCommand) {
    send_command_to(inner.hwnd, inner.owner, command);
}

/// [`send_command`] with the two handles copied out, for code that holds no
/// borrow of the window's state.
fn send_command_to(hwnd: HWND, owner: HWND, command: crate::app::SettingsCommand) {
    if owner.0.is_null() {
        return;
    }
    if !crate::app::post_settings_command(owner.0 as isize, command) {
        // Rule 7: say so inline, never a silent no-op.
        if let Some(msg) = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE) {
            set_text(msg, "Wingman's main window is not running.");
        }
    }
}

/// Posted by the Pause button to the window itself. The menu's own message
/// loop must not run inside `handle_command`, which holds `&mut SettingsInner`:
/// a Quit during the menu can free that state, and other messages handled in the
/// loop take their own borrow. The window procedure runs it before it borrows.
const WM_PAUSE_MENU: u32 = 0x8000 + 0x873;

/// The Pause button's menu: a small menu under it with the three choices; the pick
/// is sent to the app. Dismissing the menu does nothing. Holds no borrow of the
/// window's state: the owner is copied first, and nothing of the state is read
/// after the menu, which may have outlived the window.
unsafe fn run_pause_menu(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, IsWindow, TrackPopupMenu, MF_STRING,
        TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN,
    };
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner;
    if state.is_null() {
        return;
    }
    let owner = (*state).owner;
    let Some(button) = get_dlg_item(hwnd, ID_PAUSE_BUTTON) else {
        return;
    };
    let Ok(menu) = (unsafe { CreatePopupMenu() }) else {
        return;
    };
    for (i, (_, label)) in PAUSE_MENU.iter().enumerate() {
        let text = wide_z(label);
        unsafe {
            let _ = AppendMenuW(menu, MF_STRING, i + 1, PCWSTR(text.as_ptr()));
        }
    }
    let mut at = windows::Win32::Foundation::RECT::default();
    let picked = unsafe {
        let _ = GetWindowRect(button, &mut at);
        TrackPopupMenu(
            menu,
            TPM_RETURNCMD | TPM_LEFTALIGN | TPM_TOPALIGN,
            at.left,
            at.bottom,
            None,
            hwnd,
            None,
        )
    };
    unsafe {
        let _ = DestroyMenu(menu);
    }
    // A Quit while the menu was up closed the window: say nothing to it.
    if !IsWindow(Some(hwnd)).as_bool() {
        return;
    }
    let choice = usize::try_from(picked.0)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .and_then(|i| PAUSE_MENU.get(i));
    if let Some((choice, _)) = choice {
        send_command_to(hwnd, owner, crate::app::SettingsCommand::Pause(*choice));
    }
}

/// Width of the "Open config file" button, in dp (#823).
const OPEN_CONFIG_W: i32 = 120;

/// Shown on the validation line when the config file cannot be opened (rule 7:
/// never a silent no-op, never a dialog).
const OPEN_CONFIG_FAILED: &str = "Could not open the config file. Save once to create it.";

/// Opens the config file in the user's default editor. A missing file or a
/// refusal from the shell is said on the validation line.
fn open_config_file(inner: &SettingsInner) {
    let opened = match Config::path() {
        Ok(path) if path.exists() => open_url(&path.to_string_lossy()),
        _ => false,
    };
    if !opened {
        if let Some(msg) = get_dlg_item(inner.hwnd, ID_VALIDATION_MESSAGE) {
            set_text(msg, OPEN_CONFIG_FAILED);
        }
    }
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
fn ollama_status_line(
    cfg: &OllamaConfig,
    ollama_configured: bool,
) -> (String, bool, bool, Option<GpuStatus>) {
    let port = ollama_admin::port_from_base_url(&cfg.base_url).unwrap_or(11434);
    let health = ollama_admin::query_ollama_health(port);
    let health_msg = health_message(&health);
    let listening = matches!(health, OllamaHealth::Listening { .. });
    // #604: the third value, whether the listener is the stock tray app.
    let stock = matches!(
        health,
        OllamaHealth::Listening {
            kind: ListenerKind::StockTrayServer,
            ..
        }
    );

    if !should_query_ollama_details(ollama_configured, &health) {
        return (health_msg, listening, stock, None);
    }

    let budget_start = std::time::Instant::now();
    let (gpu, gpu_line) =
        match ollama_admin::ps_with_timeout(&cfg.base_url, OLLAMA_STATUS_HTTP_BUDGET) {
            Ok(entries) => {
                let gpu = ollama_admin::gpu_status_for(&entries, &cfg.model);
                let line = match gpu {
                    GpuStatus::Gpu => format!("{} is loaded on GPU.", cfg.model),
                    GpuStatus::Cpu => format!("{} is loaded on CPU.", cfg.model),
                    GpuStatus::NotLoaded => format!("{} is not loaded yet.", cfg.model),
                };
                (gpu, line)
            }
            Err(_) => return (health_msg, listening, stock, None),
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
        stock,
        Some(gpu),
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
    /// The navigation rail (#872).
    rail: HWND,
    /// See [`SettingsInner::page_content_dp`].
    page_content_dp: [i32; 5],
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
    // #872: the rail is the first tab stop, then the body (#481); the footer
    // controls made last are the last stops.
    let rail = create_rail(hwnd, instance, dpi, font);
    let body = create_body(hwnd, instance, dpi);
    let ctx = Ctx {
        parent: std::cell::Cell::new(if body.0.is_null() { hwnd } else { body }),
        page: std::cell::Cell::new(None),
        instance,
        dpi,
        font,
        children: std::cell::RefCell::new(Vec::new()),
    };
    let content_x = MARGIN;
    let content_w = PAGE_W_DP - 2 * MARGIN;

    // Every page stacks its groups from the top; these follow where the next
    // group of each page starts and where its last one ended (dp).
    let mut page_y = [PAGE_TOP_PAD_DP; 5];
    let mut page_bottom = [0i32; 5];
    let mut y;

    // -- Providers (Models) --------------------------------------------
    ctx.page.set(Some(Page::Models));
    y = page_y[Page::Models.index()];
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
    combo_select_index(active_combo, combo_index_for_order(&config.providers.order));
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
        WS_EX_CLIENTEDGE,
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

    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let openai_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 8,
        ID_OPENAI_MODEL,
    );
    fill_model_combo(
        openai_model,
        &config.providers.openai.models,
        &config.providers.openai.model,
    );
    r.advance();

    ctx.create(
        WC_STATIC,
        "Thinking time:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let openai_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
        WS_EX_CLIENTEDGE,
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

    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let anthropic_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 8,
        ID_ANTHROPIC_MODEL,
    );
    fill_model_combo(
        anthropic_model,
        &config.providers.anthropic.models,
        &config.providers.anthropic.model,
    );
    r.advance();

    ctx.create(
        WC_STATIC,
        "Thinking time:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let anthropic_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
        WS_EX_CLIENTEDGE,
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

    ctx.create(WC_STATIC, "Model:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let gemini_model = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 8,
        ID_GEMINI_MODEL,
    );
    fill_model_combo(
        gemini_model,
        &config.providers.gemini.models,
        &config.providers.gemini.model,
    );
    r.advance();

    ctx.create(
        WC_STATIC,
        "Thinking time:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        ID_GEMINI_EFFORT_LABEL,
    );
    let gemini_effort = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
    sync_gemini_thinking_enabled(hwnd);
    r.advance();

    // Ollama (#183): no key, only which pulled model to use. The combo starts
    // with the configured model alone; the pulled models are added when the
    // background fetch reports in (see `spawn_ollama_models_fetch`).
    // #427: the cloud model dropdowns list what the provider offers (or
    // what is in config.toml), so say where to add one that is missing.
    // Ollama's list comes from what is pulled, so this sits above its row.
    ctx.create(
        WC_STATIC,
        model_list_hint_for(config.mode),
        0,
        0,
        r.x,
        r.y,
        r.w - OPEN_CONFIG_W - 8,
        ROW_H,
        ID_MODEL_LIST_HINT,
    );
    ctx.create(
        WC_BUTTON,
        "Open config file",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.x + r.w - OPEN_CONFIG_W,
        r.y,
        OPEN_CONFIG_W,
        ROW_H,
        ID_OPEN_CONFIG,
    );
    r.advance();

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

    // `r` already moved past the last row and its gap (#785).
    let providers_bottom = r.y + 4;
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
    page_y[Page::Models.index()] = y;
    page_bottom[Page::Models.index()] = providers_bottom;

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
    y = page_y[Page::Models.index()];
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
    // #566: pull a model by name. The name box, a Pull/Stop button, and one
    // line for progress or the reason it failed. No list of suggested
    // models: the user types the name (see docs/providers.md).
    let pull_x = content_x + MARGIN;
    let pull_inner_w = content_w - 2 * MARGIN;
    ctx.create(
        WC_STATIC,
        "Pull model:",
        0,
        0,
        pull_x,
        y + 3,
        LABEL_W,
        ROW_H,
        0,
    );
    ctx.create(
        WC_EDIT,
        "",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
        pull_x + LABEL_W,
        y,
        pull_inner_w - LABEL_W - OLLAMA_PULL_BTN_W - 8,
        ROW_H + 4,
        ID_PULL_NAME,
    );
    ctx.create(
        WC_BUTTON,
        "Pull",
        BS_PUSHBUTTON as u32,
        0,
        pull_x + pull_inner_w - OLLAMA_PULL_BTN_W,
        y - 1,
        OLLAMA_PULL_BTN_W,
        BTN_H,
        ID_PULL_BUTTON,
    );
    y += BTN_H + 4;
    ctx.create(
        WC_STATIC,
        "",
        0,
        0,
        pull_x,
        y,
        pull_inner_w,
        ROW_H * 2,
        ID_PULL_STATUS,
    );
    y += ROW_H * 2;
    let ollama_bottom = y + 4;
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
    page_y[Page::Models.index()] = y;
    page_bottom[Page::Models.index()] = ollama_bottom;

    // -- Hotkeys (General) --------------------------------------------------------
    ctx.page.set(Some(Page::General));
    y = page_y[Page::General.index()];
    let hotkeys_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);
    let change_w = 90;
    let label_w = r.w - change_w - 12;
    // #873: Mode and Pause act at once, through the tray's own handlers.
    ctx.create(WC_STATIC, "Mode:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let mode_combo = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 5,
        ID_MODE,
    );
    for m in MODE_CHOICES {
        combo_add(mode_combo, m.label());
    }
    combo_select(mode_combo, config.mode.label());
    r.advance();
    let paused_now = crate::pause::is_paused_now();
    let (can_pause, can_resume) = pause_buttons_enabled(paused_now);
    ctx.create(
        WC_STATIC,
        &pause_status_line(paused_now.then_some("Paused")),
        0,
        0,
        r.x,
        r.y,
        label_w - change_w - 8,
        ROW_H,
        ID_PAUSE_STATUS,
    );
    let pause_button = ctx.create(
        WC_BUTTON,
        "Pause...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.x + r.w - 2 * change_w - 8,
        r.y,
        change_w,
        ROW_H,
        ID_PAUSE_BUTTON,
    );
    let resume_button = ctx.create(
        WC_BUTTON,
        "Resume",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.x + r.w - change_w,
        r.y,
        change_w,
        ROW_H,
        ID_RESUME_BUTTON,
    );
    unsafe {
        let _ = EnableWindow(pause_button, can_pause);
        let _ = EnableWindow(resume_button, can_resume);
    }
    r.advance();
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
        &secondary_label(&config.hotkeys.secondary),
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
    // #873: the pause shortcut, moved here from the tray menu.
    ctx.create(
        WC_STATIC,
        &pause_label(&config.hotkeys.pause),
        0,
        0,
        r.x,
        r.y,
        label_w,
        ROW_H,
        ID_PAUSE_LABEL,
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
        ID_CHANGE_PAUSE,
    );
    r.advance();
    // The learn flow's whole conversation happens on this one line, inline:
    // never a dialog and never a card (rule 7).
    ctx.create(
        WC_STATIC,
        "Click Change, then press the new key.",
        0,
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
    // Portable mode (#130) makes no registry writes, so the switch is greyed.
    if crate::portable::is_portable() {
        unsafe {
            let _ = EnableWindow(autostart, false);
        }
    }
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
    page_y[Page::General.index()] = y;
    page_bottom[Page::General.index()] = hotkeys_bottom;

    // -- Capture (Privacy) ----------------------------------------------------------
    ctx.page.set(Some(Page::Privacy));
    y = page_y[Page::Privacy.index()];
    let capture_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);
    ctx.create(
        WC_STATIC,
        "Screenshot detail:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let max_edge = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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

    r.advance();

    ctx.create(WC_STATIC, "Screen:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let monitor = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
        ID_CAPTURE_DETAIL_HINT,
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
        r.x + CHECKBOX_TEXT_INDENT_DP,
        r.y,
        r.w - CHECKBOX_TEXT_INDENT_DP,
        ROW_H * 2,
        ID_REDACT_HINT,
    );
    // The note's own two rows, then the group's usual bottom padding (#785).
    r.y += ROW_H * 2;
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
    page_y[Page::Privacy.index()] = y;
    page_bottom[Page::Privacy.index()] = capture_bottom;

    // -- Card (General) ---------------------------------------------------------
    ctx.page.set(Some(Page::General));
    y = page_y[Page::General.index()];
    let card_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);

    ctx.create(
        WC_STATIC,
        CARD_SECONDS_LABEL,
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
        &config.ui.card_seconds.to_string(),
        (ES_NUMBER | ES_AUTOHSCROLL) as u32,
        WS_EX_CLIENTEDGE,
        r.x + LABEL_W,
        r.y,
        CARD_SECONDS_EDIT_W,
        ROW_H,
        ID_CARD_SECONDS,
    );
    ctx.create(
        WC_STATIC,
        CARD_SECONDS_UNIT,
        0,
        0,
        r.x + LABEL_W + CARD_SECONDS_EDIT_W + 8,
        r.y,
        r.w - LABEL_W - CARD_SECONDS_EDIT_W - 8,
        ROW_H,
        0,
    );
    r.advance();

    ctx.create(WC_STATIC, "Text scale:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let track = ctx.create(
        TRACKBAR_CLASS_STR,
        "",
        TBS_HORZ as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W - 60,
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
    ctx.create(WC_STATIC, "Card corner:", 0, 0, r.x, r.y, LABEL_W, ROW_H, 0);
    let corner = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
        ROW_H * 7,
        ID_CARD_CORNER,
    );
    for label in CARD_CORNER_LABELS {
        combo_add(corner, label);
    }
    combo_select(corner, card_corner_label(config.ui.card_corner));
    r.advance();

    ctx.create(
        WC_STATIC,
        "Card monitor:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        ID_CARD_MONITOR_LABEL,
    );
    let card_monitor = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
    page_y[Page::General.index()] = y;
    page_bottom[Page::General.index()] = card_bottom;

    // -- Forms (#403, Privacy) -------------------------------------------------
    ctx.page.set(Some(Page::Privacy));
    y = page_y[Page::Privacy.index()];
    let forms_top = y;
    y += GROUP_LABEL_TOP;
    let mut r = Rows::new(content_x + MARGIN, y, content_w - 2 * MARGIN);

    ctx.create(
        WC_STATIC,
        "Review before filling:",
        0,
        0,
        r.x,
        r.y,
        LABEL_W,
        ROW_H,
        0,
    );
    let require_tick_for = ctx.create(
        WC_COMBOBOX,
        "",
        (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32,
        0,
        r.x + LABEL_W,
        r.y,
        r.w - LABEL_W,
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
    page_bottom[Page::Privacy.index()] = forms_bottom;

    // -- Actions (#872): no setting yet, so one note, never an empty page ---
    ctx.page.set(Some(Page::Actions));
    y = page_y[Page::Actions.index()];
    let actions_top = y;
    y += GROUP_LABEL_TOP;
    let note_h = ROW_H * 3;
    ctx.create(
        WC_STATIC,
        ACTIONS_NOTE,
        0,
        0,
        content_x + MARGIN,
        y,
        content_w - 2 * MARGIN,
        note_h,
        ID_ACTIONS_NOTE,
    );
    y += note_h;
    let actions_bottom = y + 4;
    ctx.create(
        WC_BUTTON,
        "Your actions",
        BS_GROUPBOX as u32,
        0,
        content_x,
        actions_top,
        content_w,
        actions_bottom - actions_top,
        0,
    );
    page_bottom[Page::Actions.index()] = actions_bottom;

    // -- Prompt (Advanced; grows to fill the space above the button row) ----
    ctx.page.set(Some(Page::Advanced));
    y = page_y[Page::Advanced.index()];
    let footer = footer_layout(y, win_h_dp, false);
    let prompt_top = footer.prompt_top;
    let prompt_bottom = footer.prompt_bottom;
    y = prompt_top + GROUP_LABEL_TOP;

    let reset_btn_h = BTN_H;
    let prompt_edit_h = footer.prompt_edit_h;
    let content_top_dp = footer.prompt_top;
    let prompt_edit = ctx.create(
        WC_EDIT,
        &prompt_to_edit(&config.ui.prompt),
        (ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32,
        WS_EX_CLIENTEDGE,
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
    // It belongs to no page and sits under the pages, right of the rail.
    ctx.parent.set(hwnd);
    ctx.page.set(None);
    let buttons_y = footer.buttons_y;
    // #344: inline "that field is wrong" message, blank until a Save
    // attempt fails validation. Sits to the left of the Save/Cancel row so
    // it never overlaps either the Prompt group or the buttons.
    let validation = ctx.create(
        WC_STATIC,
        "",
        0x4000, // SS_ENDELLIPSIS
        0,
        RAIL_W_DP + content_x + MARGIN,
        buttons_y + (BTN_H - ROW_H) / 2,
        content_w - 2 * MARGIN - 2 * BTN_W - 12 - 12,
        ROW_H,
        ID_VALIDATION_MESSAGE,
    );
    ctx.set_anchor(validation, Anchor::Footer);
    // #778: a key an environment variable overrides cannot be edited here.
    lock_env_key_fields(hwnd);
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
        ID_MODE,
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
        ID_MODE,
        ID_CARD_SECONDS,
        ID_TEXT_SCALE_TRACK,
        ID_CARD_CORNER,
        ID_CARD_MONITOR,
        ID_REQUIRE_TICK_FOR,
        ID_OLLAMA_LINK,
        ID_PULL_NAME,
        ID_PULL_BUTTON,
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

    let mut page_content_dp = [0i32; 5];
    for page in Page::ALL {
        let i = page.index();
        page_content_dp[i] = if page == Page::Advanced {
            footer.content_h()
        } else {
            page_bottom[i] + BODY_PAD_BOTTOM
        };
    }

    BuiltUi {
        prompt_edit,
        rail,
        page_content_dp,
        body,
        children: ctx.children.into_inner(),
        content_top_dp,
        win_h_dp,
    }
}

// ---------------------------------------------------------------------------
// The navigation rail (#872)
// ---------------------------------------------------------------------------
// A single-select list box, owner-drawn so it looks like the Windows 11
// Settings navigation: an icon and a name per page, the selected one on a
// soft fill with a short accent pill. The list box keeps the arrow keys, Home,
// End, type-ahead and the list and selection accessibility patterns; only the
// painting is ours.

const LB_ADDSTRING: u32 = 0x0180;
const LB_SETCURSEL: u32 = 0x0186;
const LB_GETCURSEL: u32 = 0x0188;
const LB_SETITEMHEIGHT: u32 = 0x01A0;
const LBN_SELCHANGE: u32 = 1;
const LBS_NOTIFY: u32 = 0x0001;
const LBS_OWNERDRAWFIXED: u32 = 0x0010;
const LBS_HASSTRINGS: u32 = 0x0040;
const LBS_NOINTEGRALHEIGHT: u32 = 0x0100;
const ODS_SELECTED: u32 = 0x0001;
const ODS_FOCUS: u32 = 0x0010;
const ODS_NOFOCUSRECT: u32 = 0x0200;
const WC_LISTBOX: &str = "LISTBOX";

/// Posted to the window to change the page or move focus between the rail and
/// the page. `wparam` says what (the `PAGE_*` codes), `lparam` is its argument.
const WM_SETTINGS_PAGE: u32 = 0x8000 + 45; // WM_APP + 45
/// `lparam` is the page's rail position.
const PAGE_PICK: usize = 0;
/// `lparam` is how many pages to move (negative goes back), wrapping.
const PAGE_STEP: usize = 1;
/// Focus goes into the page (Enter on the rail).
const PAGE_ENTER: usize = 2;
/// Focus goes to the rail, or into the page when it is already there (F6).
const PAGE_SWAP_FOCUS: usize = 3;

/// What a key does to the pages, apart from the rail's own arrow keys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PageKey {
    Pick(Page),
    Step(isize),
}

/// The page command for a key press: Ctrl+1 to Ctrl+5 pick a page, Ctrl+PageDown
/// and Ctrl+PageUp move to the next and the previous. Nothing without Ctrl, so
/// typing in a box is never taken. AltGr reads as Ctrl+Alt, so with Alt down
/// the key is left alone: AltGr+2 and AltGr+3 type @ and # on some layouts.
fn page_key(vk: u32, ctrl: bool, alt: bool) -> Option<PageKey> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{VK_NEXT, VK_PRIOR};
    if !ctrl || alt {
        return None;
    }
    if vk == VK_NEXT.0 as u32 {
        return Some(PageKey::Step(1));
    }
    if vk == VK_PRIOR.0 as u32 {
        return Some(PageKey::Step(-1));
    }
    Page::from_digit_key(vk).map(PageKey::Pick)
}

/// Creates the rail, filled and with the first page selected.
fn create_rail(parent: HWND, instance: HINSTANCE, dpi: u32, font: HFONT) -> HWND {
    // A list box has no name of its own (MEASURED 2026-10-17, UI Automation:
    // the window text is ignored), but the static before a control names it.
    // This one is a pixel, so it takes no room and is read as the list's name.
    let static_class = wide_z(WC_STATIC);
    let name = wide_z(RAIL_NAME);
    unsafe {
        let _ = CreateWindowExW(
            Default::default(),
            PCWSTR(static_class.as_ptr()),
            PCWSTR(name.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(WS_CHILD.0 | WS_VISIBLE.0),
            0,
            0,
            1,
            1,
            Some(parent),
            None,
            Some(instance),
            None,
        );
    }
    let class = wide_z(WC_LISTBOX);
    let title = wide_z(RAIL_NAME);
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_TABSTOP.0
        | WS_GROUP.0
        | LBS_NOTIFY
        | LBS_OWNERDRAWFIXED
        | LBS_HASSTRINGS
        | LBS_NOINTEGRALHEIGHT;
    let rail = unsafe {
        CreateWindowExW(
            Default::default(),
            PCWSTR(class.as_ptr()),
            PCWSTR(title.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            0,
            0,
            to_px(RAIL_W_DP, dpi),
            to_px(RAIL_ITEM_H_DP, dpi) * Page::ALL.len() as i32,
            Some(parent),
            Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                ID_RAIL as *mut c_void,
            )),
            Some(instance),
            None,
        )
    }
    .unwrap_or(HWND(std::ptr::null_mut()));
    if rail.0.is_null() {
        return rail;
    }
    for page in Page::ALL {
        let text = wide_z(page.label());
        unsafe {
            SendMessageW(
                rail,
                LB_ADDSTRING,
                None,
                Some(LPARAM(text.as_ptr() as isize)),
            );
        }
    }
    unsafe {
        SendMessageW(
            rail,
            LB_SETCURSEL,
            Some(WPARAM(Page::General.index())),
            None,
        );
    }
    place_rail(rail, dpi, font);
    rail
}

/// Sizes and fonts the rail for `dpi`: the left edge of the window, a little
/// below the top, exactly five items tall.
fn place_rail(rail: HWND, dpi: u32, font: HFONT) {
    let item_h = to_px(RAIL_ITEM_H_DP, dpi);
    unsafe {
        let _ = SetWindowPos(
            rail,
            None,
            0,
            to_px(RAIL_TOP_PAD_DP, dpi),
            to_px(RAIL_W_DP, dpi),
            item_h * Page::ALL.len() as i32,
            SWP_NOZORDER | SWP_NOACTIVATE,
        );
        SendMessageW(
            rail,
            LB_SETITEMHEIGHT,
            Some(WPARAM(0)),
            Some(LPARAM(item_h as isize)),
        );
        SendMessageW(
            rail,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
}

/// The colours of one rail item.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RailColors {
    /// The item's whole rectangle: the window's face colour.
    face: u32,
    /// The rounded selection fill, when the item is selected.
    fill: Option<u32>,
    /// The name and icon.
    text: u32,
    /// The accent pill, when the item is selected.
    pill: Option<u32>,
}

/// The colours of an item. High contrast is system colours only: the selected
/// item is `Highlight` with `HighlightText` on it, the pill the same text
/// colour. Otherwise the selection is the 6 % tint every Windows 11 list uses
/// and the pill is the user's accent (`accent`).
fn rail_colors(
    selected: bool,
    high_contrast: bool,
    face: u32,
    tokens: &crate::ui::theme::Tokens,
    sys: &crate::ui::theme::SysColors,
    accent: u32,
) -> RailColors {
    if !selected {
        return RailColors {
            face,
            fill: None,
            text: tokens.text,
            pill: None,
        };
    }
    if high_contrast {
        return RailColors {
            face,
            fill: Some(sys.highlight),
            text: sys.highlight_text,
            pill: Some(sys.highlight_text),
        };
    }
    RailColors {
        face,
        fill: Some(crate::ui::card_buttons::blend(face, tokens.text, 6)),
        text: tokens.text,
        pill: Some(accent),
    }
}

/// Whether Segoe Fluent Icons is installed; looked up once.
fn fluent_icons_installed() -> bool {
    static FLUENT: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *FLUENT.get_or_init(|| font_face_exists(crate::ui::theme::FLUENT_ICON_FONT_FACE))
}

/// An icon font `dp` tall at `dpi`. The caller deletes it.
unsafe fn icon_font(dp: i32, dpi: u32) -> HFONT {
    let mut lf = LOGFONTW {
        lfHeight: -to_px(dp, dpi),
        lfWeight: FW_NORMAL.0 as i32,
        ..Default::default()
    };
    let face = crate::ui::theme::icon_font_face(fluent_icons_installed());
    for (i, u) in face.encode_utf16().enumerate() {
        if i < lf.lfFaceName.len() - 1 {
            lf.lfFaceName[i] = u;
        }
    }
    CreateFontIndirectW(&lf)
}

/// `WM_MEASUREITEM`: every rail item is the same height.
unsafe fn measure_rail_item(lparam: LPARAM, dpi: u32) {
    let mis = &mut *(lparam.0 as *mut windows::Win32::UI::Controls::MEASUREITEMSTRUCT);
    if mis.CtlID == ID_RAIL as u32 {
        mis.itemHeight = to_px(RAIL_ITEM_H_DP, dpi).max(1) as u32;
    }
}

/// `WM_DRAWITEM` for the rail: one item.
unsafe fn draw_rail_item(lparam: LPARAM) {
    use crate::ui::theme::{
        accent_fill, read_accent_shades, SysColors, Theme, Tokens, CONTROL_RADIUS_DP,
        FOCUS_LINE_DP, ICON_FONT_DP, RAIL_ICON_X_DP, RAIL_INSET_DP, RAIL_PILL_H_DP, RAIL_PILL_W_DP,
        RAIL_TEXT_X_DP,
    };
    use windows::Win32::Foundation::{COLORREF, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, FillRect, FrameRect, GetStockObject, GetSysColor, RoundRect,
        SelectObject, SetBkMode, SetTextColor, COLOR_BTNFACE, DT_END_ELLIPSIS, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, NULL_PEN, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::WM_GETFONT;

    let di = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
    if di.CtlID != ID_RAIL as u32 {
        return;
    }
    // An empty list asks for its focus rectangle with the item id -1.
    let Some(page) = Page::from_index(di.itemID as i32 as isize) else {
        return;
    };
    let hdc = di.hDC;
    let rc: RECT = di.rcItem;
    let dpi = dpi_for(di.hwndItem);
    let selected = di.itemState.0 & ODS_SELECTED != 0;

    let high_contrast = crate::ui::card_buttons::system_high_contrast().is_some();
    let sys = SysColors::read();
    let theme = Theme::resolve(true, high_contrast);
    let tokens = Tokens::for_theme(theme, &sys);
    let face = GetSysColor(COLOR_BTNFACE);
    let accent = if selected && !high_contrast {
        accent_fill(theme, read_accent_shades(), face)
    } else {
        face
    };
    let colors = rail_colors(selected, high_contrast, face, &tokens, &sys, accent);

    let brush = CreateSolidBrush(COLORREF(colors.face));
    FillRect(hdc, &rc, brush);
    let _ = DeleteObject(HGDIOBJ(brush.0));

    let inset = to_px(RAIL_INSET_DP, dpi);
    let pad_y = to_px(2, dpi);
    let fill_rc = RECT {
        left: rc.left + inset,
        top: rc.top + pad_y,
        right: rc.right - inset,
        bottom: rc.bottom - pad_y,
    };
    let radius = to_px(CONTROL_RADIUS_DP, dpi);
    let null_pen = GetStockObject(NULL_PEN);
    if let Some(fill) = colors.fill {
        let b = CreateSolidBrush(COLORREF(fill));
        if high_contrast {
            FillRect(hdc, &fill_rc, b);
        } else {
            let old_b = SelectObject(hdc, HGDIOBJ(b.0));
            let old_p = SelectObject(hdc, null_pen);
            let _ = RoundRect(
                hdc,
                fill_rc.left,
                fill_rc.top,
                fill_rc.right + 1,
                fill_rc.bottom + 1,
                radius * 2,
                radius * 2,
            );
            SelectObject(hdc, old_b);
            SelectObject(hdc, old_p);
        }
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
    if let Some(pill) = colors.pill {
        let b = CreateSolidBrush(COLORREF(pill));
        let old_b = SelectObject(hdc, HGDIOBJ(b.0));
        let old_p = SelectObject(hdc, null_pen);
        let h = to_px(RAIL_PILL_H_DP, dpi);
        let w = to_px(RAIL_PILL_W_DP, dpi);
        let top = rc.top + (rc.bottom - rc.top - h) / 2;
        let left = fill_rc.left;
        let _ = RoundRect(hdc, left, top, left + w + 1, top + h + 1, w * 2, w * 2);
        SelectObject(hdc, old_b);
        SelectObject(hdc, old_p);
        let _ = DeleteObject(HGDIOBJ(b.0));
    }

    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(colors.text));
    let flags = DT_SINGLELINE | DT_VCENTER | DT_LEFT | DT_NOPREFIX | DT_END_ELLIPSIS;
    // The icon.
    let icons = icon_font(ICON_FONT_DP, dpi);
    let old_font = SelectObject(hdc, HGDIOBJ(icons.0));
    let mut icon_rc = rc;
    icon_rc.left += to_px(RAIL_ICON_X_DP, dpi);
    icon_rc.right = rc.left + to_px(RAIL_TEXT_X_DP, dpi);
    let mut glyph = [0u8; 4];
    crate::ui::text::draw_text_line(hdc, page.glyph().encode_utf8(&mut glyph), icon_rc, flags);
    // The name, in the window's own font.
    let list_font = SendMessageW(di.hwndItem, WM_GETFONT, None, None).0;
    if list_font != 0 {
        SelectObject(hdc, HGDIOBJ(list_font as *mut c_void));
    }
    let mut text_rc = rc;
    text_rc.left += to_px(RAIL_TEXT_X_DP, dpi);
    text_rc.right -= inset;
    crate::ui::text::draw_text_line(hdc, page.label(), text_rc, flags);
    SelectObject(hdc, old_font);
    let _ = DeleteObject(HGDIOBJ(icons.0));

    // A focus ring only when Windows says to draw one (keyboard use).
    if di.itemState.0 & ODS_FOCUS != 0 && di.itemState.0 & ODS_NOFOCUSRECT == 0 {
        let ring = to_px(FOCUS_LINE_DP, dpi).max(1);
        let b = CreateSolidBrush(COLORREF(tokens.text));
        let mut r = fill_rc;
        for _ in 0..ring {
            FrameRect(hdc, &r, b);
            r.left += 1;
            r.top += 1;
            r.right -= 1;
            r.bottom -= 1;
        }
        let _ = DeleteObject(HGDIOBJ(b.0));
    }
}

/// Shows `page`: scrolls the body to its top, shows its controls and hides
/// the rest, resizes the scroll range to it and keeps the rail on it. Focus
/// stays where it is, unless it was on a control that just went away, and then
/// it goes to the rail.
unsafe fn show_page(inner: &mut SettingsInner, page: Page) {
    if page != inner.page {
        // A shortcut being learned belongs to the General page, and the line
        // that tells the user to press a key goes with it.
        if inner.learning.is_some() {
            stop_learning(inner);
            set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
        }
        if !inner.body.0.is_null() {
            scroll_body_to(inner.body, 0);
        }
        inner.page = page;
        SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(0)), None);
        sync_visibility(inner);
        apply_dpi(inner);
        SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(1)), None);
        let _ = RedrawWindow(
            Some(inner.hwnd),
            None,
            None,
            RDW_ERASE | RDW_INVALIDATE | RDW_ALLCHILDREN,
        );
    }
    if !inner.rail.0.is_null() {
        let shown = SendMessageW(inner.rail, LB_GETCURSEL, None, None).0;
        if shown != page.index() as isize {
            SendMessageW(inner.rail, LB_SETCURSEL, Some(WPARAM(page.index())), None);
        }
        let focus = GetFocus();
        if !focus.0.is_null()
            && IsChild(inner.hwnd, focus).as_bool()
            && !IsWindowVisible(focus).as_bool()
        {
            let _ = SetFocus(Some(inner.rail));
        }
    }
}

/// Moves focus to the first control of the current page that can take it.
unsafe fn focus_into_page(inner: &SettingsInner) {
    use windows::Win32::UI::WindowsAndMessaging::{GWL_STYLE, WS_DISABLED};
    for c in &inner.children {
        if c.page != Some(inner.page) {
            continue;
        }
        let style = GetWindowLongPtrW(c.hwnd, GWL_STYLE) as u32;
        if IsWindowVisible(c.hwnd).as_bool()
            && style & WS_TABSTOP.0 != 0
            && style & WS_DISABLED.0 == 0
        {
            let _ = SetFocus(Some(c.hwnd));
            return;
        }
    }
    // A page with nothing to type in (Actions) leaves focus on the rail.
}

/// Runs one `WM_SETTINGS_PAGE` request.
unsafe fn handle_page_request(inner: &mut SettingsInner, what: usize, arg: isize) {
    match what {
        PAGE_PICK => {
            if let Some(page) = Page::from_index(arg) {
                show_page(inner, page);
            }
        }
        PAGE_STEP => show_page(inner, inner.page.step(arg)),
        PAGE_ENTER => focus_into_page(inner),
        PAGE_SWAP_FOCUS => {
            if GetFocus() == inner.rail {
                focus_into_page(inner);
            } else {
                let _ = SetFocus(Some(inner.rail));
            }
        }
        _ => {}
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
const WS_EX_CLIENTEDGE: u32 = 0x0000_0200;
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
            .map(|h| prompt_from_edit(&get_text(h)))
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
        set_text(l, &secondary_label(&hotkeys.secondary));
    }
    if let Some(l) = get_dlg_item(hwnd, ID_PAUSE_LABEL) {
        set_text(l, &pause_label(&hotkeys.pause));
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
#[cfg(test)]
fn choice_from_order(order: &[String]) -> usize {
    ActiveProvider::from_order(order).index()
}

/// Issue #302: the combo index to preselect for `order`. An order led by
/// something the combo does not list (a `compat:` endpoint) selects nothing,
/// instead of showing ChatGPT as if it were the active provider. An empty
/// order still reads as ChatGPT, the default.
fn combo_index_for_order(order: &[String]) -> i32 {
    ActiveProvider::shown_for(order).map_or(-1, |p| p.index() as i32)
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
    // Nothing selected (the combo was left empty because a `compat:` entry
    // leads, #302): the user chose nothing, so keep the order as it is.
    if choice >= ActiveProvider::ALL.len() {
        return configured.to_vec();
    }
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

// Test seam: the env var names to treat as set, instead of the process
// environment (which is shared by every test thread).
#[cfg(test)]
thread_local! {
    static ENV_KEYS_SET: std::cell::RefCell<Option<Vec<&'static str>>> =
        const { std::cell::RefCell::new(None) };
}

fn env_var_is_set(var: &str) -> bool {
    #[cfg(test)]
    if let Some(set) = ENV_KEYS_SET.with(|c| c.borrow().clone()) {
        return set.contains(&var);
    }
    std::env::var(var).is_ok()
}

/// The key fields whose environment variable is set (#778): the variable wins
/// over the stored key, so a key typed here would never be used or saved.
/// Pairs the field's control id with the variable name.
fn env_locked_key_fields() -> Vec<(i32, &'static str)> {
    let fields = [
        ("openai", ID_OPENAI_KEY),
        ("anthropic", ID_ANTHROPIC_KEY),
        ("gemini", ID_GEMINI_KEY),
    ];
    crate::config::ENV_OVERRIDE_VARS
        .iter()
        .filter(|(_, var)| env_var_is_set(var))
        .filter_map(|(provider, var)| {
            fields
                .iter()
                .find(|(p, _)| p == provider)
                .map(|(_, id)| (*id, *var))
        })
        .collect()
}

/// The line Settings shows while `vars` are set in the environment. Short:
/// it shares the footer row with the buttons.
fn env_key_hint(vars: &[&str]) -> String {
    match vars {
        [] => String::new(),
        [one] => format!("{one} is set and wins over this field."),
        _ => "Environment API keys are set and win over these fields.".to_string(),
    }
}

/// Greys out the key fields an environment variable overrides and says why
/// in the footer message (#778).
fn lock_env_key_fields(hwnd: HWND) {
    let locked = env_locked_key_fields();
    for (id, _) in &locked {
        if let Some(edit) = get_dlg_item(hwnd, *id) {
            unsafe {
                let _ = EnableWindow(edit, false);
            }
        }
    }
    if let Some(msg) = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE) {
        set_text(msg, &env_key_hint(&env_hint_vars()));
    }
}

/// The names of the set key variables, for [`env_key_hint`].
fn env_hint_vars() -> Vec<&'static str> {
    env_locked_key_fields()
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}

/// What Save says when the start-with-Windows switch could not be changed
/// (#777). The window stays open so nothing is lost.
fn autostart_failure_message() -> &'static str {
    "Could not change Start with Windows."
}

/// The prompt as the multiline edit wants it: CRLF line breaks (#779). The
/// edit shows a bare LF as a box and does not break the line.
fn prompt_to_edit(prompt: &str) -> String {
    prompt.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// The prompt as saved: LF line breaks only (#779).
fn prompt_from_edit(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Merges parsed form values onto a clone of `original`. Any field the form
/// could not produce a sane value for (a blank combo selection, unparsable
/// numeric text) falls back to the value already in `original` rather than
/// zeroing or erroring.
fn build_config(original: &Config, raw: &RawForm) -> Config {
    let mut cfg = original.clone();

    // Only a changed selection touches the order: an untouched Save must not
    // add a vendor the user left out on purpose (#780).
    if raw.provider_choice as i32 != combo_index_for_order(&original.providers.order) {
        cfg.providers.order = merge_provider_order(&original.providers.order, raw.provider_choice);
    }

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
    // The slider works in whole percent, so an untouched slider keeps a
    // hand-edited value it cannot show (2.5, 0.4, 1.255).
    let pct = |v: f32| (clamp_text_scale(v) * 100.0).round() as i32;
    cfg.ui.text_scale = if pct(raw.text_scale_raw) == pct(original.ui.text_scale) {
        original.ui.text_scale
    } else {
        clamp_text_scale(raw.text_scale_raw)
    };
    // An unchanged box keeps the stored prompt byte for byte, CRLF or LF.
    cfg.ui.prompt =
        if raw.prompt.trim().is_empty() || raw.prompt == prompt_from_edit(&original.ui.prompt) {
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

// ---------------------------------------------------------------------------
// `--ui-gallery` support (#560, debug builds only)
// ---------------------------------------------------------------------------

/// Window class for the gallery's Settings window: the production `wndproc`
/// under its own name, so the gallery never touches [`CLASS_NAME`] (rule 9).
#[cfg(debug_assertions)]
const GALLERY_CLASS_NAME: &str = "Wingman.Settings.Gallery.3b8f6a52";

/// A Settings window built from `Config::default()` for the offline gallery:
/// no message loop, no `OPEN_HWND`, no Ollama or cloud model fetch, never the
/// user's `config.toml`. Dropping it destroys the window.
#[cfg(debug_assertions)]
pub struct GalleryWindow {
    raw: *mut SettingsInner,
}

#[cfg(debug_assertions)]
impl GalleryWindow {
    /// Opens the window and shows it without taking focus. `dpi` lays it out
    /// at that DPI whatever the monitor says (the gallery's `--dpi`); `None`
    /// uses the window's own.
    pub fn open(instance: HINSTANCE, dpi: Option<u32>) -> Option<GalleryWindow> {
        static ONCE: std::sync::Once = std::sync::Once::new();
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        ONCE.call_once(|| {
            let _ = OK.set(unsafe { register_class_named(instance, GALLERY_CLASS_NAME) });
        });
        if !OK.get().copied().unwrap_or(false) {
            return None;
        }
        ensure_common_controls();
        FORCED_DPI.with(|f| f.set(dpi.map(|d| d.max(1))));

        let config = Config::default();
        let raw = Box::into_raw(Box::new(SettingsInner::new(config.clone())));
        let class_name = wide_z(GALLERY_CLASS_NAME);
        let title = wide_z("Wingman settings (gallery)");
        let created = unsafe {
            CreateWindowExW(
                WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                window_width_px(96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        };
        let Ok(hwnd) = created else {
            FORCED_DPI.with(|f| f.set(None));
            unsafe { drop(Box::from_raw(raw)) };
            return None;
        };
        // From here the window owns nothing the Drop below cannot free.
        let gallery = GalleryWindow { raw };
        let inner = unsafe { &mut *raw };
        inner.hwnd = hwnd;
        let dpi = dpi_for(hwnd);
        inner.font = build_font(dpi);
        let fitted_h = fitted_height_dp(hwnd, dpi);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                window_width_px(dpi) + nc_width_px(hwnd),
                to_px(fitted_h, dpi),
                SWP_NOMOVE | SWP_NOZORDER,
            );
        }
        let mut rc = windows::Win32::Foundation::RECT::default();
        let client_h_dp = if unsafe { GetClientRect(hwnd, &mut rc) }.is_ok() {
            ((rc.bottom - rc.top) * 96 / dpi as i32).max(1)
        } else {
            fitted_h
        };
        inner.dpi = dpi;
        let built = build_ui_recorded(hwnd, instance, dpi, inner.font, &config, client_h_dp);
        inner.adopt(built);
        sync_card_display_enabled(hwnd);
        unsafe {
            show_prompt_state(inner);
            let _ = ShowWindow(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE,
            );
        }
        Some(gallery)
    }

    pub fn hwnd(&self) -> HWND {
        unsafe { (*self.raw).hwnd }
    }

    /// Picks a page (what a click on the rail does).
    pub(crate) fn show_page(&mut self, page: Page) {
        let inner = unsafe { &mut *self.raw };
        unsafe { show_page(inner, page) };
    }

    /// Expands or collapses the Advanced group (what its toggle does).
    pub fn set_advanced(&mut self, expanded: bool) {
        let inner = unsafe { &mut *self.raw };
        if inner.prompt_expanded != expanded {
            inner.prompt_expanded = expanded;
            unsafe { show_prompt_state(inner) };
        }
    }
}

#[cfg(debug_assertions)]
impl Drop for GalleryWindow {
    fn drop(&mut self) {
        // WM_DESTROY frees the font and flags the close; the Box is ours.
        FORCED_DPI.with(|f| f.set(None));
        unsafe {
            let _ = DestroyWindow((*self.raw).hwnd);
            drop(Box::from_raw(self.raw));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::UI::WindowsAndMessaging::GW_HWNDPREV;

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

    // -- pages and the rail (#872) ----------------------------------------

    /// The title bar and borders of the window, in dp, as the layout tests
    /// assume (a measured window at 150 % is about 38).
    const NC_HEIGHT_GUESS_DP: i32 = 40;

    /// Whether the control `id` is shown (its own visible bit; the test
    /// window itself is never shown).
    fn is_shown(hwnd: HWND, id: i32) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
        let h = get_dlg_item(hwnd, id).expect("control exists");
        unsafe { GetWindowLongPtrW(h, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 }
    }

    #[test]
    fn every_control_is_on_exactly_one_page_or_is_chrome() {
        for (name, id) in ALL_CONTROL_IDS {
            let pages: Vec<Page> = PAGE_CONTROLS
                .iter()
                .filter(|(_, ids)| ids.contains(id))
                .map(|(p, _)| *p)
                .collect();
            if CHROME_IDS.contains(id) {
                assert!(pages.is_empty(), "{name} is chrome but is on {pages:?}");
            } else {
                assert_eq!(
                    pages.len(),
                    1,
                    "{name} must be on exactly one page: {pages:?}"
                );
            }
        }
        // And the table names nothing the id list does not know.
        let known: Vec<i32> = ALL_CONTROL_IDS.iter().map(|(_, id)| *id).collect();
        for (page, ids) in PAGE_CONTROLS {
            assert!(!ids.is_empty(), "{page:?} would be an empty page");
            for id in ids {
                assert!(known.contains(id), "{page:?} lists an unknown id {id}");
            }
        }
        // One entry per page, in rail order.
        let order: Vec<Page> = PAGE_CONTROLS.iter().map(|(p, _)| *p).collect();
        assert_eq!(order, Page::ALL.to_vec());
    }

    #[test]
    fn the_built_controls_sit_on_the_pages_the_table_names() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            for (page, ids) in PAGE_CONTROLS {
                for id in ids {
                    let h = get_dlg_item(hwnd, *id).unwrap_or_else(|| panic!("id {id} missing"));
                    let child = inner
                        .children
                        .iter()
                        .find(|c| c.hwnd == h)
                        .unwrap_or_else(|| panic!("id {id} was not recorded"));
                    assert_eq!(child.page, Some(page), "control id {id}");
                }
            }
            // The footer belongs to no page.
            for id in [ID_SAVE, ID_CANCEL, ID_VALIDATION_MESSAGE] {
                let h = get_dlg_item(hwnd, id).expect("footer control");
                let child = inner
                    .children
                    .iter()
                    .find(|c| c.hwnd == h)
                    .expect("recorded");
                assert_eq!(child.page, None, "footer control id {id}");
            }
            // Every recorded child is on a page or in the footer: a group box
            // or a label made on no page would show on all of them.
            let untagged = inner.children.iter().filter(|c| c.page.is_none()).count();
            assert_eq!(untagged, 3, "only Save, Cancel and the message line");
        });
    }

    #[test]
    fn only_the_current_pages_controls_are_shown() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            assert_eq!(inner.page, Page::General);
            assert!(is_shown(hwnd, ID_AUTOSTART));
            assert!(!is_shown(hwnd, ID_REDACT), "Privacy is not showing");
            assert!(!is_shown(hwnd, ID_OPENAI_KEY), "Models is not showing");
            assert!(is_shown(hwnd, ID_SAVE), "the footer is on every page");

            // Ctrl+3: Privacy.
            unsafe { handle_page_request(inner, PAGE_PICK, 2) };
            assert_eq!(inner.page, Page::Privacy);
            assert!(is_shown(hwnd, ID_REDACT));
            assert!(is_shown(hwnd, ID_MAX_EDGE));
            assert!(!is_shown(hwnd, ID_AUTOSTART));
            assert!(is_shown(hwnd, ID_SAVE));
            let sel = unsafe { SendMessageW(inner.rail, LB_GETCURSEL, None, None).0 };
            assert_eq!(sel, 2, "the rail follows a keyboard pick");

            // Back to General: the redaction box goes again.
            unsafe { handle_page_request(inner, PAGE_PICK, 0) };
            assert!(!is_shown(hwnd, ID_REDACT));
            assert!(is_shown(hwnd, ID_AUTOSTART));
        });
    }

    /// A control with a state of its own stays hidden on its page until that
    /// state says to show it, and never shows on another page.
    #[test]
    fn the_prompt_and_the_ollama_button_keep_their_own_visibility() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            // Advanced with the prompt folded: its toggle shows, the box not.
            unsafe { handle_page_request(inner, PAGE_PICK, 4) };
            assert!(is_shown(hwnd, ID_PROMPT_TOGGLE));
            assert!(!is_shown(hwnd, ID_PROMPT_EDIT));
            inner.prompt_expanded = true;
            unsafe { show_prompt_state(inner) };
            assert!(is_shown(hwnd, ID_PROMPT_EDIT));
            assert!(is_shown(hwnd, ID_RESET_PROMPT));
            // Leaving the page hides them; coming back shows the open box.
            unsafe { handle_page_request(inner, PAGE_PICK, 0) };
            assert!(!is_shown(hwnd, ID_PROMPT_EDIT));
            unsafe { handle_page_request(inner, PAGE_PICK, 4) };
            assert!(is_shown(hwnd, ID_PROMPT_EDIT));

            // The Ollama button is on Models and only when it has an offer.
            unsafe { handle_page_request(inner, PAGE_PICK, 1) };
            assert!(!is_shown(hwnd, ID_OLLAMA_LINK), "nothing to offer yet");
            inner.ollama_link_shown = true;
            unsafe { sync_visibility(inner) };
            assert!(is_shown(hwnd, ID_OLLAMA_LINK));
            unsafe { handle_page_request(inner, PAGE_PICK, 2) };
            assert!(!is_shown(hwnd, ID_OLLAMA_LINK), "not on the Privacy page");
        });
    }

    /// The rail's own message: a click or an arrow key makes the list box
    /// tell its parent `LBN_SELCHANGE`, and the window's `WM_COMMAND` arm picks
    /// the page. Sent the way the list box sends it.
    #[test]
    fn a_rail_selection_change_picks_the_page() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            unsafe {
                SendMessageW(inner.rail, LB_SETCURSEL, Some(WPARAM(3)), None);
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_RAIL as usize | ((LBN_SELCHANGE as usize) << 16))),
                    Some(LPARAM(inner.rail.0 as isize)),
                );
            }
            assert_eq!(unsafe { &*raw }.page, Page::Actions);
            assert!(is_shown(hwnd, ID_ACTIONS_NOTE));
            assert!(!is_shown(hwnd, ID_AUTOSTART));
        });
    }

    /// The posted request the keys make arrives through the window procedure
    /// (a page request that nothing handles would change no page).
    #[test]
    fn a_posted_page_request_reaches_the_window_procedure() {
        with_window(Config::default(), |hwnd, raw| {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETTINGS_PAGE,
                    Some(WPARAM(PAGE_STEP)),
                    Some(LPARAM(-1)),
                );
            }
            assert_eq!(unsafe { &*raw }.page, Page::Advanced, "Ctrl+PageUp wraps");
        });
    }

    #[test]
    fn the_rail_lists_the_five_pages_under_one_name() {
        use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            assert_eq!(get_text(inner.rail), RAIL_NAME);
            assert_eq!(unsafe { GetDlgCtrlID(inner.rail) }, ID_RAIL);
            let count = unsafe { SendMessageW(inner.rail, 0x018B, None, None).0 }; // LB_GETCOUNT
            assert_eq!(count, 5);
            for (i, page) in Page::ALL.iter().enumerate() {
                let mut buf = [0u16; 32];
                let n = unsafe {
                    SendMessageW(
                        inner.rail,
                        0x0189, // LB_GETTEXT
                        Some(WPARAM(i)),
                        Some(LPARAM(buf.as_mut_ptr() as isize)),
                    )
                    .0
                };
                assert_eq!(
                    String::from_utf16_lossy(&buf[..n.max(0) as usize]),
                    page.label()
                );
            }
            // It is one Tab stop, and the first.
            let style = unsafe { GetWindowLongPtrW(inner.rail, GWL_STYLE) } as u32;
            assert!(style & WS_TABSTOP.0 != 0 && style & WS_VISIBLE.0 != 0);
            // Its name comes first, then the rail, then the pages.
            let name = unsafe { GetWindow(hwnd, GW_CHILD) }.expect("first child");
            assert_eq!(get_text(name), RAIL_NAME);
            let next = unsafe { GetWindow(name, GW_HWNDNEXT) }.expect("second child");
            assert_eq!(next, inner.rail, "the rail follows its name in z-order");
        });
    }

    #[test]
    fn page_keys_are_ctrl_only() {
        assert_eq!(
            page_key('1' as u32, true, false),
            Some(PageKey::Pick(Page::General))
        );
        assert_eq!(
            page_key('3' as u32, true, false),
            Some(PageKey::Pick(Page::Privacy))
        );
        assert_eq!(
            page_key('5' as u32, true, false),
            Some(PageKey::Pick(Page::Advanced))
        );
        assert_eq!(page_key('6' as u32, true, false), None);
        assert_eq!(page_key('0' as u32, true, false), None);
        assert_eq!(
            page_key('3' as u32, false, false),
            None,
            "typing a 3 is not a command"
        );
        assert_eq!(
            page_key(0x22, true, false),
            Some(PageKey::Step(1)),
            "PageDown"
        );
        assert_eq!(
            page_key(0x21, true, false),
            Some(PageKey::Step(-1)),
            "PageUp"
        );
        assert_eq!(page_key(0x22, false, false), None);
        // AltGr is Ctrl+Alt: AltGr+3 and AltGr+PageDown are not commands.
        assert_eq!(page_key('3' as u32, true, true), None);
        assert_eq!(page_key(0x22, true, true), None);
        assert_eq!(page_key(0x21, true, true), None);
    }

    /// Save on a bad field shows the page the field is on, then focuses it:
    /// SetFocus on a hidden page's control does nothing.
    #[test]
    fn save_rejecting_a_field_shows_its_page_and_focuses_it() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        with_open_window(|hwnd, _owner| {
            let inner =
                unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner) };
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "abc");
            unsafe { show_page(inner, Page::Privacy) };
            assert!(!is_shown(hwnd, ID_CARD_SECONDS));
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_SAVE as usize)),
                    Some(LPARAM(0)),
                );
            }
            assert_eq!(inner.page, Page::General);
            assert!(is_shown(hwnd, ID_CARD_SECONDS));
            assert!(
                unsafe { IsWindow(Some(hwnd)) }.as_bool(),
                "Save was rejected"
            );
            assert_eq!(
                unsafe { GetFocus() },
                get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap()
            );
        });
    }

    #[test]
    fn pages_step_round_the_ends() {
        assert_eq!(Page::General.step(1), Page::Models);
        assert_eq!(Page::Advanced.step(1), Page::General);
        assert_eq!(Page::General.step(-1), Page::Advanced);
        assert_eq!(Page::from_index(-1), None, "LB_ERR is no page");
        assert_eq!(Page::from_index(5), None);
        assert_eq!(Page::from_index(4), Some(Page::Advanced));
        for (i, p) in Page::ALL.iter().enumerate() {
            assert_eq!(p.index(), i);
        }
    }

    #[test]
    fn rail_colours_follow_the_theme_rules() {
        use crate::ui::theme::{Theme, Tokens, HC_BLACK, HC_WHITE, LIGHT};
        let face = 0x00F0_F0F0;
        let accent = 0x00C0_6700;
        // Not selected: just the face, the theme's text.
        let idle = rail_colors(false, false, face, &LIGHT, &HC_BLACK, accent);
        assert_eq!((idle.fill, idle.pill), (None, None));
        assert_eq!(idle.text, LIGHT.text);
        // Selected: a tint off the face and the accent pill.
        let on = rail_colors(true, false, face, &LIGHT, &HC_BLACK, accent);
        assert_ne!(on.fill, Some(face));
        assert!(on.fill.is_some());
        assert_eq!(on.pill, Some(accent));
        // High contrast: system colours only, in both system themes, with the
        // text on the selection readable.
        for sys in [HC_BLACK, HC_WHITE] {
            let tokens = Tokens::for_theme(Theme::HighContrast, &sys);
            let c = rail_colors(true, true, sys.window, &tokens, &sys, accent);
            assert_eq!(c.fill, Some(sys.highlight));
            assert_eq!(c.text, sys.highlight_text);
            assert_eq!(c.pill, Some(sys.highlight_text));
            let ratio = crate::ui::theme::contrast_ratio(
                crate::ui::theme::relative_luminance(c.text),
                crate::ui::theme::relative_luminance(sys.highlight),
            );
            assert!(ratio >= 4.5, "selected item contrast {ratio}");
            let idle = rail_colors(false, true, sys.window, &tokens, &sys, accent);
            assert_eq!(idle.text, sys.window_text);
        }
    }

    /// The rail is built from `theme.rs` tokens: five items fit under the top
    /// pad in the height the window has at the smallest size it opens at, and
    /// no literal pixel sizes sit in its code.
    #[test]
    fn the_rail_has_room_for_five_items() {
        let rail_dp = crate::ui::theme::RAIL_TOP_PAD_DP + 5 * RAIL_ITEM_H_DP;
        assert!(rail_dp <= MIN_WIN_H_DP.max(WIN_H_DP / 2), "{rail_dp}");
        assert_eq!(WIN_W_DP, RAIL_W_DP + PAGE_W_DP);
        // A page's width is what the one column used to be.
        assert_eq!(PAGE_W_DP, 620);
    }

    /// #872's layout rule: no page scrolls at 100 % and 150 % on a 1080p
    /// screen. The window is as tall as the fullest page (plus the footer and a
    /// title bar of about 40 dp) when the monitor allows it.
    #[test]
    fn no_page_needs_scrolling_at_100_and_150_percent_on_1080p() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            // The work area of a 1080p screen with a taskbar: 1040 px.
            for dpi in [96u32, 144] {
                let fitted = fitted_height_for_work_px(1040, dpi);
                for page in Page::ALL {
                    let need =
                        inner.page_content_dp[page.index()] + FOOTER_H_DP + NC_HEIGHT_GUESS_DP;
                    assert!(
                        need <= fitted,
                        "{page:?} needs {need} dp at {dpi} dpi, the window gets {fitted}"
                    );
                }
            }
        });
    }

    /// #872's measurement: what UI Automation reports for the owner-drawn rail.
    /// Run by hand: `cargo test rail_uia -- --ignored --nocapture`. It prints
    /// the list's control type, name and each item's type, name and selection.
    #[test]
    #[ignore = "live UI Automation measurement; run by hand"]
    fn rail_uia_role_is_a_list_of_named_list_items() {
        use windows::Win32::System::Com::{
            CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED,
        };
        use windows::Win32::UI::Accessibility::{
            IUIAutomation, TreeScope_Children, UIA_ListControlTypeId, UIA_ListItemControlTypeId,
        };
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNOACTIVATE;

        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            }
            let automation: IUIAutomation =
                unsafe { crate::inputs::uia_automation::create_automation() }.expect("UIA");
            let rail = unsafe { automation.ElementFromHandle(inner.rail) }.expect("rail element");
            let ctype = unsafe { rail.CurrentControlType() }.expect("type");
            let name = unsafe { rail.CurrentName() }.expect("name").to_string();
            println!(
                "rail: control type {} (List is {}), name {name:?}",
                ctype.0, UIA_ListControlTypeId.0
            );
            let all = unsafe { automation.CreateTrueCondition() }.expect("condition");
            let items = unsafe { rail.FindAll(TreeScope_Children, &all) }.expect("items");
            let n = unsafe { items.Length() }.expect("len");
            println!("rail children: {n}");
            let mut names = Vec::new();
            for i in 0..n {
                let item = unsafe { items.GetElement(i) }.expect("item");
                let t = unsafe { item.CurrentControlType() }.expect("t");
                let nm = unsafe { item.CurrentName() }.expect("n").to_string();
                println!(
                    "  item {i}: type {} (ListItem is {}), name {nm:?}",
                    t.0, UIA_ListItemControlTypeId.0
                );
                names.push((t, nm));
            }
            unsafe { CoUninitialize() };
            assert_eq!(ctype, UIA_ListControlTypeId, "the rail keeps the List role");
            assert_eq!(name, RAIL_NAME);
            assert_eq!(names.len(), 5);
            for (page, (t, nm)) in Page::ALL.iter().zip(&names) {
                assert_eq!(*t, UIA_ListItemControlTypeId);
                assert_eq!(nm, page.label());
            }
        });
    }

    /// The keys reach the window: `handle_key` turns Ctrl+digit, Ctrl+PageUp
    /// and PageDown, F6 and Enter on the rail into the posted page request.
    /// A source scan (CRLF-normalised, the body must be found), because a
    /// real key needs a real message pump.
    #[test]
    fn handle_key_posts_the_page_requests() {
        let src = include_str!("settings.rs").replace('\r', "");
        let start = src
            .find("unsafe fn handle_key(")
            .expect("handle_key exists");
        let end = start + src[start..].find("\n}\n").expect("handle_key ends");
        let body = &src[start..end];
        assert!(!body.is_empty());
        for needle in [
            "page_key(vk, ctrl, alt)",
            "VK_F6",
            "PAGE_ENTER",
            "WM_SETTINGS_PAGE",
        ] {
            assert!(body.contains(needle), "handle_key lacks {needle}");
        }
    }

    /// Tab goes from the rail into the shown page and skips the hidden ones:
    /// the next stop after the rail is a control of the current page.
    #[test]
    fn tab_from_the_rail_lands_on_the_current_page() {
        use windows::Win32::UI::WindowsAndMessaging::GetNextDlgTabItem;
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            for page in [Page::General, Page::Privacy, Page::Advanced] {
                unsafe { show_page(inner, page) };
                let next = unsafe { GetNextDlgTabItem(hwnd, Some(inner.rail), false) }
                    .expect("a tab stop after the rail");
                let on_page = inner
                    .children
                    .iter()
                    .find(|c| c.hwnd == next)
                    .map(|c| c.page);
                assert_eq!(on_page, Some(Some(page)), "after the rail on {page:?}");
            }
        });
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
            ("Ollama is not running.".to_string(), false, false, None)
        );
    }

    // -- GPU fix section (#382, pure) ---------------------------------------

    #[test]
    fn with_gpu_fix_replaces_the_text_and_makes_the_button_a_fix() {
        let running = ollama_section("Ollama is running.".to_string(), true, true, true);
        let fixed = with_gpu_fix(running.clone(), Some("offer".to_string()));
        assert!(fixed.fix);
        assert_eq!(fixed.link, None);
        assert_eq!(fixed.text, "offer");
        assert_eq!(with_gpu_fix(running.clone(), None), running);
    }

    #[test]
    fn the_gpu_fix_offer_is_never_made_when_nothing_listens() {
        let url = "http://127.0.0.1:11434";
        assert_eq!(gpu_fix_offer(false, false, None, url), None);
        assert_eq!(gpu_fix_offer(true, false, Some(GpuStatus::Gpu), url), None);
    }

    #[test]
    fn the_gpu_fix_offer_is_never_made_for_a_remote_server() {
        let remote = "http://192.168.1.5:11434";
        assert_eq!(
            gpu_fix_offer(true, false, Some(GpuStatus::Cpu), remote),
            None
        );
        assert_eq!(gpu_fix_offer(true, true, None, remote), None);
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

    // -- Follow-ups (#635, #604, #569) -------------------------------------

    #[test]
    fn the_stock_tray_listener_gets_no_local_models_button() {
        let running = ollama_section("Ollama is running.".into(), true, true, true);
        assert!(running.link.is_some());
        let stock = without_link_for_stock_tray(running.clone(), true);
        assert_eq!(stock.link, None);
        assert_eq!(stock.text, running.text, "the warning text is kept");
        // Any other listener keeps its button.
        assert_eq!(without_link_for_stock_tray(running.clone(), false), running);
    }

    #[test]
    fn enter_in_the_pull_box_pulls_instead_of_saving() {
        assert_eq!(enter_command(ID_PULL_NAME), ID_PULL_BUTTON);
        assert_eq!(enter_command(ID_PULL_BUTTON), ID_PULL_BUTTON);
    }

    #[test]
    fn a_pulled_model_is_found_under_its_listed_name() {
        let tag = |name: &str| -> ollama_admin::TagsModel {
            serde_json::from_value(serde_json::json!({ "name": name })).unwrap()
        };
        let models = vec![tag("a:1b"), tag("llama3.2:latest")];
        assert_eq!(resolve_pulled_name(&models, "a:1b"), "a:1b");
        // A name typed without a tag is listed with ":latest".
        assert_eq!(resolve_pulled_name(&models, "llama3.2"), "llama3.2:latest");
        assert_eq!(resolve_pulled_name(&models, "other"), "other");
        // And the combo selects it.
        let (labels, selected) = ollama_model_entries(&models, "llama3.2:latest");
        assert_eq!(labels[selected], "llama3.2:latest");
    }

    /// #569 wired check: the Thinking combo is really greyed in the window
    /// for a 2.5 model, and live for a 3.x one.
    #[test]
    fn the_gemini_thinking_combo_is_greyed_for_a_2_5_model() {
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        for (model, enabled) in [("gemini-2.5-pro", false), ("gemini-3.8-flash", true)] {
            let mut cfg = Config::default();
            cfg.providers.gemini.model = model.to_string();
            with_window(cfg, |hwnd, _| {
                for id in [ID_GEMINI_EFFORT, ID_GEMINI_EFFORT_LABEL] {
                    let h = get_dlg_item(hwnd, id).unwrap();
                    assert_eq!(unsafe { IsWindowEnabled(h) }.as_bool(), enabled, "{model}");
                }
            });
        }
    }

    #[test]
    fn gemini_thinking_applies_only_to_the_3_line() {
        assert!(gemini_thinking_applies("gemini-3.8-flash"));
        assert!(!gemini_thinking_applies("gemini-2.5-pro"));
    }

    // -- Pull model (#566, pure) -------------------------------------------

    #[test]
    fn pull_error_text_names_an_unreachable_server_plainly() {
        let e = anyhow::anyhow!("ollama: transport error: connection refused");
        assert_eq!(
            pull_error_text(&e),
            "Could not reach Ollama. Start it, then try again."
        );
    }

    #[test]
    fn pull_error_text_keeps_the_servers_reason() {
        let e = anyhow::anyhow!("ollama: pull failed: pull model manifest: file does not exist");
        assert_eq!(
            pull_error_text(&e),
            "Pull failed: pull model manifest: file does not exist"
        );
    }

    #[test]
    fn pull_error_text_is_capped_and_has_no_em_dash() {
        let e = anyhow::anyhow!("ollama: pull failed: {}\u{2014}", "x".repeat(500));
        let t = pull_error_text(&e);
        assert!(t.chars().count() <= OLLAMA_TEXT_BUDGET, "{t}");
        assert!(!t.contains('\u{2014}'));
    }

    #[test]
    fn the_pull_controls_have_unique_ids() {
        // The three new ids are in ALL_CONTROL_IDS, which the uniqueness test
        // above iterates; this checks they are really listed there.
        for name in ["ID_PULL_NAME", "ID_PULL_BUTTON", "ID_PULL_STATUS"] {
            assert!(ALL_CONTROL_IDS.iter().any(|(n, _)| *n == name), "{name}");
        }
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
            // The Ollama group is on the Models page; its button only shows
            // while that page does.
            unsafe { show_page(inner, Page::Models) };
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

    /// Opens a real (hidden) Settings window whose Ollama base URL is `base`,
    /// runs `body` with its handle and state, and tears it down.
    fn with_pull_window(base: &str, body: impl FnOnce(HWND, *mut SettingsInner)) {
        let mut cfg = Config::default();
        cfg.providers.ollama.base_url = base.to_string();
        with_window(cfg, body);
    }

    /// Like [`with_pull_window`] for any config.
    fn with_window(cfg: Config, body: impl FnOnce(HWND, *mut SettingsInner)) {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();

        let raw = Box::into_raw(Box::new(SettingsInner::new(cfg.clone())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (pull test)");
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
        let font = build_font(96);
        let client_h_dp = client_size(hwnd).1;
        let built = build_ui_recorded(hwnd, instance, 96, font, &cfg, client_h_dp);
        {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
        }
        body(hwnd, raw);
        unsafe {
            let _ = DestroyWindow(hwnd);
            drop(Box::from_raw(raw));
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
    }

    /// #822: one field column. Every combo and edit that sits beside a label
    /// starts at the same x (the label column's right edge), so the boxes do
    /// not look ragged. The design rect of each child is in dp.
    #[test]
    fn settings_field_columns_share_one_left_edge() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let x_of = |id: i32| -> i32 {
                let child = get_dlg_item(hwnd, id).expect("control exists");
                inner
                    .children
                    .iter()
                    .find(|c| c.hwnd == child)
                    .expect("control was recorded")
                    .x
            };
            let ids = [
                ID_ACTIVE_PROVIDER,
                ID_OPENAI_KEY,
                ID_OPENAI_MODEL,
                ID_OPENAI_EFFORT,
                ID_ANTHROPIC_KEY,
                ID_ANTHROPIC_MODEL,
                ID_ANTHROPIC_EFFORT,
                ID_GEMINI_KEY,
                ID_GEMINI_MODEL,
                ID_GEMINI_EFFORT,
                ID_OLLAMA_MODEL,
                ID_PULL_NAME,
                ID_MAX_EDGE,
                ID_MONITOR,
                ID_CARD_CORNER,
                ID_TEXT_SCALE_TRACK,
                ID_CARD_SECONDS,
                ID_CARD_MONITOR,
                ID_REQUIRE_TICK_FOR,
            ];
            let first = x_of(ids[0]);
            for id in ids {
                assert_eq!(x_of(id), first, "control id {id} is off the field column");
            }
        });
    }

    /// #823: Settings speaks in plain words. No control text carries a config
    /// term (a "(0 =" magic value, or a tell to edit config.toml), and the
    /// second shortcut and the form tick are named by what they do.
    #[test]
    fn settings_labels_are_plain_words_not_config_terms() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            let texts: Vec<String> = inner.children.iter().map(|c| get_text(c.hwnd)).collect();
            assert!(texts.len() > 40, "the window was built: {}", texts.len());
            for t in &texts {
                assert!(!t.contains("(0 ="), "magic zero in a label: {t}");
                assert!(
                    !t.contains("config.toml"),
                    "tells the user to edit a file: {t}"
                );
                assert!(!t.starts_with("Other shortcut"), "unnamed shortcut: {t}");
                assert!(!t.starts_with("Require a tick"), "unclear label: {t}");
                assert!(
                    t != "Card display:" && t != "Card position:",
                    "two names for one thing: {t}"
                );
            }
            assert!(texts.iter().any(|t| t.starts_with("Hide the card after")));
            assert!(texts
                .iter()
                .any(|t| t.starts_with("Shortcut for any keyboard")));
        });
    }

    /// #871 wired-to-nothing: Save in a real window hands the sink the config
    /// the window opened with and the one it built, so the sink (the writer
    /// thread in production) can send only what changed. The sink is the one
    /// path out of the window, so if Save never called it nothing would be
    /// saved.
    #[test]
    fn save_sends_only_the_touched_fields_to_the_sink() {
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
        let seen: std::rc::Rc<std::cell::RefCell<Vec<String>>> = Default::default();
        let record = seen.clone();
        with_window(Config::default(), |hwnd, raw| {
            unsafe { &mut *raw }.save_sink = Some(Box::new(move |original, edited| {
                let changes = crate::config_writer::diff(original, edited).unwrap();
                record
                    .borrow_mut()
                    .extend(changes.iter().map(|c| c.path().join(".")));
                Ok(())
            }));
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "42");
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_SAVE as usize)),
                    Some(LPARAM(0)),
                );
            }
        });
        assert_eq!(*seen.borrow(), vec!["ui.card_seconds"]);
    }

    /// #871: a sink that cannot take the edit keeps the window open and says
    /// why inline (rule 7), and produces no result.
    #[test]
    fn a_sink_that_refuses_keeps_the_window_open_and_says_why() {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageW};
        with_window(Config::default(), |hwnd, raw| {
            unsafe { &mut *raw }.save_sink =
                Some(Box::new(|_, _| Err("the writer is not running".into())));
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_SAVE as usize)),
                    Some(LPARAM(0)),
                );
            }
            assert!(
                unsafe { IsWindow(Some(hwnd)) }.as_bool(),
                "window must stay"
            );
            assert!(unsafe { &*raw }.result.is_none());
            let msg = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).expect("message line");
            assert_eq!(get_text(msg), "Could not save: the writer is not running");
        });
    }

    /// #823 wired-to-nothing: the "Open config file" button is a real control
    /// in the built window, Enter on it presses it, and its failure line is
    /// plain words.
    #[test]
    fn the_open_config_button_exists_and_takes_enter() {
        with_window(Config::default(), |hwnd, _raw| {
            let button = get_dlg_item(hwnd, ID_OPEN_CONFIG).expect("button exists");
            assert_eq!(get_text(button), "Open config file");
        });
        assert_eq!(enter_command(ID_OPEN_CONFIG), ID_OPEN_CONFIG);
        assert!(!OPEN_CONFIG_FAILED.contains('\u{2014}'));
    }

    /// #824: the hotkey hint is not the disabled grey. It is an enabled
    /// control (a disabled static is drawn in the faint disabled colour), and
    /// the colour chosen for it reads at 4.5:1 on the Settings surface.
    #[test]
    fn the_hotkey_hint_is_enabled_and_readable() {
        use crate::ui::theme::{contrast_ratio, relative_luminance, rgb};
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;

        with_window(Config::default(), |hwnd, _raw| {
            let status = get_dlg_item(hwnd, ID_HOTKEY_STATUS).expect("status line");
            assert!(
                unsafe { IsWindowEnabled(status) }.as_bool(),
                "a disabled static is drawn in the disabled grey"
            );
        });

        // The surface Settings is drawn on, read from the system. In a real
        // high contrast session the grey path is not used, so skip it there.
        if crate::ui::card_buttons::system_high_contrast().is_none() {
            let face = unsafe {
                windows::Win32::Graphics::Gdi::GetSysColor(
                    windows::Win32::Graphics::Gdi::COLOR_3DFACE,
                )
            };
            let ink = hint_text_color(false, rgb(0, 0, 0));
            let ratio = contrast_ratio(relative_luminance(ink), relative_luminance(face));
            assert!(ratio >= 4.5, "hint contrast {ratio:.2}:1 is under 4.5:1");
        }
        // High contrast uses the system's own text colour, never a grey.
        let hc = rgb(255, 255, 255);
        assert_eq!(hint_text_color(true, hc), hc);
    }

    /// #972: every helper line is drawn as a hint (secondary text colour), and
    /// nothing else is.
    #[test]
    fn every_helper_line_is_a_hint_and_nothing_else_is() {
        for id in [
            ID_HOTKEY_STATUS,
            ID_MODEL_LIST_HINT,
            ID_CAPTURE_DETAIL_HINT,
            ID_REDACT_HINT,
        ] {
            assert!(is_hint_id(id), "control {id} is a helper line");
        }
        for id in [
            ID_PRIMARY_LABEL,
            ID_PULL_STATUS,
            ID_OLLAMA_STATUS,
            ID_REDACT,
            0,
        ] {
            assert!(!is_hint_id(id), "control {id} is not a helper line");
        }
        with_window(Config::default(), |hwnd, _raw| {
            for id in [ID_CAPTURE_DETAIL_HINT, ID_REDACT_HINT] {
                assert!(get_dlg_item(hwnd, id).is_some(), "hint {id} is built");
            }
        });
    }

    /// #972: helper lines share one left edge: the row labels' edge, or, under
    /// a checkbox, where the checkbox's own text starts.
    #[test]
    fn helper_lines_share_two_left_edges_by_one_rule() {
        use windows::Win32::Foundation::RECT;
        let left = |hwnd: HWND, id: i32| {
            let c = get_dlg_item(hwnd, id).expect("control exists");
            let mut r = RECT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(c, &mut r);
            }
            r.left
        };
        with_window(Config::default(), |hwnd, _raw| {
            let dpi = dpi_for(hwnd);
            let label_edge = left(hwnd, ID_PRIMARY_LABEL);
            assert_eq!(left(hwnd, ID_HOTKEY_STATUS), label_edge);
            assert_eq!(left(hwnd, ID_MODEL_LIST_HINT), label_edge);
            assert_eq!(left(hwnd, ID_CAPTURE_DETAIL_HINT), label_edge);
            // The layout scales `x + indent` as one sum, so the gap is the
            // scaled indent give or take the pixel each edge rounds by.
            let gap = left(hwnd, ID_REDACT_HINT) - left(hwnd, ID_REDACT);
            let want = to_px(CHECKBOX_TEXT_INDENT_DP, dpi);
            assert!(
                (gap - want).abs() <= 1,
                "the note under a checkbox starts at the checkbox's text ({gap} px, want {want} at {dpi} dpi)"
            );
        });
    }

    /// #785: a group ends a small step below its last control, not a blank
    /// band. Measured from each group box to the lowest visible control inside
    /// it, in dp at the test window's DPI.
    #[test]
    fn groups_end_close_below_their_last_control() {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let dpi = dpi_for(hwnd);
            // The control's top and its closed height: a combo's window rect
            // also holds its dropped list.
            let rect = |h: HWND| {
                let (mut r, mut c) = (RECT::default(), RECT::default());
                unsafe {
                    let _ = GetWindowRect(h, &mut r);
                    let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(h, &mut c);
                }
                RECT {
                    bottom: r.top + (c.bottom - c.top),
                    ..r
                }
            };
            let shown = |h: HWND| unsafe {
                windows::Win32::UI::WindowsAndMessaging::GetWindowLongPtrW(
                    h,
                    windows::Win32::UI::WindowsAndMessaging::GWL_STYLE,
                ) as u32
                    & WS_VISIBLE.0
                    != 0
            };
            let groups: Vec<(String, RECT)> = inner
                .children
                .iter()
                .filter(|c| unsafe { is_group_box(c.hwnd) })
                .map(|c| (get_text(c.hwnd), rect(c.hwnd)))
                .collect();
            assert!(groups.len() >= 5, "found {} group boxes", groups.len());
            let mut checked = 0;
            for (name, page) in [
                ("Providers", Page::Models),
                ("Ollama", Page::Models),
                ("Capture", Page::Privacy),
            ] {
                unsafe { show_page(&mut *raw, page) };
                // The group's rect is read with its page showing.
                let groups: Vec<(String, RECT)> = inner
                    .children
                    .iter()
                    .filter(|c| unsafe { is_group_box(c.hwnd) })
                    .map(|c| (get_text(c.hwnd), rect(c.hwnd)))
                    .collect();
                let (_, g) = groups.iter().find(|(t, _)| t == name).expect(name);
                let last = inner
                    .children
                    .iter()
                    .filter(|c| unsafe { !is_group_box(c.hwnd) && shown(c.hwnd) })
                    .map(|c| rect(c.hwnd))
                    .filter(|r| r.top >= g.top && r.bottom <= g.bottom)
                    .map(|r| r.bottom)
                    .max()
                    .expect("controls inside the group");
                let slack = g.bottom - last;
                assert!(
                    slack <= to_px(16, dpi),
                    "{name} group ends {slack} px below its last control"
                );
                checked += 1;
            }
            assert_eq!(checked, 3);
        });
    }

    /// Whether `hwnd` is a group box (a button with `BS_GROUPBOX`).
    unsafe fn is_group_box(hwnd: HWND) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetClassNameW, GetWindowLongPtrW, GWL_STYLE,
        };
        let mut name = [0u16; 16];
        let n = GetClassNameW(hwnd, &mut name) as usize;
        String::from_utf16_lossy(&name[..n]) == "Button"
            && (GetWindowLongPtrW(hwnd, GWL_STYLE) as i32 & 0xF) == BS_GROUPBOX
    }

    /// #935: a 1 px line sits on the first row of the footer, directly under
    /// the scrolling body, at every scale and for every client size, so a row
    /// the body cuts off reads as an edge under a divider.
    #[test]
    fn footer_divider_sits_on_the_body_edge() {
        for dpi in [96u32, 144, 192, 240] {
            let (client_w, client_h) = (to_px(WIN_W_DP, dpi) + 17, to_px(700, dpi));
            let (l, t, r, b) = footer_divider_rect(client_w, client_h, dpi);
            let body_bottom = client_h - to_px(FOOTER_H_DP, dpi);
            assert_eq!(
                t, body_bottom,
                "the line starts where the body ends ({dpi} dpi)"
            );
            assert_eq!(b - t, 1, "a hairline at every scale ({dpi} dpi)");
            assert_eq!(
                (l, r),
                (to_px(RAIL_W_DP, dpi), client_w),
                "the line spans the pages, not the rail"
            );
        }
    }

    /// #935: the divider uses the theme's hairline token, and the system's
    /// own text colour in high contrast (a light grey would vanish there).
    #[test]
    fn footer_divider_colour_comes_from_the_tokens() {
        use crate::ui::theme::{HC_BLACK, HC_WHITE, LIGHT};
        assert_eq!(footer_divider_color(false, &HC_BLACK), LIGHT.border);
        for sys in [HC_BLACK, HC_WHITE] {
            assert_eq!(footer_divider_color(true, &sys), sys.window_text);
        }
    }

    /// #934: a text box takes its fill, border and text from the theme tokens
    /// (the system colours in high contrast), and the focus line is the accent
    /// in a normal theme and the system highlight in high contrast.
    #[test]
    fn edit_colours_come_from_the_tokens() {
        use crate::ui::theme::{rgb, HC_BLACK, HC_WHITE, LIGHT};
        let accent = rgb(0, 103, 192);
        let face = rgb(240, 240, 240);
        let c = edit_colors(false, &HC_BLACK, accent, face);
        assert_eq!(c.fill, LIGHT.control_fill);
        assert_eq!(c.border, LIGHT.border);
        assert_eq!(c.text, LIGHT.text);
        assert_eq!(c.focus_line, accent);
        assert_eq!(c.face, face);
        for sys in [HC_BLACK, HC_WHITE] {
            let c = edit_colors(true, &sys, accent, face);
            assert_eq!(c.fill, sys.window);
            assert_eq!(c.border, sys.window_text);
            assert_eq!(c.text, sys.window_text);
            assert_eq!(c.focus_line, sys.highlight);
        }
    }

    /// #934: the focus line is a bar along the bottom of the box, inside the
    /// border and clear of the rounded corners, at every scale.
    #[test]
    fn the_edit_focus_line_sits_inside_the_bottom_border() {
        for dpi in [96u32, 144, 192, 240] {
            let (w, h) = (to_px(200, dpi), to_px(ROW_H, dpi));
            let (l, t, r, b) = edit_focus_line_rect(w, h, dpi);
            let radius = to_px(crate::ui::theme::CONTROL_RADIUS_DP, dpi);
            assert_eq!(b, h, "covers the bottom border row ({dpi} dpi)");
            assert_eq!(b - t, to_px(2, dpi).max(2), "2 dp thick ({dpi} dpi)");
            assert_eq!(l, radius, "starts after the left corner ({dpi} dpi)");
            assert_eq!(r, w - radius, "ends before the right corner ({dpi} dpi)");
        }
    }

    /// #934: every text box in Settings is restyled: it carries the subclass
    /// that paints the border, and the window above it answers
    /// `WM_CTLCOLOREDIT` with the token fill. A box missing either would still
    /// be the flat white one.
    #[test]
    fn every_settings_text_box_is_styled() {
        use windows::Win32::Graphics::Gdi::{GetBkColor, GetDC, ReleaseDC};
        use windows::Win32::UI::Shell::GetWindowSubclass;
        use windows::Win32::UI::WindowsAndMessaging::{GetClassNameW, WM_CTLCOLOREDIT};
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let mut edits = 0;
            for c in &inner.children {
                let mut name = [0u16; 16];
                let n = unsafe { GetClassNameW(c.hwnd, &mut name) } as usize;
                if String::from_utf16_lossy(&name[..n]) != "Edit" {
                    continue;
                }
                edits += 1;
                let mut data = 0usize;
                assert!(
                    unsafe {
                        GetWindowSubclass(
                            c.hwnd,
                            Some(edit_subclass),
                            EDIT_SUBCLASS_ID,
                            Some(&mut data),
                        )
                    }
                    .as_bool(),
                    "a text box is not subclassed"
                );
                let parent = unsafe { GetParent(c.hwnd) }.expect("parent");
                let hdc = unsafe { GetDC(Some(c.hwnd)) };
                let brush = unsafe {
                    SendMessageW(
                        parent,
                        WM_CTLCOLOREDIT,
                        Some(WPARAM(hdc.0 as usize)),
                        Some(LPARAM(c.hwnd.0 as isize)),
                    )
                };
                let bk = unsafe { GetBkColor(hdc) };
                unsafe { ReleaseDC(Some(c.hwnd), hdc) };
                assert_ne!(brush.0, 0, "the parent returns a brush");
                let hc = crate::ui::card_buttons::system_high_contrast().is_some();
                if !hc {
                    assert_eq!(bk.0, crate::ui::theme::LIGHT.control_fill);
                }
            }
            assert!(
                edits >= 5,
                "found {edits} text boxes, expected the whole set"
            );
            let _ = hwnd;
        });
    }

    /// #934: the frame draw puts the border colour on the box's edge and
    /// leaves the inside alone, and the accent goes on the bottom edge only
    /// while the box has the focus. Drawn on a memory bitmap and read back.
    #[test]
    fn the_frame_draw_paints_border_and_focus_line() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
            SelectObject,
        };
        let colors = EditColors {
            fill: 0x00FF_FFFF,
            border: 0x0000_00FF,
            text: 0,
            focus_line: 0x00FF_0000,
            face: 0x0000_FF00,
            disabled_fill: 0x0080_8080,
            disabled_border: 0x0000_8080,
            disabled_text: 0,
        };
        for dpi in [96u32, 240] {
            let (w, h) = (to_px(120, dpi), to_px(ROW_H, dpi));
            for focused in [false, true] {
                let (screen, dc, bmp) = unsafe {
                    let screen = GetDC(None);
                    let dc = CreateCompatibleDC(Some(screen));
                    let bmp = CreateCompatibleBitmap(screen, w, h);
                    SelectObject(dc, HGDIOBJ(bmp.0));
                    (screen, dc, bmp)
                };
                unsafe { draw_edit_frame(dc, w, h, dpi, &colors, focused, true) };
                let px = |x: i32, y: i32| unsafe { GetPixel(dc, x, y).0 };
                let (mx, my) = (w / 2, h / 2);
                assert_eq!(px(mx, 0), colors.border, "top border ({dpi} dpi)");
                assert_eq!(px(0, my), colors.border, "left border ({dpi} dpi)");
                assert_eq!(px(0, 0), colors.face, "corner shows what is behind");
                if focused {
                    assert_eq!(px(mx, h - 1), colors.focus_line, "focus line ({dpi} dpi)");
                    assert_eq!(px(mx, 0), colors.border, "top stays the border");
                } else {
                    assert_eq!(px(mx, h - 1), colors.border, "bottom border at rest");
                }
                unsafe {
                    let _ = DeleteDC(dc);
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                    ReleaseDC(None, screen);
                }
            }
        }
    }

    /// #934: a disabled box is drawn with the disabled border, and
    /// never gets the accent line even when it holds the focus.
    #[test]
    fn a_disabled_frame_uses_the_disabled_colours() {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetDC, GetPixel, ReleaseDC,
            SelectObject,
        };
        let colors = EditColors {
            fill: 0x00FF_FFFF,
            border: 0x0000_00FF,
            text: 0,
            focus_line: 0x00FF_0000,
            face: 0x0000_FF00,
            disabled_fill: 0x0080_8080,
            disabled_border: 0x0000_8080,
            disabled_text: 0,
        };
        let (w, h) = (to_px(120, 96), to_px(ROW_H, 96));
        let (screen, dc, bmp) = unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w, h);
            SelectObject(dc, HGDIOBJ(bmp.0));
            (screen, dc, bmp)
        };
        unsafe { draw_edit_frame(dc, w, h, 96, &colors, true, false) };
        let px = |x: i32, y: i32| unsafe { GetPixel(dc, x, y).0 };
        assert_eq!(px(w / 2, 0), colors.disabled_border);
        assert_eq!(px(w / 2, h - 1), colors.disabled_border, "no focus line");
        unsafe {
            let _ = DeleteDC(dc);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            ReleaseDC(None, screen);
        }
    }

    /// #934: disabled colours are tokens too: GrayText ink on the window
    /// colour in high contrast, and a fill between the box fill and the face
    /// in a normal theme.
    #[test]
    fn disabled_edit_colours_come_from_the_tokens() {
        use crate::ui::theme::{rgb, HC_BLACK};
        let face = rgb(240, 240, 240);
        let c = edit_colors(true, &HC_BLACK, rgb(0, 103, 192), face);
        assert_eq!(c.disabled_fill, HC_BLACK.window);
        assert_eq!(c.disabled_border, HC_BLACK.gray_text);
        assert_eq!(c.disabled_text, HC_BLACK.gray_text);
        let c = edit_colors(false, &HC_BLACK, rgb(0, 103, 192), face);
        assert_ne!(c.disabled_fill, c.fill);
        assert_ne!(c.disabled_text, c.text);
    }

    /// #934: painting and colour messages use the cached style: they do not
    /// rebuild it (a registry read each). A settings change drops the cache
    /// and the next paint builds it once. Closing frees the brushes.
    #[test]
    fn painting_a_text_box_does_not_read_the_registry() {
        use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
        use windows::Win32::UI::WindowsAndMessaging::{
            WM_CTLCOLOREDIT, WM_NCPAINT, WM_SETTINGCHANGE,
        };
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let edit = inner
                .children
                .iter()
                .map(|c| c.hwnd)
                .find(|h| unsafe { is_edit_control(*h) })
                .expect("a text box");
            let parent = unsafe { GetParent(edit) }.expect("parent");
            let ask = || unsafe {
                let hdc = GetDC(Some(edit));
                SendMessageW(
                    parent,
                    WM_CTLCOLOREDIT,
                    Some(WPARAM(hdc.0 as usize)),
                    Some(LPARAM(edit.0 as isize)),
                );
                ReleaseDC(Some(edit), hdc);
            };
            ask();
            let builds = EDIT_STYLE_BUILDS.get();
            for _ in 0..5 {
                ask();
                unsafe { SendMessageW(edit, WM_NCPAINT, Some(WPARAM(1)), Some(LPARAM(0))) };
            }
            assert_eq!(
                EDIT_STYLE_BUILDS.get(),
                builds,
                "painting rebuilt the style"
            );
            unsafe { SendMessageW(hwnd, WM_SETTINGCHANGE, Some(WPARAM(0)), Some(LPARAM(0))) };
            ask();
            assert_eq!(
                EDIT_STYLE_BUILDS.get(),
                builds + 1,
                "a change rebuilds once"
            );
            ask();
            assert_eq!(EDIT_STYLE_BUILDS.get(), builds + 1);
            unsafe { invalidate_edit_style() };
            assert!(EDIT_STYLE.get().is_none(), "the brushes are released");
        });
    }

    /// Dispatches this thread's queued messages until `done` is true or 10 s
    /// pass.
    fn pump_until(mut done: impl FnMut() -> bool) {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !done() && std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn click_pull_button(hwnd: HWND) {
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
        unsafe {
            SendMessageW(
                hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PULL_BUTTON as usize)),
                Some(LPARAM(0)),
            );
        }
    }

    /// A loopback server that answers one `/api/pull` with `first`, then, if
    /// `hold` is set, keeps the connection open for 5 s before sending
    /// `success`. Returns the base URL.
    fn fake_pull_server(first: &'static str, hold: bool) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.ends_with(b"}") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
                );
                let _ = s.write_all(first.as_bytes());
                let _ = s.flush();
                if hold {
                    std::thread::sleep(Duration::from_secs(5));
                    let _ = s.write_all(b"{\"status\":\"success\"}\n");
                }
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    /// #566 "wired to nothing" check: clicking Pull on the real window starts
    /// a worker whose progress and result reach the window. If the button had
    /// no `WM_COMMAND` arm, or the worker's post had no `WM_OLLAMA_PULL` arm,
    /// the status line would stay empty and the button would still say Pull.
    #[test]
    fn clicking_pull_shows_progress_then_done_in_the_window() {
        let base = fake_pull_server(
            "{\"status\":\"pulling manifest\"}\n{\"status\":\"success\"}\n",
            false,
        );
        with_pull_window(&base, |hwnd, raw| {
            set_text(get_dlg_item(hwnd, ID_PULL_NAME).unwrap(), "tiny:1b");
            click_pull_button(hwnd);
            let button = get_dlg_item(hwnd, ID_PULL_BUTTON).unwrap();
            assert_eq!(get_text(button), "Stop", "a running pull offers Stop");
            let status = get_dlg_item(hwnd, ID_PULL_STATUS).unwrap();
            pump_until(|| get_text(status).starts_with("Pulled"));
            assert_eq!(
                get_text(status),
                "Pulled tiny:1b. It is now in the model list."
            );
            assert_eq!(get_text(button), "Pull", "back to idle when done");
            assert!(unsafe { &*raw }.pull_cancel.is_none());
        });
    }

    #[test]
    fn save_is_refused_while_a_pull_runs_and_a_stale_post_is_ignored() {
        use windows::Win32::UI::WindowsAndMessaging::SendMessageW;
        let base = fake_pull_server("{\"status\":\"pulling manifest\"}\n", true);
        with_pull_window(&base, |hwnd, raw| {
            let status = get_dlg_item(hwnd, ID_PULL_STATUS).unwrap();
            set_text(get_dlg_item(hwnd, ID_PULL_NAME).unwrap(), "tiny:1b");
            click_pull_button(hwnd);
            pump_until(|| get_text(status) == "Pulling manifest...");
            let cancel = unsafe { &*raw }.pull_cancel.clone().expect("running");

            // A post from an older pull changes nothing.
            let current = unsafe { &*raw }.pull_gen;
            *unsafe { &*raw }.pull_slot.lock().unwrap() = PullUpdate {
                text: "stale".into(),
                finished: true,
                succeeded: true,
                model: None,
            };
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_OLLAMA_PULL,
                    Some(WPARAM(current.wrapping_sub(1))),
                    Some(LPARAM(0)),
                );
            }
            assert_eq!(get_text(status), "Pulling manifest...");
            assert!(unsafe { &*raw }.pull_cancel.is_some());

            // Save does not cancel the pull; it says why.
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_SAVE as usize)),
                    Some(LPARAM(0)),
                );
            }
            let msg = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).unwrap();
            assert_eq!(get_text(msg), "A pull is running. Stop it first.");
            assert!(!cancel.load(Ordering::SeqCst));
            assert!(unsafe { &*raw }.result.is_none());
        });
    }

    #[test]
    fn a_failed_pull_ends_in_plain_text_in_the_window() {
        let base = fake_pull_server("{\"error\":\"file does not exist\"}\n", false);
        with_pull_window(&base, |hwnd, _| {
            set_text(get_dlg_item(hwnd, ID_PULL_NAME).unwrap(), "nope");
            click_pull_button(hwnd);
            let status = get_dlg_item(hwnd, ID_PULL_STATUS).unwrap();
            pump_until(|| get_text(status).starts_with("Pull failed"));
            assert_eq!(get_text(status), "Pull failed: file does not exist");
            let button = get_dlg_item(hwnd, ID_PULL_BUTTON).unwrap();
            assert_eq!(get_text(button), "Pull");
        });
    }

    #[test]
    fn stop_ends_a_running_pull_at_once_and_a_bad_name_is_refused() {
        let base = fake_pull_server("{\"status\":\"pulling manifest\"}\n", true);
        with_pull_window(&base, |hwnd, raw| {
            let status = get_dlg_item(hwnd, ID_PULL_STATUS).unwrap();
            let button = get_dlg_item(hwnd, ID_PULL_BUTTON).unwrap();
            // An empty name starts nothing.
            click_pull_button(hwnd);
            assert!(get_text(status).starts_with("Type a model name"));
            assert!(unsafe { &*raw }.pull_cancel.is_none());

            set_text(get_dlg_item(hwnd, ID_PULL_NAME).unwrap(), "tiny:1b");
            click_pull_button(hwnd);
            pump_until(|| get_text(status) == "Pulling manifest...");
            assert_eq!(get_text(status), "Pulling manifest...");
            let cancel = unsafe { &*raw }.pull_cancel.clone().expect("running");

            click_pull_button(hwnd);
            assert!(cancel.load(Ordering::SeqCst), "Stop sets the cancel flag");
            assert_eq!(get_text(button), "Pull");
            assert!(get_text(status).starts_with("Pull stopped"));
            assert!(unsafe { &*raw }.pull_cancel.is_none());
        });
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

        let (status, _, _, _) = ollama_status_line(&cfg, false);
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
    fn a_compat_leader_selects_nothing_and_a_save_keeps_the_order() {
        let order = vec![
            "compat:groq".to_string(),
            "anthropic".to_string(),
            "openai".to_string(),
        ];
        assert_eq!(combo_index_for_order(&order), -1);
        assert_eq!(combo_index_for_order(&[]), 0);
        assert_eq!(combo_index_for_order(&["gemini".to_string()]), 3);
        // CB_GETCURSEL returns -1 for "nothing", read as usize.
        assert_eq!(merge_provider_order(&order, (-1i32) as usize), order);
    }

    #[test]
    fn only_the_newest_fetch_for_a_vendor_is_kept() {
        let sync = CloudSync::default();
        let first = sync.generation[0].fetch_add(1, Ordering::SeqCst) + 1;
        let second = sync.generation[0].fetch_add(1, Ordering::SeqCst) + 1;
        let newest = sync.generation[0].load(Ordering::SeqCst);
        assert!(!is_current_generation(newest, first));
        assert!(is_current_generation(newest, second));
        // Another vendor's counter is independent.
        assert!(is_current_generation(
            sync.generation[1].load(Ordering::SeqCst),
            0
        ));
    }

    #[test]
    fn only_a_key_field_losing_focus_triggers_a_typed_key_fetch() {
        assert_eq!(typed_key_vendor(ID_OPENAI_KEY, EN_KILLFOCUS), Some(0));
        assert_eq!(typed_key_vendor(ID_ANTHROPIC_KEY, EN_KILLFOCUS), Some(1));
        assert_eq!(typed_key_vendor(ID_GEMINI_KEY, EN_KILLFOCUS), Some(2));
        assert_eq!(typed_key_vendor(ID_OPENAI_KEY, 0x0300), None); // EN_CHANGE
        assert_eq!(typed_key_vendor(ID_OLLAMA_MODEL, EN_KILLFOCUS), None);
    }

    #[test]
    fn a_typed_key_is_fetched_once_and_a_masked_or_empty_one_never() {
        let saved = "sk-saved-1234";
        let masked = mask_key(saved);
        // Untouched mask, empty and the unreadable placeholder: nothing.
        assert_eq!(key_to_fetch(saved, &masked, saved), None);
        assert_eq!(key_to_fetch(saved, "", saved), None);
        assert_eq!(key_to_fetch(saved, "   ", saved), None);
        assert_eq!(
            key_to_fetch(
                crate::config::UNREADABLE_KEY_MARKER,
                UNREADABLE_KEY_PLACEHOLDER_TEXT,
                ""
            ),
            None
        );
        // A newly typed key is fetched, trimmed, and not fetched twice.
        assert_eq!(
            key_to_fetch(saved, " sk-new-9999 ", saved).as_deref(),
            Some("sk-new-9999")
        );
        assert_eq!(key_to_fetch(saved, "sk-new-9999", "sk-new-9999"), None);
        // First key typed where none was saved.
        assert_eq!(
            key_to_fetch("", "sk-first", "").as_deref(),
            Some("sk-first")
        );
    }

    /// #678: a key typed into the wrong vendor's field is not fetched with.
    #[test]
    fn a_typed_key_is_fetched_only_for_its_own_vendor() {
        let anthropic = "sk-ant-api03-xyz";
        let openai = "sk-proj-abc";
        let gemini = "AIzaSyABC";
        assert!(key_fits_vendor(0, openai));
        assert!(!key_fits_vendor(0, anthropic));
        assert!(!key_fits_vendor(0, gemini));
        assert!(key_fits_vendor(1, anthropic));
        assert!(!key_fits_vendor(1, openai));
        assert!(key_fits_vendor(2, &format!(" {gemini} ")));
        assert!(!key_fits_vendor(2, openai));
        assert!(!key_fits_vendor(0, ""));
    }

    #[test]
    fn the_model_list_hint_is_one_short_line_without_a_dash() {
        for h in [
            MODEL_LIST_HINT,
            MODEL_LIST_HINT_LOCAL,
            MODEL_LIST_HINT_OFFLINE,
        ] {
            assert!(h.len() <= 64, "it must fit one row: {h}");
            assert!(!h.contains('\u{2014}'));
        }
    }

    /// #674: only Auto and Cloud may ask a cloud vendor for its model list,
    /// and Local and Offline say why they do not.
    #[test]
    fn cloud_lists_are_skipped_in_local_and_offline_with_a_reason() {
        use crate::mode::Mode;
        assert!(cloud_list_allowed(Mode::Auto));
        assert!(cloud_list_allowed(Mode::Cloud));
        assert!(!cloud_list_allowed(Mode::Local));
        assert!(!cloud_list_allowed(Mode::Offline));
        assert_eq!(model_list_hint_for(Mode::Auto), MODEL_LIST_HINT);
        assert_eq!(model_list_hint_for(Mode::Cloud), MODEL_LIST_HINT);
        assert!(model_list_hint_for(Mode::Local).starts_with("Local"));
        assert!(model_list_hint_for(Mode::Offline).starts_with("Offline"));
    }

    /// #427 wired-to-nothing: the hint is a real control in the built window,
    /// above the Ollama model row (Ollama has no `models` list to extend).
    #[test]
    fn the_model_list_hint_is_built_above_the_ollama_row() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();
        let title = wide_z("Wingman settings (model hint test)");
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

        let hint = get_dlg_item(hwnd, ID_MODEL_LIST_HINT).expect("hint exists");
        assert_eq!(get_text(hint), MODEL_LIST_HINT);
        let ollama = get_dlg_item(hwnd, ID_OLLAMA_MODEL).expect("ollama combo");
        let gemini = get_dlg_item(hwnd, ID_GEMINI_MODEL).expect("gemini combo");
        let top = |w: HWND| {
            let mut rc = windows::Win32::Foundation::RECT::default();
            let _ = unsafe { GetWindowRect(w, &mut rc) };
            rc.top
        };
        assert!(top(gemini) < top(hint), "below the cloud model rows");
        assert!(top(hint) < top(ollama), "above the Ollama row");

        unsafe {
            let _ = DestroyWindow(hwnd);
            let _ = DeleteObject(HGDIOBJ(font.0));
        }
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

    // -- untouched Save is a no-op (#775, #779, #780) ----------------------

    /// A config that exercises the fields an untouched Save used to change.
    fn round_trip_config() -> Config {
        let mut cfg = Config::default();
        cfg.providers.order = names(&["openai", "gemini"]);
        cfg.providers.openai.model = "gpt-5".to_string();
        cfg.ui.prompt = "line one\nline two\n\nline four".to_string();
        cfg.ui.card_seconds = 9;
        cfg.ui.text_scale = 1.25;
        cfg
    }

    #[test]
    fn an_untouched_save_never_adds_a_vendor_to_the_order() {
        for order in [
            vec!["openai"],
            vec!["anthropic"],
            vec!["openai", "gemini"],
            vec!["ollama"],
            vec!["gemini", "anthropic"],
        ] {
            let mut original = Config::default();
            original.providers.order = names(&order);
            let cfg = build_config(&original, &raw_from(&original));
            assert_eq!(cfg.providers.order, original.providers.order, "{order:?}");
        }
    }

    #[test]
    fn choosing_a_provider_still_changes_the_order() {
        let mut original = Config::default();
        original.providers.order = names(&["openai"]);
        let mut raw = raw_from(&original);
        raw.provider_choice = 1;
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.providers.order, names(&["anthropic", "openai"]));
    }

    #[test]
    fn an_untouched_save_keeps_a_hand_edited_text_scale_and_a_crlf_prompt() {
        for scale in [2.5_f32, 0.4, 1.255] {
            let mut original = Config::default();
            original.ui.text_scale = scale;
            original.ui.prompt = "one\r\ntwo".to_string();
            // What the form reads back: the slider in whole percent.
            let mut raw = raw_from(&original);
            raw.text_scale_raw = (clamp_text_scale(scale) * 100.0).round() / 100.0;
            raw.prompt = prompt_from_edit(&prompt_to_edit(&original.ui.prompt));
            let cfg = build_config(&original, &raw);
            assert_eq!(cfg.ui.text_scale, scale);
            assert_eq!(cfg.ui.prompt, "one\r\ntwo");
            // Moving the slider still changes it.
            raw.text_scale_raw = 1.0;
            assert_eq!(build_config(&original, &raw).ui.text_scale, 1.0);
        }
    }

    #[test]
    fn enter_and_esc_with_a_dropdown_open_do_not_save_or_cancel_the_window() {
        use windows::Win32::UI::WindowsAndMessaging::{
            IsWindow, PeekMessageW, PM_REMOVE, SW_SHOWNA, WM_KEYDOWN,
        };
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let combo = get_dlg_item(hwnd, ID_OPENAI_MODEL).unwrap();
            for key in [VK_ESCAPE, VK_RETURN] {
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                    SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(1)), None);
                }
                assert!(unsafe { wheel_over_open_combo(combo) }, "list did not open");
                let msg = MSG {
                    hwnd: combo,
                    message: WM_KEYDOWN,
                    wParam: WPARAM(key.0 as usize),
                    ..Default::default()
                };
                assert!(unsafe { handle_key(hwnd, &msg, inner.prompt_edit) });
                // No Save or Cancel was queued, and the window is alive.
                let mut queued = MSG::default();
                let posted = unsafe {
                    PeekMessageW(&mut queued, Some(hwnd), WM_COMMAND, WM_COMMAND, PM_REMOVE)
                };
                assert!(!posted.as_bool(), "a command was posted for {key:?}");
                assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
                assert!(inner.result.is_none());
                unsafe {
                    SendMessageW(combo, CB_SHOWDROPDOWN, Some(WPARAM(0)), None);
                }
            }
            // With the list closed, Esc still cancels the window.
            let msg = MSG {
                hwnd: combo,
                message: WM_KEYDOWN,
                wParam: WPARAM(VK_ESCAPE.0 as usize),
                ..Default::default()
            };
            assert!(unsafe { handle_key(hwnd, &msg, inner.prompt_edit) });
            let mut queued = MSG::default();
            let posted =
                unsafe { PeekMessageW(&mut queued, Some(hwnd), WM_COMMAND, WM_COMMAND, PM_REMOVE) };
            assert!(posted.as_bool());
        });
    }

    #[test]
    fn the_env_hint_survives_a_save_that_clears_other_messages() {
        ENV_KEYS_SET.with(|c| *c.borrow_mut() = Some(vec!["GEMINI_API_KEY"]));
        assert_eq!(env_hint_vars(), vec!["GEMINI_API_KEY"]);
        ENV_KEYS_SET.with(|c| *c.borrow_mut() = None);
    }

    #[test]
    fn the_prompt_box_text_uses_crlf_and_the_saved_prompt_uses_lf() {
        assert_eq!(prompt_to_edit("a\nb\n\nc"), "a\r\nb\r\n\r\nc");
        assert_eq!(prompt_to_edit("a\r\nb"), "a\r\nb");
        assert_eq!(prompt_from_edit("a\r\nb\r\n\r\nc"), "a\nb\n\nc");
        assert_eq!(prompt_from_edit(&prompt_to_edit("x\ny")), "x\ny");
    }

    #[test]
    fn the_env_hint_names_the_variable_and_has_no_dash() {
        assert_eq!(env_key_hint(&[]), "");
        let one = env_key_hint(&["OPENAI_API_KEY"]);
        assert!(one.contains("OPENAI_API_KEY"), "{one}");
        let many = env_key_hint(&["OPENAI_API_KEY", "GEMINI_API_KEY"]);
        assert!(!many.is_empty());
        assert!(!one.contains('\u{2014}') && !many.contains('\u{2014}'));
    }

    #[test]
    fn an_autostart_failure_message_is_plain_and_has_no_dash() {
        let m = autostart_failure_message();
        assert!(
            !m.is_empty() && m.len() <= 45 && !m.contains('\u{2014}'),
            "{m}"
        );
    }

    #[test]
    fn a_model_that_prefixes_a_listed_one_opens_as_itself() {
        let cfg = round_trip_config();
        assert!(cfg.providers.openai.models.iter().any(|m| m == "gpt-5"));
        assert!(cfg.providers.openai.models.iter().any(|m| m == "gpt-5.5"));
        with_window(cfg, |_hwnd, raw| {
            let inner = unsafe { &*raw };
            assert_eq!(read_form(inner).providers.openai.model, "gpt-5");
        });
    }

    #[test]
    fn opening_settings_and_saving_with_no_edits_changes_nothing() {
        let original = round_trip_config();
        with_window(original.clone(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            assert_eq!(read_form(inner), original);
            // The multiline prompt shows real line breaks.
            let shown = get_text(get_dlg_item(hwnd, ID_PROMPT_EDIT).unwrap());
            assert_eq!(shown, "line one\r\nline two\r\n\r\nline four");
        });
    }

    #[test]
    fn an_env_set_key_field_is_disabled_and_the_window_says_why() {
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        ENV_KEYS_SET.with(|c| *c.borrow_mut() = Some(vec!["OPENAI_API_KEY"]));
        with_window(Config::default(), |hwnd, _raw| {
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            let other = get_dlg_item(hwnd, ID_ANTHROPIC_KEY).unwrap();
            assert!(!unsafe { IsWindowEnabled(key) }.as_bool());
            assert!(unsafe { IsWindowEnabled(other) }.as_bool());
            let msg = get_text(get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).unwrap());
            assert!(msg.contains("OPENAI_API_KEY"), "{msg}");
        });
        ENV_KEYS_SET.with(|c| *c.borrow_mut() = None);
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
            provider_choice: combo_index_for_order(&original.providers.order) as usize,
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
    fn build_config_keeps_a_compat_leading_order_on_an_untouched_save() {
        let mut original = Config::default();
        original.providers.order = vec![
            "compat:groq".to_string(),
            "anthropic".to_string(),
            "openai".to_string(),
        ];
        let raw = raw_from(&original);
        // Production reads CB_GETCURSEL = -1 for an empty combo.
        assert_eq!(raw.provider_choice, (-1i32) as usize);
        assert_eq!(
            build_config(&original, &raw).providers.order,
            original.providers.order
        );
        let mut max = raw_from(&original);
        max.provider_choice = usize::MAX;
        assert_eq!(
            build_config(&original, &max).providers.order,
            original.providers.order
        );
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
    /// `ROW_H + ROW_GAP = 30`: `708 + 30 = 738`. #427 then added the hint
    /// row under the cloud model dropdowns to the Providers group, one more
    /// `ROW_H + ROW_GAP = 30`: `738 + 30 = 768`. Kept here as a literal, not
    /// derived, so a change to those sections has to update this test
    /// deliberately rather than silently keep passing against a moving
    /// target.
    ///
    /// #872 put every page's first group at the top of the body, so the Prompt
    /// group, alone on the Advanced page, starts at the page's top pad.
    const CONTENT_TOP_DP: i32 = 8;

    /// The Advanced page's content height with the prompt folded away, in dp:
    /// what `window_height_dp` is given when that page is the tallest.
    fn advanced_collapsed_content_dp() -> i32 {
        footer_layout(CONTENT_TOP_DP, 0, false).content_h()
    }

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
        // The prompt group is alone on its page now (#872), so it only
        // outgrows the view on a very short one.
        let f = prompt_footer_layout(CONTENT_TOP_DP, 200);
        assert!(f.content_h() > 200 - FOOTER_H_DP);
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
        let collapsed = window_height_dp(advanced_collapsed_content_dp(), false, 5000, nc);
        let e = footer_layout(CONTENT_TOP_DP, 0, true);
        let expanded_needed = e.content_h() + FOOTER_H_DP + nc;
        assert!(collapsed + MIN_PROMPT_EDIT_H <= expanded_needed);
        assert_eq!(
            window_height_dp(advanced_collapsed_content_dp(), true, WIN_H_DP, nc),
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
        // The fullest page is taller than the fit: the fit wins.
        assert_eq!(window_height_dp(900, false, 500, 40), 500);
    }

    #[test]
    fn collapsed_window_height_holds_exactly_the_body_and_the_footer() {
        let nc = 40;
        let h = window_height_dp(advanced_collapsed_content_dp(), false, 5000, nc);
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
        let short = footer_layout(CONTENT_TOP_DP, 200, true);
        assert!(short.content_h() > 200 - FOOTER_H_DP, "must scroll");
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
        assert_eq!(get_text(prompt_edit), prompt_to_edit(&config.ui.prompt));

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

    // -- gallery DPI seam (#745) -------------------------------------------

    /// #745 wired check: a gallery Settings window opened at 240 DPI lays
    /// out at 240 (inner.dpi, `dpi_for`, the Save button's width), is
    /// bigger than one at 96, and a `WM_DPICHANGED` cannot replace it. If the
    /// override were wired to nothing, both buttons would be the same size.
    #[cfg(debug_assertions)]
    #[test]
    fn a_forced_gallery_dpi_lays_settings_out_at_that_dpi() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let save_w = |w: &GalleryWindow| {
            let save = get_dlg_item(w.hwnd(), ID_SAVE).expect("Save button");
            client_size(save).0
        };
        let small = GalleryWindow::open(instance, Some(96)).expect("open at 96");
        let small_w = save_w(&small);
        drop(small);
        let big = GalleryWindow::open(instance, Some(240)).expect("open at 240");
        assert_eq!(dpi_for(big.hwnd()), 240);
        assert_eq!(unsafe { &*big.raw }.dpi, 240);
        let big_w = save_w(&big);
        assert!(big_w * 100 / small_w >= 235, "{small_w} -> {big_w}");
        unsafe {
            SendMessageW(
                big.hwnd(),
                WM_DPICHANGED,
                Some(WPARAM(96 | (96 << 16))),
                Some(LPARAM(0)),
            );
        }
        assert_eq!(unsafe { &*big.raw }.dpi, 240, "a DPI message must not win");
        drop(big);
        assert_eq!(forced_dpi(), None, "closing the window clears the override");
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
            page: None,
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
            // The Models page is the longest, so it is the one to scroll.
            unsafe { show_page(inner, Page::Models) };
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

        // The Pull button starts below the view and a scroll to the bottom
        // (what the scrollbar's End does) brings it into the view.
        let toggle = get_dlg_item(hwnd, ID_PULL_BUTTON).expect("pull button");
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
        unsafe { show_page(&mut *raw, Page::Advanced) };
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
        assert_eq!(
            get_text(prompt),
            prompt_to_edit(&Config::default().ui.prompt)
        );

        click();
        assert!(!visible(prompt) && !visible(reset));
        assert_eq!(get_text(toggle), "Show Check my work &prompt");
        assert_eq!(top_in_parent_px(hwnd, save), collapsed_save_y);
        assert_eq!(content(), collapsed_content);
        // Hidden text is still readable, so Save keeps it.
        assert_eq!(
            get_text(prompt),
            prompt_to_edit(&Config::default().ui.prompt)
        );
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
        // The footer, the body, the rail and the rail's name (#872).
        assert_eq!(direct_child_count(hwnd), footer + 3);
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

    // -- layout fixes #624, #625 and the card display grey-out #597 -------

    struct LayoutWin(HWND, *mut SettingsInner);
    impl Drop for LayoutWin {
        fn drop(&mut self) {
            unsafe {
                let _ = DestroyWindow(self.0);
                drop(Box::from_raw(self.1));
            }
        }
    }

    /// A window sized the way `show_modal` sizes it (client width = design
    /// width + the body's scrollbar), at 96 dpi, built from a default config.
    fn open_layout_window() -> LayoutWin {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();
        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (layout test)");
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_CAPTION | WS_SYSMENU | WS_VISIBLE,
                0,
                0,
                window_width_px(96),
                to_px(WIN_H_DP, 96),
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        }
        .expect("CreateWindowExW");
        let win = LayoutWin(hwnd, raw);
        unsafe {
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                window_width_px(96) + nc_width_px(hwnd),
                to_px(WIN_H_DP, 96),
                SWP_NOMOVE | SWP_NOZORDER,
            );
        }
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
        let inner = unsafe { &mut *raw };
        inner.hwnd = hwnd;
        inner.font = font;
        inner.adopt(built);
        unsafe { apply_dpi(inner) };
        win
    }

    fn right_in_parent_px(parent: HWND, child: HWND) -> i32 {
        use windows::Win32::Graphics::Gdi::ScreenToClient;
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut r = windows::Win32::Foundation::RECT::default();
        unsafe { GetWindowRect(child, &mut r) }.expect("GetWindowRect");
        let mut pt = windows::Win32::Foundation::POINT {
            x: r.right,
            y: r.top,
        };
        assert!(unsafe { ScreenToClient(parent, &mut pt) }.as_bool());
        pt.x
    }

    /// #625: Cancel's right edge lines up with the group boxes' (one margin
    /// before the scrollbar column), and the client is exactly as wide as the
    /// design says. The window used to be too narrow by its frame width and
    /// the footer was pushed over the scrollbar column, so Cancel sat a few
    /// pixels from the window edge.
    #[test]
    fn the_footer_ends_in_line_with_the_groups_and_the_client_is_design_wide() {
        let win = open_layout_window();
        let (client_w, _) = client_size(win.0);
        assert_eq!(client_w, window_width_px(96), "client width");
        let cancel = get_dlg_item(win.0, ID_CANCEL).expect("Cancel");
        let right = right_in_parent_px(win.0, cancel);
        assert_eq!(
            client_w - right,
            to_px(MARGIN, 96) + scrollbar_width_px(96),
            "Cancel margin"
        );
        // The group boxes end one margin before the scrollbar column.
        let group_right = to_px(WIN_W_DP - MARGIN, 96);
        assert_eq!(right, group_right, "footer edge vs group edge");
    }

    /// #624: the Card group's first label is wide enough for its own text, so
    /// it does not wrap and clip its second line.
    #[test]
    fn the_card_seconds_label_fits_on_one_line() {
        use windows::Win32::Graphics::Gdi::{GetTextExtentPoint32W, SelectObject};
        let win = open_layout_window();
        let seconds = get_dlg_item(win.0, ID_CARD_SECONDS).expect("seconds");
        let body = unsafe { &*win.1 }.body;
        let label = unsafe { GetWindow(seconds, GW_HWNDPREV) }.expect("label");
        let label_w = {
            let mut r = windows::Win32::Foundation::RECT::default();
            unsafe { GetClientRect(label, &mut r) }.expect("label rect");
            r.right - r.left
        };
        let text = CARD_SECONDS_LABEL;
        let wide: Vec<u16> = text.encode_utf16().collect();
        let dc = unsafe { GetDC(Some(body)) };
        let old = unsafe { SelectObject(dc, HGDIOBJ((&*win.1).font.0)) };
        let mut size = windows::Win32::Foundation::SIZE::default();
        unsafe {
            let _ = GetTextExtentPoint32W(dc, &wide, &mut size);
            SelectObject(dc, old);
            ReleaseDC(Some(body), dc);
        }
        assert!(
            size.cx <= label_w,
            "label is {label_w}px wide, its text needs {}px",
            size.cx
        );
        // The edit starts right of the label, not under it.
        assert!(
            top_in_parent_px(body, label) == top_in_parent_px(body, seconds),
            "same row"
        );
    }

    #[test]
    fn the_card_display_applies_unless_the_card_follows_the_mouse() {
        for label in CARD_CORNER_LABELS {
            assert_eq!(card_display_applies(label), label != "Next to the mouse");
        }
        assert!(card_display_applies(""), "an unknown label keeps it usable");
    }

    /// #597, with a real window: choosing "Next to the mouse" through a real
    /// `WM_COMMAND` (CBN_SELCHANGE) greys out Card display and its label;
    /// choosing a corner again re-enables both. The default config starts
    /// with a corner, so both start enabled.
    #[test]
    fn card_display_is_greyed_out_while_the_card_follows_the_mouse() {
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        let win = open_layout_window();
        let monitor = get_dlg_item(win.0, ID_CARD_MONITOR).expect("display combo");
        let label = get_dlg_item(win.0, ID_CARD_MONITOR_LABEL).expect("display label");
        let corner = get_dlg_item(win.0, ID_CARD_CORNER).expect("corner combo");
        let enabled = |h: HWND| unsafe { IsWindowEnabled(h) }.as_bool();
        assert!(enabled(monitor) && enabled(label), "starts enabled");

        let pick = |text: &str| {
            combo_select(corner, text);
            unsafe {
                SendMessageW(
                    win.0,
                    WM_COMMAND,
                    Some(WPARAM(
                        ((CBN_SELCHANGE as usize) << 16) | ID_CARD_CORNER as usize,
                    )),
                    Some(LPARAM(corner.0 as isize)),
                );
            }
        };
        pick("Next to the mouse");
        assert!(!enabled(monitor), "display greyed out");
        assert!(!enabled(label), "label greyed out");
        pick("Top left");
        assert!(enabled(monitor) && enabled(label), "re-enabled");
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
        let pos = (100, 100, WIN_W_DP, 700);
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

    /// The tests that use the process-wide `OPEN_HWND` guard take this lock, so
    /// two of them never see each other's window.
    static OPEN_GUARD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// A real top-level window of this thread to stand in for the app's main
    /// window: Settings posts to it.
    fn test_owner() -> HWND {
        unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                windows::core::w!("STATIC"),
                windows::core::w!("settings test owner"),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
        }
        .expect("owner window")
    }

    /// Takes one message of `id` posted to `to`, if any.
    fn take_posted(to: HWND, id: u32) -> Option<MSG> {
        let mut m = MSG::default();
        unsafe { PeekMessageW(&mut m, Some(to), id, id, PM_REMOVE) }
            .as_bool()
            .then_some(m)
    }

    /// Opens a real, hidden Settings window the way `open` does, on the tests'
    /// class, and runs `body` with it. Always closes it.
    fn with_open_window(body: impl FnOnce(HWND, HWND)) {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let _lock = OPEN_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        let owner = test_owner();
        assert!(open_impl(
            instance,
            owner,
            &Config::default(),
            TEST_CLASS_NAME,
            false
        ));
        let hwnd = HWND(OPEN_HWND.load(Ordering::SeqCst) as *mut c_void);
        assert!(!hwnd.0.is_null(), "open must set the guard");
        body(hwnd, owner);
        close_now();
        unsafe {
            let _ = DestroyWindow(owner);
        }
    }

    /// #871 wired-to-nothing: the window `open` makes exists with no loop of its
    /// own, frees itself when destroyed (no `Box::from_raw` by a caller), clears
    /// the guard, and tells the owner it is gone. If the owner post or the
    /// self-free were not wired, this would leak, crash or time out.
    #[test]
    fn open_makes_a_modeless_window_that_frees_itself_and_tells_the_owner() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        with_open_window(|hwnd, owner| {
            assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
            let inner =
                unsafe { &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner) };
            assert!(inner.owns_itself);
            assert_eq!(inner.owner, owner);
            assert!(take_posted(owner, crate::app::WM_APP_SETTINGS_LEFT).is_none());
            close_now();
            assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
            assert_eq!(OPEN_HWND.load(Ordering::SeqCst), 0);
            assert!(
                take_posted(owner, crate::app::WM_APP_SETTINGS_LEFT).is_some(),
                "closing must tell the owner, or a waiting card never shows"
            );
        });
    }

    /// #871 wired-to-nothing: Save in the real window posts the changed fields
    /// (and only those) to the owner as `WM_APP_SETTINGS_SAVE`, then closes. If the
    /// sink were not the production one, or the post were dropped, nothing would
    /// reach the writer and no card would ever say "Settings saved".
    #[test]
    fn save_posts_only_the_changed_fields_to_the_owner_and_closes() {
        use windows::Win32::UI::WindowsAndMessaging::{IsWindow, SendMessageW};
        with_open_window(|hwnd, owner| {
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "42");
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_SAVE as usize)),
                    Some(LPARAM(0)),
                );
            }
            let posted = take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE)
                .expect("Save must post the save to the owner window");
            let save = unsafe { *Box::from_raw(posted.lParam.0 as *mut crate::app::SettingsSave) };
            let paths: Vec<String> = save.changes.iter().map(|c| c.path().join(".")).collect();
            assert_eq!(paths, vec!["ui.card_seconds"]);
            assert_eq!(save.edited.ui.card_seconds, 42);
            assert!(
                !unsafe { IsWindow(Some(hwnd)) }.as_bool(),
                "Save closes the window"
            );
        });
    }

    /// #871 finding 7: Windows destroys a window whose creation fails after
    /// `WM_NCCREATE`, and the window procedure already freed the state then. The
    /// failure path must not free it a second time.
    #[test]
    fn a_window_that_fails_to_create_after_nccreate_frees_its_state_exactly_once() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::WM_CREATE;
        const FAIL_CLASS: &str = "Wingman.Settings.Test.FailCreate.2c8e51d7";
        unsafe extern "system" fn fail_create(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == WM_CREATE {
                return LRESULT(-1);
            }
            wndproc(hwnd, msg, wparam, lparam)
        }
        let _lock = OPEN_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let class = wide_z(FAIL_CLASS);
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(fail_create),
                hInstance: instance,
                lpszClassName: PCWSTR(class.as_ptr()),
                ..Default::default()
            };
            assert!(unsafe { RegisterClassExW(&wc) } != 0);
        });
        let before = INNERS_DROPPED.load(Ordering::SeqCst);
        let opened = open_impl(
            instance,
            HWND(std::ptr::null_mut()),
            &Config::default(),
            FAIL_CLASS,
            false,
        );
        assert!(!opened, "the window cannot be created");
        assert_eq!(
            INNERS_DROPPED.load(Ordering::SeqCst) - before,
            1,
            "the window state must be freed once, not twice"
        );
        assert_eq!(OPEN_HWND.load(Ordering::SeqCst), 0, "no guard is left set");
    }

    /// The same failure before `WM_NCCREATE` (the class does not exist): the
    /// window never saw the state, so `open_impl` frees it, once.
    #[test]
    fn a_window_that_fails_before_nccreate_is_freed_once_by_the_caller() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let _lock = OPEN_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let before = INNERS_DROPPED.load(Ordering::SeqCst);
        let opened = open_impl(
            HINSTANCE(h.0),
            HWND(std::ptr::null_mut()),
            &Config::default(),
            "Wingman.Settings.Test.NoSuchClass.91b4",
            false,
        );
        assert!(!opened);
        assert_eq!(INNERS_DROPPED.load(Ordering::SeqCst) - before, 1);
    }

    /// #871: Esc in the open window is taken by `pre_dispatch` and becomes a
    /// Cancel command for the window; a key for some other window is left alone.
    #[test]
    fn pre_dispatch_takes_keys_for_the_settings_window_and_nothing_else() {
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, IsWindow};
        with_open_window(|hwnd, owner| {
            let field = get_dlg_item(hwnd, ID_CARD_SECONDS).expect("a child control");
            let esc = MSG {
                hwnd: field,
                message: WM_KEYDOWN,
                wParam: WPARAM(VK_ESCAPE.0 as usize),
                ..Default::default()
            };
            assert!(pre_dispatch(&esc), "Esc in a Settings control is ours");
            let cancel = take_posted(hwnd, WM_COMMAND).expect("Esc posts Cancel");
            assert_eq!(cancel.wParam.0 & 0xFFFF, ID_CANCEL as usize);

            let foreign = MSG {
                hwnd: owner,
                message: WM_KEYDOWN,
                wParam: WPARAM(VK_ESCAPE.0 as usize),
                ..Default::default()
            };
            assert!(
                !pre_dispatch(&foreign),
                "another window's key passes through"
            );

            // Delivering the Cancel closes it, with no loop of ours involved.
            unsafe { DispatchMessageW(&cancel) };
            assert!(!unsafe { IsWindow(Some(hwnd)) }.as_bool());
        });
    }

    /// #871: with no window open, `pre_dispatch` never claims a message.
    #[test]
    fn pre_dispatch_ignores_everything_while_no_window_is_open() {
        let _lock = OPEN_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        assert_eq!(OPEN_HWND.load(Ordering::SeqCst), 0);
        let msg = MSG {
            message: WM_KEYDOWN,
            wParam: WPARAM(VK_ESCAPE.0 as usize),
            ..Default::default()
        };
        assert!(!pre_dispatch(&msg));
        assert!(!is_foreground());
    }

    #[test]
    fn a_second_open_while_one_is_open_creates_no_window() {
        let _lock = OPEN_GUARD_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Simulate "already open" without actually creating a window: set
        // the guard directly, exactly like a real open would, and confirm
        // open's early-return path is taken (no window is created, no panic,
        // no hang) and counts as success: the open one is brought forward.
        let prev = OPEN_HWND.swap(1, Ordering::SeqCst);
        let h = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
            .expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let result = open(instance, HWND(std::ptr::null_mut()), &Config::default());
        assert!(result);
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

    // -- #873: Mode, Pause and the pause shortcut on General ----------------

    #[test]
    fn the_mode_rows_are_the_trays_labels_in_the_trays_order() {
        use crate::mode::Mode;
        let labels: Vec<&str> = MODE_CHOICES.iter().map(|m| m.label()).collect();
        assert_eq!(labels, ["Cloud", "Local", "Auto", "Offline"]);
        for (i, m) in MODE_CHOICES.iter().enumerate() {
            assert_eq!(mode_index(*m), i);
            assert_eq!(mode_at(i as isize), Some(*m));
        }
        // A combo with nothing picked answers CB_ERR (-1).
        assert_eq!(mode_at(-1), None);
        assert_eq!(mode_at(MODE_CHOICES.len() as isize), None);
        assert_eq!(
            mode_at(mode_index(Mode::Offline) as isize),
            Some(Mode::Offline)
        );
    }

    #[test]
    fn the_pause_line_says_running_or_how_long_it_is_paused() {
        assert_eq!(pause_status_line(None), "Wingman is running.");
        assert_eq!(
            pause_status_line(Some("Paused until 14:05")),
            "Paused until 14:05."
        );
        assert_eq!(pause_status_line(Some("Paused")), "Paused.");
        for text in [pause_status_line(None), pause_status_line(Some("Paused"))] {
            assert!(!text.contains('\u{2014}'), "no em dash in {text}");
        }
    }

    #[test]
    fn pause_is_offered_while_running_and_resume_while_paused() {
        assert_eq!(pause_buttons_enabled(false), (true, false));
        assert_eq!(pause_buttons_enabled(true), (false, true));
    }

    #[test]
    fn the_pause_menu_offers_the_trays_three_choices_in_order() {
        use crate::pause::PauseChoice;
        let choices: Vec<PauseChoice> = PAUSE_MENU.iter().map(|(c, _)| *c).collect();
        assert_eq!(
            choices,
            [
                PauseChoice::OneHour,
                PauseChoice::UntilTomorrow,
                PauseChoice::UntilResumed
            ]
        );
        for (_, label) in PAUSE_MENU {
            assert!(!label.contains('\u{2014}'), "no em dash in {label}");
        }
    }

    /// Posts what the control would, so a combo or button wired to nothing fails.
    fn command_posted_for(owner: HWND) -> Option<crate::app::SettingsCommand> {
        take_posted(owner, crate::app::WM_APP_SETTINGS_COMMAND)
            .and_then(|m| crate::app::SettingsCommand::from_wparam(m.wParam.0))
    }

    /// #873 wired-to-nothing: picking Offline in the real combo posts the
    /// owner window the command whose handler moves the tray's check mark.
    #[test]
    fn picking_a_mode_in_the_combo_asks_the_owner_to_switch() {
        use crate::mode::Mode;
        with_open_window(|hwnd, owner| {
            let combo = get_dlg_item(hwnd, ID_MODE).expect("mode combo");
            combo_select(combo, Mode::Offline.label());
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(((CBN_SELCHANGE as usize) << 16) | ID_MODE as usize)),
                    Some(LPARAM(combo.0 as isize)),
                );
            }
            assert_eq!(
                command_posted_for(owner),
                Some(crate::app::SettingsCommand::Mode(Mode::Offline))
            );
        });
    }

    #[test]
    fn the_resume_button_asks_the_owner_to_resume() {
        with_open_window(|hwnd, owner| {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(
                        ((BN_CLICKED as usize) << 16) | ID_RESUME_BUTTON as usize,
                    )),
                    Some(LPARAM(0)),
                );
            }
            assert_eq!(
                command_posted_for(owner),
                Some(crate::app::SettingsCommand::Resume)
            );
        });
    }

    /// #873: the controls follow a change made in the tray while Settings is open.
    #[test]
    fn the_controls_follow_the_trays_mode_and_pause() {
        use crate::mode::Mode;
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        with_open_window(|hwnd, _owner| {
            let status = get_dlg_item(hwnd, ID_PAUSE_STATUS).expect("pause line");
            let pause = get_dlg_item(hwnd, ID_PAUSE_BUTTON).expect("pause button");
            let resume = get_dlg_item(hwnd, ID_RESUME_BUTTON).expect("resume button");
            let enabled = |h: HWND| unsafe { IsWindowEnabled(h) }.as_bool();

            apply_live(hwnd, Mode::Offline, Some("Paused until 14:05"));
            assert_eq!(combo_selected_text(hwnd, ID_MODE), "Offline");
            assert_eq!(get_text(status), "Paused until 14:05.");
            assert!(!enabled(pause) && enabled(resume));

            apply_live(hwnd, Mode::Auto, None);
            assert_eq!(combo_selected_text(hwnd, ID_MODE), "Auto");
            assert_eq!(get_text(status), "Wingman is running.");
            assert!(enabled(pause) && !enabled(resume));
        });
    }

    /// #873: the Pause button asks for its menu by message, so the menu's own
    /// loop never runs inside the handler that holds the window's state, and a
    /// pending Change is cancelled first (the hook would eat the menu's keys).
    #[test]
    fn the_pause_button_defers_its_menu_and_cancels_a_pending_change() {
        with_open_window(|hwnd, _owner| {
            let inner =
                unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner) };
            inner.learning = Some(HK_PRIMARY);
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(
                        ((BN_CLICKED as usize) << 16) | ID_PAUSE_BUTTON as usize,
                    )),
                    Some(LPARAM(0)),
                );
            }
            let inner =
                unsafe { &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner) };
            assert_eq!(inner.learning, None, "the pending Change was not cancelled");
            assert!(
                take_posted(hwnd, WM_PAUSE_MENU).is_some(),
                "the menu was not asked for by message"
            );
        });
    }

    /// The Change beside the pause shortcut arms the pause slot (`HK_PAUSE`):
    /// the arm is in the handler, and pressing it speaks on the status line
    /// (no keyboard hook runs in the test, so it says why it cannot arm).
    #[test]
    fn the_pause_change_button_arms_the_pause_slot() {
        let src = include_str!("settings.rs").replace('\r', "");
        assert!(
            src.contains(
                "ID_CHANGE_PAUSE if notify == BN_CLICKED => start_change(inner, HK_PAUSE),"
            ),
            "the Change button is not wired to HK_PAUSE"
        );
        with_open_window(|hwnd, _owner| {
            let status = get_dlg_item(hwnd, ID_HOTKEY_STATUS).expect("status line");
            let before = get_text(status);
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(
                        ((BN_CLICKED as usize) << 16) | ID_CHANGE_PAUSE as usize,
                    )),
                    Some(LPARAM(0)),
                );
            }
            assert_ne!(get_text(status), before, "pressing Change said nothing");
            stop_learning(unsafe {
                &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner)
            });
        });
    }

    /// The pause shortcut is a row on General: a label and a Change button.
    #[test]
    fn the_pause_shortcut_label_shows_the_staged_chord_or_not_set() {
        let mut hk = Config::default().hotkeys;
        hk.pause = None;
        assert_eq!(pause_label(&hk.pause), "Pause shortcut: not set");
        let c = chord(b'P' as u32, true, false, true, false);
        hk.pause = Some(c);
        assert_eq!(
            pause_label(&hk.pause),
            format!("Pause shortcut: {}", chord_to_string(&c))
        );
    }
}
