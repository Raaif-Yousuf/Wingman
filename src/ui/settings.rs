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
//! **Changes apply at once** (#875): there is no Save and no Cancel. A checkbox
//! or a list applies when it changes, a text or number box when Enter is pressed
//! or the box is left, the text size slider when it is released. Each change is
//! one field (`settings_commit::Field`): `commit_field` works out what that field
//! changed (`settings_commit::plan`), hands it to the `settings-writer` thread
//! through the owner window, and the thread patches that key onto the file as it
//! is on disk now, so a hand edit made meanwhile survives. The writer's answer
//! comes back (`note_write_result`): the status line says "Saved: Card corner"
//! with an Undo button, or why it was not saved, and the control goes back to the
//! value that is on disk. A window left with a row still failed puts up one card
//! (`WM_APP_SETTINGS_LEFT`'s `wparam` is the count). Coming back to the window
//! reads the file again, on a worker thread, and shows a hand edit in every row
//! that is not being typed in. This module never touches the disk or the network
//! on the UI thread.
//!
//! The window has a left navigation rail and five pages (#872): General,
//! Models, Privacy, Actions and Advanced. Every control is created once, tagged
//! with its page, and the rail shows one page's controls and hides the rest, so
//! a hidden page is out of the Tab order and the accessibility tree. Ctrl+1 to
//! Ctrl+5, Ctrl+PageUp and Ctrl+PageDown change the page from anywhere, F6
//! moves between the rail and the page. The status line and Undo are pinned under
//! the pages.
//!
//! The General page's Mode row and Pause and Resume buttons act at once (#873): they post `WM_APP_SETTINGS_COMMAND` to the owner, which runs the
//! tray's own `set_mode`, `pause_for` and `resume`. The app calls [`sync_live`]
//! after any change, so the controls follow the tray and the pause key too. The
//! pause shortcut is applied like the other two.
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
//! - **Theme** (#582): a real window is still drawn with the stock (light)
//!   system control appearance, whatever the OS light/dark setting is. Dark is
//!   being built slice by slice (spec `2026-10-16-settings-dark-and-high-
//!   contrast-design.md`) and stays off for users, behind
//!   `settings_theme::SETTINGS_DARK_ENABLED`, until every control follows it:
//!   a correct light window beats a half-dark one. So far the title bar, the
//!   window face, the labels, the text boxes, the rail, the footer divider,
//!   the push buttons and the checkboxes (`settings_buttons`, drawn from
//!   `NM_CUSTOMDRAW`) follow the theme; combos, the trackbar and the
//!   scrollbar are still stock. `edit_style` resolves the theme once per change
//!   and everything else reads it. The UI gallery forces a theme
//!   (`--ui-gallery --theme`), which bypasses the gate.
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
//!   rebind. The new chord is applied at once, like every other field.

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
    IsChild, IsDialogMessageW, IsWindow, KillTimer, LoadCursorW, PeekMessageW, PostMessageW,
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
use crate::ui::settings_commit::{self as commit, Field};
use crate::ui::settings_models::{self, Slot};

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
    // What the FILE held when the window opened is read on a worker once the
    // window exists (#1105, `start_open_read`): the running config is not that
    // (a tray change not yet written, a default never written), and comparing
    // with it refuses a save that touched nothing by hand. Tests do not read
    // the real file (rule 9).
    if live {
        inner.file_path = crate::config::Config::path().ok();
    }
    inner.save_sink = Some(Box::new(move |label, changes, original, edited| {
        crate::app::post_settings_save(
            owner_raw,
            crate::app::SettingsSave {
                label: label.to_string(),
                changes,
                original: original.clone(),
                edited: edited.clone(),
            },
        )
    }));
    let raw = Box::into_raw(inner);

    let class_name = wide_z(class);
    let title = wide_z(SETTINGS_TITLE);
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
    // #582: the dark buttons and checkboxes need common controls v6, which the
    // exe's manifest asks for (#807). A debug run of the real window says so
    // when it is missing; a release build just keeps the stock paint.
    #[cfg(debug_assertions)]
    if let Some(box_) = get_dlg_item(hwnd, ID_AUTOSTART) {
        debug_assert!(
            crate::ui::settings_theme::button_is_common_controls_v6(box_),
            "Settings buttons are not common controls v6: is assets/app.manifest embedded?"
        );
    }
    sync_card_display_enabled(hwnd);
    if live {
        // #1105: the file's baseline and the Run key are read off the UI thread;
        // the Start with Windows box is unchecked until its worker answers.
        start_open_read(inner_ref);
        // The box cannot be clicked until the Run key has been read.
        if let Some(box_) = get_dlg_item(hwnd, ID_AUTOSTART) {
            unsafe {
                let _ = EnableWindow(box_, false);
            }
        }
        start_autostart(inner_ref, AutostartJob::Read);
        // #527, #568: the Ollama status and button arrive later, off the UI
        // thread; the group says "Checking Ollama..." until then.
        spawn_ollama_status_probe(
            hwnd,
            config.providers.ollama.clone(),
            config.providers.order.iter().any(|p| p == "ollama"),
            inner_ref.ollama_status.clone(),
            inner_ref.alive.clone(),
        );
        start_checking_animation(inner_ref, crate::ui::card::client_area_animation_enabled());
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

/// Enables or disables the open Settings window (#1013), so an OS dialog that
/// is not owned by it can stand in for a modal. Returns whether a window was
/// there to change. UI thread only.
pub fn set_enabled(on: bool) -> bool {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    if open == 0 {
        return false;
    }
    unsafe {
        let _ = EnableWindow(HWND(open as *mut c_void), on);
    }
    true
}

/// Brings the open Settings window back to the front (#1013), after an OS
/// dialog that disabled it closed and Windows activated another app. Does
/// nothing when Settings is gone. UI thread only.
pub fn bring_to_front() {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    if open == 0 {
        return;
    }
    let hwnd = HWND(open as *mut c_void);
    unsafe {
        if IsWindow(Some(hwnd)).as_bool() {
            let _ = SetForegroundWindow(hwnd);
        }
    }
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
/// push button is pressed (Enter on Undo undoes, on the prompt toggle toggles);
/// anywhere else Enter applies the box it is in (#875).
fn enter_command(ctrl_id: i32) -> i32 {
    match ctrl_id {
        ID_UNDO
        | ID_PROMPT_TOGGLE
        | ID_RESET_PROMPT
        | ID_OLLAMA_LINK
        | ID_CHANGE_PRIMARY
        | ID_CHANGE_SECONDARY
        | ID_CHANGE_PAUSE
        | ID_PAUSE_BUTTON
        | ID_RESUME_BUTTON
        | ID_PULL_BUTTON
        | ID_OPEN_CONFIG
        | ID_OPEN_ACTIONS
        | ID_IMPORT_ACTION
        | ID_EXPORT_ACTION
        | ID_ADD_PROVIDER
        | ID_OPENAI_SHOW_KEY
        | ID_ANTHROPIC_SHOW_KEY
        | ID_GEMINI_SHOW_KEY => ctrl_id,
        // #635: Enter in the Pull model box starts the pull.
        ID_PULL_NAME => ID_PULL_BUTTON,
        _ => ID_APPLY_FOCUSED,
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
        // #875: otherwise Esc puts back the text being typed, or closes the
        // window when there is none (`escape_pressed`).
        if !cancel_learning_if_active(hwnd) {
            let _ = PostMessageW(
                Some(hwnd),
                WM_COMMAND,
                WPARAM(ID_ESCAPE as usize),
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
/// waited for it can be shown. `unsaved` is how many rows were still failed when
/// the window closed (0 when it only lost the foreground): the owner puts up one
/// card for them. A window with no owner (the tests') says nothing.
fn tell_owner_left(owner: HWND, unsaved: usize) {
    if owner.0.is_null() {
        return;
    }
    unsafe {
        let _ = PostMessageW(
            Some(owner),
            crate::app::WM_APP_SETTINGS_LEFT,
            WPARAM(unsaved),
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
    /// The smaller font of the helper lines (#1015); null until the first
    /// layout, when they use `font`.
    hint_font: HFONT,
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
    /// #874: the Models page controls that move or hide with the provider
    /// choice, and what the choice currently hides.
    models_flow: Vec<FlowChild>,
    /// The user pressed "Add another provider": every vendor shows.
    models_revealed: bool,
    /// Which provider blocks show now: OpenAI, Anthropic, Gemini, Ollama.
    models_shown: [bool; 4],
    /// Whether "Add another provider" is offered now.
    models_add_shown: bool,
    /// How much shorter the Models page is than with every block, in dp.
    models_shrink_dp: i32,
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
    /// #828: the dot frame of the "Checking Ollama" cue; `Some` only while the
    /// check runs and the cue's timer is set.
    checking_frame: Option<usize>,
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
    /// Where a commit sends its change: in production the owner window, which
    /// hands it to the writer thread (see `open`). `None` in the tests' windows.
    save_sink: Option<SaveSink>,
    /// #875: the file as this window last saw it (defaults for keys it does not
    /// spell out). A whole value (the provider order) is checked against it, so a
    /// hand edit of the same list is never written over. `original` is what the
    /// window believes is applied; this is what it believes the file holds.
    baseline: Config,
    /// #875: the writes in flight, the rows that failed and what Undo does.
    tracker: commit::Tracker,
    /// Whether the Undo button shows (the latest change was saved).
    undo_shown: bool,
    /// Set once the window is closing: nothing is committed from then on.
    closing: bool,
    /// The window is not the active one (`WM_ACTIVATE` inactive): a key box that
    /// loses the focus for that reason is not applied.
    deactivated: bool,
    /// The key box that lost the focus while the window was deactivated and has
    /// not been applied: applied on close if nothing applied it sooner.
    deferred_key: Option<Field>,
    /// #876: the typed keys waiting on their vendor, and what to say when the
    /// write of a checked key is answered.
    key_checks: commit::KeyChecks,
    key_notes: commit::KeyNotes,
    /// #876: how a typed key is asked of its vendor. The real request, except
    /// in the tests, which put a fake behind it.
    key_seams: KeySeams,
    /// #875: the file's path and the modified time the window last read, so
    /// coming back to the window reads it again only when it moved. `None` in the
    /// tests' windows (rule 9: they never read the real file).
    file_path: Option<std::path::PathBuf>,
    file_mtime: Option<std::time::SystemTime>,
    /// A re-read of the file is running on its worker.
    refreshing: bool,
    /// The worker's answer, taken by `WM_SETTINGS_REFRESH`.
    refresh_slot: RefreshSlot,
    /// #1105: the open-time read of the file runs on a worker; its answer is
    /// taken by `WM_SETTINGS_OPENED`.
    open_slot: OpenSlot,
    /// A commit or Undo has moved `baseline` since the window opened.
    wrote: bool,
    /// #1105: the Start with Windows worker. One job at a time; a click that
    /// comes while one runs sets `autostart_again`.
    autostart_busy: bool,
    autostart_again: bool,
    autostart_slot: AutostartSlot,
    autostart_seams: AutostartSeams,
    /// #1152: the Actions page's worker (the file check, the list to export),
    /// one at a time, and the actions its Export menu offers.
    actions_busy: std::sync::Arc<std::sync::atomic::AtomicBool>,
    actions_slot: ActionsSlot,
    export_choices: Vec<(String, String)>,
    /// #345: the hotkeys as edited in this window. Starts as the original's
    /// and is applied at once when a chord is learned.
    hotkeys: Hotkeys,
    /// #345: the slot a pending Change is waiting on a key for.
    learning: Option<usize>,
    /// #345: the `(slot, chord)` last warned about, so pressing the same
    /// conflicting chord again confirms it (same flow as the tray's).
    pending_conflict: Option<(usize, Chord)>,
}

/// Takes one commit: what the field is called, the keys to patch onto the file,
/// the config before it and the config with it (from which a changed API key is
/// worked out). An `Err` is a plain sentence for the row.
type SaveSink = Box<
    dyn Fn(&str, Vec<crate::config_writer::FieldChange>, &Config, &Config) -> Result<(), String>,
>;

/// What a typed key's check reads from outside the window, so the tests can put
/// a fake vendor and a chosen mode behind them (rule 9: no real network, no
/// process-wide mode shared between parallel tests).
#[derive(Clone, Copy)]
struct KeySeams {
    /// Asks a vendor about a key. Runs on a worker thread.
    check: fn(crate::provider::model_list::Vendor, &str) -> crate::provider::model_list::KeyCheck,
    /// The mode now.
    mode: fn() -> crate::mode::Mode,
    /// The full model list, for a check whose first page was not all of it.
    fetch_all: fn(crate::provider::model_list::Vendor, &str) -> anyhow::Result<Vec<String>>,
    /// Starts the check's worker thread (#1140: a test makes it fail).
    spawn: fn(Box<dyn FnOnce() + Send>) -> std::io::Result<()>,
    /// #1130: tells the app a key saved on close was refused, as a card. Args:
    /// the owner window, the headline, the detail. True when it was posted.
    late_card: fn(isize, String, String) -> bool,
}

/// The real worker thread of a key check.
fn spawn_key_check_thread(job: Box<dyn FnOnce() + Send>) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("settings-key-check".into())
        .spawn(job)
        .map(|_| ())
}

impl KeySeams {
    #[cfg(not(test))]
    const REAL: KeySeams = KeySeams {
        check: crate::provider::model_list::check_key,
        mode: crate::mode::current,
        fetch_all: crate::provider::model_list::fetch,
        spawn: spawn_key_check_thread,
        late_card: crate::app::post_late_key_refusal,
    };

    /// In a test build a window never reaches a vendor, whichever test made it:
    /// the check finds nothing out, in Auto mode.
    #[cfg(test)]
    const REAL: KeySeams = KeySeams {
        check: |_, _| crate::provider::model_list::KeyCheck::Unchecked,
        mode: || crate::mode::Mode::Auto,
        fetch_all: |_, _| Err(anyhow::anyhow!("no network in tests")),
        spawn: spawn_key_check_thread,
        late_card: crate::app::post_late_key_refusal,
    };
}

/// What the re-read of the file found: its modified time and what it parsed to.
struct Refreshed {
    mtime: Option<std::time::SystemTime>,
    /// `None` when the file had not moved, or could not be read as settings.
    config: Option<Config>,
}

type RefreshSlot = std::sync::Arc<std::sync::Mutex<Option<Refreshed>>>;

// Counts the `SettingsInner` values dropped on this thread, so a test can tell
// a double free from a single one. Per thread on purpose: many other tests
// build and drop a `SettingsInner` in parallel without taking the open-guard
// lock, and a process-wide count would include their drops.
#[cfg(test)]
thread_local! {
    static INNERS_DROPPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
impl Drop for SettingsInner {
    fn drop(&mut self) {
        INNERS_DROPPED.with(|c| c.set(c.get() + 1));
    }
}

impl SettingsInner {
    fn new(original: Config) -> Self {
        SettingsInner {
            hwnd: HWND(std::ptr::null_mut()),
            font: HFONT(std::ptr::null_mut()),
            hint_font: HFONT(std::ptr::null_mut()),
            dpi: 96,
            children: Vec::new(),
            content_top_dp: 0,
            win_h_dp: WIN_H_DP,
            prompt_edit: HWND(std::ptr::null_mut()),
            rail: HWND(std::ptr::null_mut()),
            page: Page::General,
            page_content_dp: [0; 5],
            models_flow: Vec::new(),
            models_revealed: false,
            models_shown: [true; 4],
            models_add_shown: false,
            models_shrink_dp: 0,
            body: HWND(std::ptr::null_mut()),
            body_dpi: 96,
            ollama_link_url: None,
            ollama_link_shown: false,
            ollama_fix: false,
            ollama_status: Default::default(),
            checking_frame: None,
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
            baseline: original.clone(),
            tracker: Default::default(),
            undo_shown: false,
            closing: false,
            deactivated: false,
            deferred_key: None,
            key_checks: Default::default(),
            key_notes: Default::default(),
            key_seams: KeySeams::REAL,
            file_path: None,
            file_mtime: None,
            refreshing: false,
            refresh_slot: Default::default(),
            open_slot: Default::default(),
            wrote: false,
            autostart_busy: false,
            autostart_again: false,
            autostart_slot: Default::default(),
            autostart_seams: AutostartSeams::REAL,
            actions_busy: Default::default(),
            actions_slot: Default::default(),
            export_choices: Vec::new(),
            learning: None,
            pending_conflict: None,
            original,
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
        self.models_flow = built.models_flow;
        self.content_top_dp = built.content_top_dp;
        self.win_h_dp = built.win_h_dp;
        // The Models page starts with the active provider's rows only, and
        // only the first page shows.
        unsafe { sync_models_flow(self) };
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
    if msg == WM_EXPORT_MENU {
        run_export_menu(hwnd);
        return LRESULT(0);
    }
    // #582: the footer buttons (Undo, Reset) draw from the tokens in dark.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_NOTIFY {
        if let Some(answer) = answer_button_draw(hwnd, lparam) {
            return answer;
        }
    }
    let inner = &mut *ptr;

    match msg {
        WM_CLOSE => {
            // #875: nothing waits for a Save, so the box being typed in is
            // applied first. A destroyed window does not reliably send
            // `EN_KILLFOCUS`.
            commit_focused(inner);
            // A key box that lost the focus to a deactivation (alt-tab) was held
            // back; closing is the last chance. A key that fits is applied, one
            // that does not becomes a failed row, which the close card counts.
            if let Some(field) = inner.deferred_key.take() {
                commit_field(inner, field);
            }
            save_checking_keys_unchecked(inner);
            inner.closing = true;
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
            // #875: the label follows the drag; the size is applied on release.
            if (wparam.0 & 0xFFFF) as u32 == TB_ENDTRACK {
                commit_field(inner, Field::TextScale);
            }
            LRESULT(0)
        }
        // #875: the app's answer for a write (sent, not posted: it is answered
        // at once with whether the row took it).
        WM_SETTINGS_WRITE_RESULT => {
            let answer = &*(lparam.0 as *const WriteAnswer);
            LRESULT(on_write_result(inner, answer) as isize)
        }
        // #875: the worker re-read the file after the window was activated.
        WM_SETTINGS_REFRESH => {
            take_refresh(inner);
            LRESULT(0)
        }
        // #1105: the Start with Windows worker answered.
        WM_SETTINGS_AUTOSTART => {
            take_autostart(inner);
            LRESULT(0)
        }
        // #1152: the Actions page's worker answered.
        WM_SETTINGS_ACTIONS => {
            take_actions(inner);
            LRESULT(0)
        }
        // #1105: the worker read the file the window opened on.
        WM_SETTINGS_OPENED => {
            take_open_read(inner);
            LRESULT(0)
        }
        // #872: the rail draws itself through us.
        WM_DRAWITEM => {
            if is_rail_draw_item(lparam) {
                draw_rail_item(lparam);
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        WM_MEASUREITEM => {
            let mis = &*(lparam.0 as *const windows::Win32::UI::Controls::MEASUREITEMSTRUCT);
            if mis.CtlID == ID_RAIL as u32 {
                measure_rail_item(lparam, inner.dpi);
                LRESULT(1)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }
        // The rail's background is the window's, not the list box's white.
        WM_CTLCOLORLISTBOX => LRESULT(face_brush_now().0 as isize),
        // #582: the title bar follows the theme from the first frame.
        windows::Win32::UI::WindowsAndMessaging::WM_CREATE => {
            apply_title_bar(hwnd);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // #582: the window's own face and the labels on it (the status line),
        // where Settings paints them (dark). Otherwise the class brush and the
        // default colours, as before.
        windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND if edit_style().paints_face => {
            erase_with_face(hwnd, wparam)
        }
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC if edit_style().paints_face => {
            answer_label_colors(windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void))
        }
        WM_SETTINGS_PAGE => {
            handle_page_request(inner, wparam.0, lparam.0);
            LRESULT(0)
        }
        WM_PAINT => {
            paint_footer_divider(hwnd);
            LRESULT(0)
        }
        WM_OLLAMA_STATUS => {
            stop_checking_animation(inner);
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
                // #875: a model just pulled is picked in the box, so it is
                // applied like any pick (no Save to carry it).
                if prefer.is_some() {
                    commit_field(inner, Field::OllamaModel);
                }
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
        // #876: a vendor answered for a typed key.
        WM_KEY_CHECKED => {
            if let Some((generation, verdict)) = commit::decode_verdict(lparam.0) {
                on_key_checked(inner, wparam.0, generation, verdict);
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
            inner.deactivated = true;
            tell_owner_left(inner.owner, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // #871: losing the foreground releases whatever waited for it (a
        // finished request's card, a press of the Wingman key). Set before the
        // default handling takes the focus away, so a key box losing it for this
        // reason is not applied.
        WM_ACTIVATE if (wparam.0 & 0xFFFF) == 0 => {
            inner.deactivated = true;
            tell_owner_left(inner.owner, 0);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        // #875: coming back to the window, read the file again (on a worker)
        // so a hand edit made meanwhile shows in the rows nobody is typing in.
        WM_ACTIVATE => {
            inner.deactivated = false;
            start_refresh(inner);
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_TIMER if wparam.0 == LEARN_TIMER_ID => {
            learn_timed_out(inner);
            LRESULT(0)
        }
        WM_TIMER if wparam.0 == CHECK_TIMER_ID => {
            tick_checking_animation(inner);
            LRESULT(0)
        }
        // #934: the text box colours (and the accent in them) are read once
        // per change, not per paint; repaint the boxes with the new ones.
        windows::Win32::UI::WindowsAndMessaging::WM_SETTINGCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_SYSCOLORCHANGE
        | windows::Win32::UI::WindowsAndMessaging::WM_THEMECHANGED
        | WM_DWMCOLORIZATIONCOLORCHANGED_MSG => {
            // #582: the erase repaints the painted face, so it is only asked
            // for when the face was painted before or is now (light does not
            // flicker more than it did).
            let was_painted = edit_style().paints_face;
            invalidate_edit_style();
            invalidate_rail_style();
            // The one owner of the theme resolves it again here; the title
            // bar follows.
            apply_title_bar(hwnd);
            let erase = if was_painted || edit_style().paints_face {
                windows::Win32::Graphics::Gdi::RDW_ERASE
            } else {
                Default::default()
            };
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                windows::Win32::Graphics::Gdi::RDW_FRAME
                    | RDW_INVALIDATE
                    | erase
                    | windows::Win32::Graphics::Gdi::RDW_ALLCHILDREN,
            );
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
        WM_DESTROY => {
            clear_accessible_names(hwnd);
            invalidate_edit_style();
            invalidate_rail_style();
            inner.closing = true;
            inner.alive.store(false, Ordering::SeqCst);
            stop_checking_animation(inner);
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
            if !inner.hint_font.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(inner.hint_font.0));
                inner.hint_font = HFONT(std::ptr::null_mut());
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
                // #876: a key still being checked that the close did not save
                // (a plain destroy, or its answer was already on the way) is
                // not written, and that is told like any other row left unsaved.
                for vendor in inner.key_checks.take_pending() {
                    if let Some(field) = key_field_of(vendor) {
                        inner.tracker.fail(field, commit::KEY_UNFINISHED);
                    }
                }
                // A row still failed is told once, as a card (rule 7): inline
                // is all a window being typed in can show.
                tell_owner_left(inner.owner, inner.tracker.failed_count());
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
    // #585: a key typed but not applied yet still fetches its model list once
    // the user leaves the field. Leaving it applies it too (#875).
    if let Some(index) = typed_key_vendor(id, notify) {
        // A box the window lost because it was deactivated (alt-tab) keeps its
        // text: a half-typed key must not be stored. Enter and moving to another
        // control of Settings apply it.
        // No early model-list fetch here: the deferred commit's key check brings
        // the list, so the key is not sent twice.
        if inner.deactivated {
            inner.deferred_key = commit_trigger(id, notify);
            return;
        }
        if let Some(field) = commit_trigger(id, notify) {
            commit_field(inner, field);
        }
        // A key sent to its vendor to be checked brings the model list with the
        // answer: no second request.
        if !inner.key_checks.is_pending(index) {
            fetch_for_typed_key(inner, index);
        }
        return;
    }
    // #1143: a combo whose list opens is no longer under the pointer.
    if notify == windows::Win32::UI::WindowsAndMessaging::CBN_DROPDOWN {
        if let Some(combo) = get_dlg_item(inner.hwnd, id) {
            set_combo_hover(combo, false);
        }
    }
    match id {
        ID_UNDO if notify == BN_CLICKED => undo_last(inner),
        // Enter in a box (#875): apply it.
        ID_APPLY_FOCUSED => commit_focused(inner),
        ID_ESCAPE => escape_pressed(inner),
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
        ID_OPEN_ACTIONS if notify == BN_CLICKED => start_actions_job(inner, ActionsJob::Open),
        ID_IMPORT_ACTION if notify == BN_CLICKED => {
            send_command(inner, crate::app::SettingsCommand::ImportAction)
        }
        ID_EXPORT_ACTION if notify == BN_CLICKED => {
            // Like the Pause button: a pending Change would eat the menu's keys.
            if inner.learning.is_some() {
                stop_learning(inner);
                set_hotkey_status(inner.hwnd, "Cancelled. The shortcut is unchanged.");
            }
            start_actions_job(inner, ActionsJob::Export)
        }
        ID_PULL_BUTTON if notify == BN_CLICKED => on_pull_button(inner),
        ID_RESET_PROMPT => {
            set_text(inner.prompt_edit, &prompt_to_edit(DEFAULT_PROMPT));
        }
        ID_ACTIVE_PROVIDER if notify == CBN_SELCHANGE => unsafe { sync_models_flow(inner) },
        ID_ADD_PROVIDER if notify == BN_CLICKED => unsafe { reveal_providers(inner) },
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
    // #875: a change in a control is applied at once, after whatever the control
    // does for the window itself (the arms above).
    if let Some(field) = commit_trigger(id, notify) {
        commit_field(inner, field);
    } else if id == ID_RESET_PROMPT && notify == BN_CLICKED {
        commit_field(inner, Field::Prompt);
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

/// The Settings window's title (#828).
const SETTINGS_TITLE: &str = "Wingman Settings";

/// #828: the text scale as a percent ("125%"), the way Windows words it.
fn text_scale_label(scale: f32) -> String {
    format!("{}%", (clamp_text_scale(scale) * 100.0).round() as i32)
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
    set_text(label, &text_scale_label(scale));
}

// ---------------------------------------------------------------------------
// Instant apply (#875)
// ---------------------------------------------------------------------------
// A change in the window is one field. `commit_field` reads the form, asks
// `settings_commit::plan` what that field changed, and sends it through the sink
// (the owner window, then the writer thread). Nothing here reads or writes the
// file: the answer comes back through `note_write_result`.

/// What a control edits, or `None` for one that is not a setting (a label, a
/// button, the rail). The three shortcuts are changed by their Change buttons,
/// which start a capture rather than apply anything, so they are not here.
fn field_of_control(id: i32) -> Option<Field> {
    Some(match id {
        ID_ACTIVE_PROVIDER => Field::ActiveProvider,
        ID_OPENAI_KEY => Field::OpenAiKey,
        ID_OPENAI_MODEL => Field::OpenAiModel,
        ID_OPENAI_EFFORT => Field::OpenAiEffort,
        ID_ANTHROPIC_KEY => Field::AnthropicKey,
        ID_ANTHROPIC_MODEL => Field::AnthropicModel,
        ID_ANTHROPIC_EFFORT => Field::AnthropicEffort,
        ID_GEMINI_KEY => Field::GeminiKey,
        ID_GEMINI_MODEL => Field::GeminiModel,
        ID_GEMINI_EFFORT => Field::GeminiEffort,
        ID_OLLAMA_MODEL => Field::OllamaModel,
        ID_KEY_MODE => Field::KeyMode,
        ID_DOUBLE_PRESS => Field::DoublePressMs,
        ID_CLICK_AWAY => Field::ClickAway,
        ID_AUTOSTART => Field::Autostart,
        ID_CARD_SECONDS => Field::CardSeconds,
        ID_TEXT_SCALE_TRACK => Field::TextScale,
        ID_CARD_CORNER => Field::CardCorner,
        ID_CARD_MONITOR => Field::CardMonitor,
        ID_MAX_EDGE => Field::MaxEdge,
        ID_MONITOR => Field::Monitor,
        ID_REDACT => Field::Redact,
        ID_REQUIRE_TICK_FOR => Field::RequireTickFor,
        ID_PROMPT_EDIT => Field::Prompt,
        _ => return None,
    })
}

/// The control that shows `field`. For a shortcut that is its Change button.
fn control_of_field(field: Field) -> i32 {
    match field {
        Field::ActiveProvider => ID_ACTIVE_PROVIDER,
        Field::OpenAiKey => ID_OPENAI_KEY,
        Field::OpenAiModel => ID_OPENAI_MODEL,
        Field::OpenAiEffort => ID_OPENAI_EFFORT,
        Field::AnthropicKey => ID_ANTHROPIC_KEY,
        Field::AnthropicModel => ID_ANTHROPIC_MODEL,
        Field::AnthropicEffort => ID_ANTHROPIC_EFFORT,
        Field::GeminiKey => ID_GEMINI_KEY,
        Field::GeminiModel => ID_GEMINI_MODEL,
        Field::GeminiEffort => ID_GEMINI_EFFORT,
        Field::OllamaModel => ID_OLLAMA_MODEL,
        Field::PrimaryKey => ID_CHANGE_PRIMARY,
        Field::SecondaryKey => ID_CHANGE_SECONDARY,
        Field::PauseKey => ID_CHANGE_PAUSE,
        Field::KeyMode => ID_KEY_MODE,
        Field::DoublePressMs => ID_DOUBLE_PRESS,
        Field::ClickAway => ID_CLICK_AWAY,
        Field::Autostart => ID_AUTOSTART,
        Field::CardSeconds => ID_CARD_SECONDS,
        Field::TextScale => ID_TEXT_SCALE_TRACK,
        Field::CardCorner => ID_CARD_CORNER,
        Field::CardMonitor => ID_CARD_MONITOR,
        Field::MaxEdge => ID_MAX_EDGE,
        Field::Monitor => ID_MONITOR,
        Field::Redact => ID_REDACT,
        Field::RequireTickFor => ID_REQUIRE_TICK_FOR,
        Field::Prompt => ID_PROMPT_EDIT,
    }
}

/// When a field is applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Trigger {
    /// A list pick (`CBN_SELCHANGE`).
    Select,
    /// A checkbox click (`BN_CLICKED`).
    Click,
    /// A box left (`EN_KILLFOCUS`) or Enter in it.
    Leave,
    /// The slider let go (`TB_ENDTRACK`).
    Release,
    /// A learned chord (`handle_learned`).
    Capture,
}

fn trigger_of(field: Field) -> Trigger {
    match field {
        _ if field.is_text_box() => Trigger::Leave,
        Field::Autostart | Field::Redact | Field::ClickAway => Trigger::Click,
        Field::TextScale => Trigger::Release,
        Field::PrimaryKey | Field::SecondaryKey | Field::PauseKey => Trigger::Capture,
        _ => Trigger::Select,
    }
}

/// The field a control's notification applies, if it is the kind that applies it.
/// Never a keystroke: a box applies when it is left or Enter is pressed, so a
/// half-typed number is never written.
fn commit_trigger(id: i32, notify: u32) -> Option<Field> {
    let field = field_of_control(id)?;
    let fires = match trigger_of(field) {
        Trigger::Select => notify == CBN_SELCHANGE,
        Trigger::Click => notify == BN_CLICKED,
        Trigger::Leave => notify == EN_KILLFOCUS,
        Trigger::Release | Trigger::Capture => false,
    };
    fires.then_some(field)
}

/// Applies the box that has the keyboard focus, if it is a setting: Enter, and
/// closing the window.
fn commit_focused(inner: &mut SettingsInner) {
    let focus = unsafe { GetFocus() };
    if focus.0.is_null() {
        return;
    }
    let id = unsafe { GetDlgCtrlID(focus) };
    if let Some(field) = field_of_control(id) {
        commit_field(inner, field);
    }
}

/// The text a field is validated on, from the form.
fn raw_text(raw: &RawForm, field: Field) -> &str {
    match field {
        Field::CardSeconds => &raw.card_seconds_text,
        Field::DoublePressMs => &raw.double_press_text,
        _ => "",
    }
}

/// Why a typed API key is not stored, or `None`: it has the wrong shape for its
/// vendor (#678). Only a key that differs from the applied one is checked, so the
/// mask, the unreadable-key placeholder and a cleared box are left alone. What
/// the vendor says about a well-shaped key is `start_key_check`'s.
fn key_refusal(applied: &Config, field: Field, raw: &RawForm) -> Option<&'static str> {
    let (index, text, applied_key, why) = match field {
        Field::OpenAiKey => (
            0,
            &raw.openai_key,
            &applied.providers.openai.api_key,
            "That does not look like an OpenAI key. It starts with sk-.",
        ),
        Field::AnthropicKey => (
            1,
            &raw.anthropic_key,
            &applied.providers.anthropic.api_key,
            "That does not look like an Anthropic key. It starts with sk-ant-.",
        ),
        Field::GeminiKey => (
            2,
            &raw.gemini_key,
            &applied.providers.gemini.api_key,
            "That does not look like a Gemini key. It starts with AIza.",
        ),
        _ => return None,
    };
    let key = resolve_key_field(applied_key, text);
    if key == *applied_key || key.trim().is_empty() || key_fits_vendor(index, &key) {
        return None;
    }
    Some(why)
}

/// Puts the status line's text up, and shows or hides Undo with it.
fn set_status(inner: &mut SettingsInner, text: &str, undo: bool) {
    say(inner.hwnd, ID_VALIDATION_MESSAGE, text);
    set_undo_shown(inner, undo);
}

/// Sets the words of a status line (the footer's, or the shortcut line) and
/// raises a live region change on it, so a screen reader announces "Saved:
/// Card corner" and every error without the focus moving (#781, #877). A
/// static is a plain, enabled control, which is what the event needs.
fn say(hwnd: HWND, id: i32, text: &str) {
    use windows::Win32::UI::Accessibility::NotifyWinEvent;
    use windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_LIVEREGIONCHANGED;
    const OBJID_CLIENT: i32 = -4;
    let Some(line) = get_dlg_item(hwnd, id) else {
        return;
    };
    set_text(line, text);
    unsafe { NotifyWinEvent(EVENT_OBJECT_LIVEREGIONCHANGED, line, OBJID_CLIENT, 0) };
}

fn set_undo_shown(inner: &mut SettingsInner, shown: bool) {
    if inner.undo_shown == shown {
        return;
    }
    inner.undo_shown = shown;
    let Some(button) = get_dlg_item(inner.hwnd, ID_UNDO) else {
        return;
    };
    unsafe {
        // A button that goes away with the keyboard focus on it would strand the
        // focus: the rail is always there.
        if !shown && GetFocus() == button && !inner.rail.0.is_null() {
            let _ = SetFocus(Some(inner.rail));
        }
        let _ = ShowWindow(button, if shown { SW_SHOW } else { SW_HIDE });
    }
}

/// Applies one field: what it changed goes to the writer, the window's picture
/// of what is applied moves on at once, and the answer decides the status line.
fn commit_field(inner: &mut SettingsInner, field: Field) {
    if inner.closing {
        return;
    }
    if field == Field::Autostart {
        commit_autostart(inner);
        return;
    }
    if inner.deferred_key == Some(field) {
        inner.deferred_key = None;
    }
    let raw = read_raw_form(inner);
    let refused = commit::invalid(field, raw_text(&raw, field))
        .or_else(|| key_refusal(&inner.original, field, &raw));
    if let Some(why) = refused {
        // Not written, and the text stays for fixing. The row is failed, so the
        // next commit's status line does not hide it and closing counts it. The
        // focus is left alone: moving it from here would send another box its
        // `EN_KILLFOCUS` in the middle of this one.
        inner.tracker.fail(field, why);
        set_status(inner, &commit::invalid_line(field.label(), why), false);
        return;
    }
    let mut note = None;
    // Pause is deliberately not consulted: this is a read-only GET of something
    // the user typed, like the model-list fetch Settings already makes.
    if let Some(index) = field.key_vendor() {
        if let Some(key) = typed_key_to_check(&inner.original, index, &raw) {
            match commit::key_step((inner.key_seams.mode)()) {
                commit::KeyStep::Check => {
                    start_key_check(inner, field, index, key);
                    return;
                }
                commit::KeyStep::SaveWithNote(text) => note = Some(text),
            }
        }
    }
    apply_field(inner, field, &raw, note);
}

/// The key typed in vendor `index`'s box when it is a new, non-empty one: the
/// only kind that is asked of its vendor. The mask, the unreadable-key
/// placeholder and a cleared box are not.
fn typed_key_to_check(applied: &Config, index: usize, raw: &RawForm) -> Option<String> {
    let (applied_key, typed) = match index {
        0 => (&applied.providers.openai.api_key, &raw.openai_key),
        1 => (&applied.providers.anthropic.api_key, &raw.anthropic_key),
        _ => (&applied.providers.gemini.api_key, &raw.gemini_key),
    };
    let key = resolve_key_field(applied_key, typed);
    let key = key.trim();
    (!key.is_empty() && key != applied_key.trim() && key != crate::config::UNREADABLE_KEY_MARKER)
        .then(|| key.to_string())
}

/// The key box of vendor `index`.
fn key_field_of(index: usize) -> Option<Field> {
    match index {
        0 => Some(Field::OpenAiKey),
        1 => Some(Field::AnthropicKey),
        2 => Some(Field::GeminiKey),
        _ => None,
    }
}

/// Asks the vendor about a typed key on a worker thread (the UI thread hosts the
/// keyboard hook and never waits on the network). The row says so; the answer
/// comes back as `WM_KEY_CHECKED`, and only then is the key written. Nothing
/// runs once the answer is in.
fn start_key_check(inner: &mut SettingsInner, field: Field, index: usize, key: String) {
    let Some(generation) = inner.key_checks.begin(index, &key) else {
        // This very key is already out for checking.
        return;
    };
    // A fresh attempt: an earlier refusal of this row no longer stands.
    inner.tracker.clear_failed(field);
    set_status(inner, commit::CHECKING_LINE, false);
    let vendor = CLOUD_VENDORS[index];
    let check = inner.key_seams.check;
    let fetch_all = inner.key_seams.fetch_all;
    // The list that comes with the answer replaces a slower list fetch's.
    let mine = inner.cloud_sync.generation[index].fetch_add(1, Ordering::SeqCst) + 1;
    let slots = inner.cloud_models.clone();
    let sync = inner.cloud_sync.clone();
    let alive = inner.alive.clone();
    let hwnd_raw = inner.hwnd.0 as isize;
    let owner_raw = inner.owner.0 as isize;
    let late_card = inner.key_seams.late_card;
    // This check's claim: the close and this worker settle its answer once.
    let claim = inner
        .key_checks
        .claim_of(index)
        .unwrap_or_else(commit::new_claim);
    let job = move || {
        use crate::provider::model_list::KeyCheck;
        let mut full_list_owed = false;
        let verdict = match check(vendor, &key) {
            KeyCheck::Accepted(ids, complete) => {
                // A first page that is not the whole list is not stored: the
                // dropdown would keep it, and the key would count as fetched.
                full_list_owed = !complete;
                if complete && store_fetched_list(&slots, &sync, index, mine, &key, ids) {
                    post_while_open(&alive, || unsafe {
                        let _ = PostMessageW(
                            Some(HWND(hwnd_raw as *mut c_void)),
                            WM_CLOUD_MODELS,
                            WPARAM(index),
                            LPARAM(0),
                        );
                    });
                }
                commit::KeyVerdict::Accepted
            }
            KeyCheck::Rejected => commit::KeyVerdict::Rejected,
            KeyCheck::Unchecked => {
                // No key text and no error text: the report is pasted into
                // public issues. The egress log has the failure.
                crate::diagnostics::note_warning(&format!(
                    "Key check for {} could not finish: saving the key unchecked",
                    vendor.tag()
                ));
                commit::KeyVerdict::Unchecked
            }
        };
        // #1130: the answer is the window's unless the close already saved
        // this very key unchecked; then a refusal is told in a card by the app.
        if commit::worker_claims(&claim) {
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd_raw as *mut c_void)),
                    WM_KEY_CHECKED,
                    WPARAM(index),
                    LPARAM(commit::encode_verdict(generation, verdict)),
                );
            });
        }
        if commit::late_refusal(verdict, commit::closed_saved(&claim)) {
            let (headline, detail) = commit::late_refusal_card(index);
            let _ = late_card(owner_raw, headline, detail);
        }
        // The verdict is out; now the rest of a list that goes on, which the
        // check did not wait for (#1140).
        if full_list_owed {
            if let Ok(ids) = fetch_all(vendor, &key) {
                if store_fetched_list(&slots, &sync, index, mine, &key, ids) {
                    post_while_open(&alive, || unsafe {
                        let _ = PostMessageW(
                            Some(HWND(hwnd_raw as *mut c_void)),
                            WM_CLOUD_MODELS,
                            WPARAM(index),
                            LPARAM(0),
                        );
                    });
                }
            }
        }
    };
    if (inner.key_seams.spawn)(Box::new(job)).is_err() {
        // No worker, so no answer would ever come and the row would say
        // "Checking key..." for good (#1140). Settle it as a check that could
        // not finish: the key is saved with the "not checked" note.
        crate::diagnostics::note_warning(&format!(
            "Key check for {} could not start: saving the key unchecked",
            vendor.tag()
        ));
        on_key_checked(inner, index, generation, commit::KeyVerdict::Unchecked);
    }
}

/// A vendor answered for a typed key. A refusal keeps the text for fixing and
/// fails the row; anything else writes the key through the same path an
/// unchecked field takes, with the status line saying how far the check got. An
/// answer for a key replaced or edited since is ignored.
fn on_key_checked(
    inner: &mut SettingsInner,
    index: usize,
    generation: u64,
    verdict: commit::KeyVerdict,
) {
    let Some(field) = key_field_of(index) else {
        return;
    };
    if inner.closing {
        return;
    }
    let raw = read_raw_form(inner);
    let typed = match index {
        0 => resolve_key_field(&inner.original.providers.openai.api_key, &raw.openai_key),
        1 => resolve_key_field(
            &inner.original.providers.anthropic.api_key,
            &raw.anthropic_key,
        ),
        _ => resolve_key_field(&inner.original.providers.gemini.api_key, &raw.gemini_key),
    };
    if !inner.key_checks.finish(index, generation, &typed) {
        // Replaced or retyped: the newer key has its own check, or none yet and
        // commits when it is left. Only the "Checking" line is taken down.
        if !inner.key_checks.is_pending(index) && status_is(inner.hwnd, commit::CHECKING_LINE) {
            set_status(inner, "", false);
        }
        return;
    }
    if commit::verdict_blocks(verdict) {
        let why = commit::rejected_reason(index);
        inner.tracker.fail(field, &why);
        set_status(inner, &commit::invalid_line(field.label(), &why), false);
        return;
    }
    apply_field(inner, field, &raw, commit::verdict_note(verdict));
}

/// #1130: closing while a typed key is still out for checking saves that key
/// unchecked, as a check that could not finish does (only a refusal blocks a
/// key), so it is not lost. The check's claim decides who tells the user what
/// the vendor says: the worker (a refusal after this is a card, `late_refusal`)
/// or the window (the answer was already posted).
fn save_checking_keys_unchecked(inner: &mut SettingsInner) {
    for (index, key, claim) in inner.key_checks.snapshot() {
        let Some(field) = key_field_of(index) else {
            continue;
        };
        // The worker answered already: its message is on the way to a closing
        // window, so the key is left to the destroy, which counts it as unsaved.
        if !commit::close_claims(&claim) {
            continue;
        }
        inner.key_checks.forget(index);
        // From here the worker sees the window is gone.
        inner.alive.store(false, Ordering::SeqCst);
        let mut raw = read_raw_form(inner);
        match index {
            0 => raw.openai_key = key,
            1 => raw.anthropic_key = key,
            _ => raw.gemini_key = key,
        }
        apply_field(
            inner,
            field,
            &raw,
            commit::verdict_note(commit::KeyVerdict::Unchecked),
        );
    }
}

/// Whether the status line shows exactly `text`.
fn status_is(hwnd: HWND, text: &str) -> bool {
    get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).is_some_and(|line| get_text(line) == text)
}

/// Writes what `field` changed: the part of a commit after the field is known to
/// be valid and, for a key, checked. `note` is what the status line says once the
/// write is saved, in place of "Saved: <field>".
fn apply_field(inner: &mut SettingsInner, field: Field, raw: &RawForm, note: Option<&'static str>) {
    let edited = finalize_config(inner, raw);
    let plan = match commit::plan(field, &inner.baseline, &inner.original, &edited) {
        Ok(Some(plan)) => plan,
        Ok(None) => {
            // Nothing changed: the value in the box is the applied one, so a
            // row that was failed for a bad entry is fine again. A box shows the
            // applied text again, so stray spaces or a blank do not linger.
            inner.tracker.clear_failed(field);
            if field.is_text_box() {
                let applied = inner.original.clone();
                show_field(inner, field, &applied);
            }
            return;
        }
        Err(why) => {
            fail_row(inner, field, &why);
            return;
        }
    };
    let sent = match &inner.save_sink {
        Some(sink) => sink(
            field.label(),
            plan.changes.clone(),
            &inner.original,
            &plan.original,
        ),
        None => Ok(()),
    };
    if let Err(reason) = sent {
        fail_row(inner, field, &reason);
        return;
    }
    inner.wrote = true;
    inner.tracker.begin(commit::InFlight {
        field,
        undoing: false,
        before_original: std::mem::replace(&mut inner.original, plan.original),
        before_baseline: std::mem::replace(&mut inner.baseline, plan.baseline),
    });
    inner.tracker.clear_failed(field);
    if field.key_vendor().is_some() {
        inner.key_notes.push(field, note);
    }
    // Undo covers the latest change, and never a key.
    inner.tracker.undo = (!plan.key_changed).then(|| commit::UndoSlot {
        field,
        what: commit::UndoWhat::Config {
            restore: plan.undo.clone(),
        },
    });
    set_undo_shown(inner, false);
    // The box shows what is applied: a key as its mask, a number as it parsed.
    if field.key_vendor().is_some() || field.is_text_box() {
        let applied = inner.original.clone();
        show_field(inner, field, &applied);
    }
    if field == Field::ActiveProvider {
        unsafe { sync_models_flow(inner) };
    }
}

/// Why a Start with Windows change failed (#777): the registry would not take it.
const AUTOSTART_FAILED: &str = "Windows did not allow the change.";

/// What a Start with Windows worker is asked to do (#1105).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AutostartJob {
    /// Only read the Run key (the window just opened, or a row is shown again).
    Read,
    /// The user clicked the box: make the key say this.
    Set(bool),
    /// Undo: put the key back to this.
    Undo(bool),
}

/// What the worker found. `now` is the key as it stands after the job, which is
/// what the checkbox shows whatever happened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct AutostartAnswer {
    was: bool,
    now: bool,
    failed: bool,
    changed: bool,
}

struct AutostartDone {
    job: AutostartJob,
    answer: AutostartAnswer,
}

/// The Run key behind two function pointers, so the tests put a fake there
/// (rule 9: no test touches the real key).
#[derive(Clone, Copy)]
struct AutostartSeams {
    is_enabled: fn() -> bool,
    set_enabled: fn(bool) -> anyhow::Result<()>,
}

impl AutostartSeams {
    #[cfg(not(test))]
    const REAL: AutostartSeams = AutostartSeams {
        is_enabled: crate::autostart::is_enabled,
        set_enabled: crate::autostart::set_enabled,
    };

    /// In a test build no window reaches the real Run key.
    #[cfg(test)]
    const REAL: AutostartSeams = AutostartSeams {
        is_enabled: || false,
        set_enabled: |_| Ok(()),
    };
}

type AutostartSlot = std::sync::Arc<std::sync::Mutex<Option<AutostartDone>>>;

/// The worker's whole job. A write that changes nothing is skipped. The answer
/// carries the key as it stands afterwards, read back, so a refused or half
/// applied write never leaves the box showing something untrue.
fn run_autostart(job: AutostartJob, seams: AutostartSeams) -> AutostartAnswer {
    let was = (seams.is_enabled)();
    let want = match job {
        AutostartJob::Read => None,
        AutostartJob::Set(want) | AutostartJob::Undo(want) => Some(want),
    };
    let Some(want) = want.filter(|w| *w != was) else {
        return AutostartAnswer {
            was,
            now: was,
            failed: false,
            changed: false,
        };
    };
    let wrote = (seams.set_enabled)(want);
    let now = (seams.is_enabled)();
    let failed = wrote.is_err() || now != want;
    AutostartAnswer {
        was,
        now,
        failed,
        changed: !failed,
    }
}

#[derive(Debug, PartialEq, Eq)]
enum AutostartVerdict {
    /// Nothing to say: a read, or a click that changed nothing.
    Quiet,
    Saved {
        was: bool,
    },
    Undone,
    Failed,
}

fn autostart_verdict(job: AutostartJob, answer: &AutostartAnswer) -> AutostartVerdict {
    match job {
        AutostartJob::Read => AutostartVerdict::Quiet,
        _ if answer.failed => AutostartVerdict::Failed,
        AutostartJob::Set(_) if answer.changed => AutostartVerdict::Saved { was: answer.was },
        AutostartJob::Undo(_) if answer.changed => AutostartVerdict::Undone,
        _ => AutostartVerdict::Quiet,
    }
}

/// Whether a job that arrives while another runs is remembered. Only a click
/// is: it reads the box when its turn comes. A read is dropped (the running
/// job's answer is as fresh), and it must never be run as a click, which
/// would commit a placeholder box.
fn queued_when_busy(job: AutostartJob) -> bool {
    matches!(job, AutostartJob::Set(_))
}

/// Runs a Start with Windows job on a short worker and posts the answer back
/// (#1105): the window's thread never touches the registry. One job runs at a
/// time; a click that comes meanwhile is looked at when the answer lands.
fn start_autostart(inner: &mut SettingsInner, job: AutostartJob) {
    if inner.closing {
        return;
    }
    if inner.autostart_busy {
        if queued_when_busy(job) {
            inner.autostart_again = true;
        }
        return;
    }
    inner.autostart_busy = true;
    let slot = inner.autostart_slot.clone();
    let alive = inner.alive.clone();
    let hwnd = inner.hwnd.0 as isize;
    let seams = inner.autostart_seams;
    let spawned = std::thread::Builder::new()
        .name("settings-autostart".into())
        .spawn(move || {
            let answer = run_autostart(job, seams);
            if let Ok(mut g) = slot.lock() {
                *g = Some(AutostartDone { job, answer });
            }
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_SETTINGS_AUTOSTART,
                    WPARAM(0),
                    LPARAM(0),
                );
            });
        });
    if spawned.is_err() {
        inner.autostart_busy = false;
        if job == AutostartJob::Read {
            // No answer will come: do not leave the box dead.
            if let Some(box_) = get_dlg_item(inner.hwnd, ID_AUTOSTART) {
                if !crate::portable::is_portable() {
                    unsafe {
                        let _ = EnableWindow(box_, true);
                    }
                }
            }
        } else {
            // Rule 7: not started is a failed row, not a silent no-op.
            inner.tracker.fail(Field::Autostart, AUTOSTART_FAILED);
            set_status(
                inner,
                &commit::failed_line(Field::Autostart.label(), AUTOSTART_FAILED),
                false,
            );
        }
    }
}

/// Start with Windows is the registry's, not the file's: the click is handed to
/// a worker, and the box shows what the key really says once it answers.
fn commit_autostart(inner: &mut SettingsInner) {
    let want = checkbox_checked(inner.hwnd, ID_AUTOSTART);
    start_autostart(inner, AutostartJob::Set(want));
}

/// The worker's answer: the box follows the key, and the status line says what
/// happened.
fn take_autostart(inner: &mut SettingsInner) {
    inner.autostart_busy = false;
    let Some(done) = inner.autostart_slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    if inner.closing {
        return;
    }
    // The first answer makes the box real: it was disabled until the Run key
    // was read (portable mode keeps it disabled for good).
    if !crate::portable::is_portable() {
        if let Some(box_) = get_dlg_item(inner.hwnd, ID_AUTOSTART) {
            unsafe {
                let _ = EnableWindow(box_, true);
            }
        }
    }
    // Clicked again while the job ran (this is the retry after a failed or
    // finished job): the box holds the newer wish, so it is not overwritten
    // with this answer, and that wish is sent now as a fresh job.
    if std::mem::take(&mut inner.autostart_again) {
        commit_autostart(inner);
    } else if let Some(box_) = get_dlg_item(inner.hwnd, ID_AUTOSTART) {
        set_checkbox(box_, done.answer.now);
    }
    let label = Field::Autostart.label();
    match autostart_verdict(done.job, &done.answer) {
        AutostartVerdict::Quiet => {}
        AutostartVerdict::Failed => {
            inner.tracker.fail(Field::Autostart, AUTOSTART_FAILED);
            set_status(inner, &commit::failed_line(label, AUTOSTART_FAILED), false);
        }
        AutostartVerdict::Saved { was } => {
            inner.tracker.clear_failed(Field::Autostart);
            inner.tracker.undo = Some(commit::UndoSlot {
                field: Field::Autostart,
                what: commit::UndoWhat::Autostart { was },
            });
            set_status(inner, &commit::saved_line(label), true);
        }
        AutostartVerdict::Undone => {
            set_status(inner, &commit::undone_line(label), false);
        }
    }
}

/// What reading the file at open found (#1105).
struct Opened {
    baseline: Option<Config>,
    mtime: Option<std::time::SystemTime>,
}

type OpenSlot = std::sync::Arc<std::sync::Mutex<Option<Opened>>>;

/// Whether what the open-time read found may still replace the baseline: only
/// while it is the truth. A write already moved the baseline, a re-read already
/// took the file, or the window is going away.
fn open_read_applies(
    wrote: bool,
    writes_in_flight: usize,
    file_already_read: bool,
    closing: bool,
) -> bool {
    !wrote && writes_in_flight == 0 && !file_already_read && !closing
}

/// Reads the file's baseline and modified time on a worker (#1105); the answer
/// is `WM_SETTINGS_OPENED`. When it cannot be used the window keeps the running
/// config as its baseline and the next activation re-reads the file.
fn start_open_read(inner: &mut SettingsInner) {
    let Some(path) = inner.file_path.clone() else {
        return;
    };
    let slot = inner.open_slot.clone();
    let alive = inner.alive.clone();
    let hwnd = inner.hwnd.0 as isize;
    let _ = std::thread::Builder::new()
        .name("settings-open-read".into())
        .spawn(move || {
            let opened = Opened {
                baseline: crate::config_writer::file_baseline(&path),
                mtime: crate::config::file_mtime(&path),
            };
            if let Ok(mut g) = slot.lock() {
                *g = Some(opened);
            }
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_SETTINGS_OPENED,
                    WPARAM(0),
                    LPARAM(0),
                );
            });
        });
}

fn take_open_read(inner: &mut SettingsInner) {
    let Some(found) = inner.open_slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    if !open_read_applies(
        inner.wrote,
        inner.tracker.writes_in_flight(),
        inner.file_mtime.is_some(),
        inner.closing,
    ) {
        return;
    }
    if let Some(file) = found.baseline {
        inner.baseline = file;
    }
    inner.file_mtime = found.mtime;
}

/// A field that could not be applied: the control goes back to what is applied,
/// the row is failed, and the status line says why.
fn fail_row(inner: &mut SettingsInner, field: Field, reason: &str) {
    inner.tracker.fail(field, reason);
    if inner
        .tracker
        .undo
        .as_ref()
        .is_some_and(|u| u.field == field)
    {
        inner.tracker.undo = None;
    }
    let applied = inner.original.clone();
    show_field(inner, field, &applied);
    set_status(inner, &commit::failed_line(field.label(), reason), false);
}

/// The app's answer for a write. Only a write this window sent counts; one from
/// an earlier window is left for the app to say.
struct WriteAnswer<'a> {
    label: &'a str,
    result: &'a Result<(), String>,
}

/// Tells the open window how a write it sent went. Returns whether a row took
/// it: `false` when no window is open or the write was not this window's, so the
/// app shows its own card instead (rule 7). UI thread only.
pub fn note_write_result(label: &str, result: &Result<(), String>) -> bool {
    let open = OPEN_HWND.load(Ordering::SeqCst);
    if open == 0 {
        return false;
    }
    let answer = WriteAnswer { label, result };
    unsafe {
        SendMessageW(
            HWND(open as *mut c_void),
            WM_SETTINGS_WRITE_RESULT,
            None,
            Some(LPARAM(&answer as *const WriteAnswer as isize)),
        )
        .0 != 0
    }
}

fn on_write_result(inner: &mut SettingsInner, answer: &WriteAnswer) -> bool {
    let Some(field) = Field::from_label(answer.label) else {
        return false;
    };
    let Some(entry) = inner.tracker.finish(field) else {
        return false;
    };
    // The note belongs to this write, saved or not.
    let note = inner.key_notes.take(field);
    match answer.result {
        Ok(()) => {
            inner.tracker.clear_failed(field);
            if entry.undoing {
                set_status(inner, &commit::undone_line(field.label()), false);
            } else {
                // Undo is offered only while this is still the latest change.
                let latest = inner
                    .tracker
                    .undo
                    .as_ref()
                    .is_some_and(|u| u.field == field)
                    && inner.tracker.writes_in_flight() == 0;
                let line = note.map_or_else(|| commit::saved_line(field.label()), str::to_string);
                set_status(inner, &line, latest);
            }
        }
        Err(reason) => {
            // The file still holds the old value: so does the window again,
            // unless a newer write of the field is waiting. Then the window
            // keeps the newer value, which may still save.
            if !inner.tracker.settle_failed(&entry) {
                if let Ok(back) = commit::copy_field(field, &entry.before_original, &inner.original)
                {
                    inner.original = back;
                }
                if let Ok(back) = commit::copy_field(field, &entry.before_baseline, &inner.baseline)
                {
                    inner.baseline = back;
                }
            }
            let shown = commit::failure_reason(entry.undoing, reason);
            fail_row(inner, field, &shown);
        }
    }
    true
}

/// Undo: the latest change goes back through the same writer, and the control
/// shows the old value at once.
fn undo_last(inner: &mut SettingsInner) {
    // A Start with Windows job is still running: its answer comes first, and
    // Undo stays on offer until then.
    if inner.autostart_busy
        && inner
            .tracker
            .undo
            .as_ref()
            .is_some_and(|u| u.field == Field::Autostart)
    {
        return;
    }
    let Some(slot) = inner.tracker.undo.take() else {
        set_undo_shown(inner, false);
        return;
    };
    set_undo_shown(inner, false);
    let field = slot.field;
    match slot.what {
        commit::UndoWhat::Autostart { was } => {
            start_autostart(inner, AutostartJob::Undo(was));
        }
        commit::UndoWhat::Config { restore } => {
            let restored = crate::config_writer::reapply(&inner.original, &restore)
                .map_err(|e| format!("{e:#}"));
            let baseline = crate::config_writer::reapply(&inner.baseline, &restore)
                .map_err(|e| format!("{e:#}"));
            let (Ok(restored), Ok(baseline)) = (restored, baseline) else {
                fail_row(inner, field, "The old value could not be put back.");
                return;
            };
            let sent = match &inner.save_sink {
                Some(sink) => sink(field.label(), restore, &inner.original, &restored),
                None => Ok(()),
            };
            if let Err(reason) = sent {
                fail_row(inner, field, &reason);
                return;
            }
            inner.wrote = true;
            inner.tracker.begin(commit::InFlight {
                field,
                undoing: true,
                before_original: std::mem::replace(&mut inner.original, restored),
                before_baseline: std::mem::replace(&mut inner.baseline, baseline),
            });
            let shown = inner.original.clone();
            show_field(inner, field, &shown);
        }
    }
}

/// Whether a text box holds something other than what is applied (so Esc has
/// something to put back).
fn text_pending(field: Field, text: &str, applied: &Config) -> bool {
    match field {
        Field::CardSeconds => text.trim() != applied.ui.card_seconds.to_string(),
        Field::DoublePressMs => text.trim() != applied.hotkeys.double_press_ms.to_string(),
        Field::Prompt => prompt_from_edit(text) != prompt_from_edit(&applied.ui.prompt),
        Field::OpenAiKey => text != key_field_display(&applied.providers.openai.api_key),
        Field::AnthropicKey => text != key_field_display(&applied.providers.anthropic.api_key),
        Field::GeminiKey => text != key_field_display(&applied.providers.gemini.api_key),
        _ => false,
    }
}

/// Esc: with text typed in a box and not applied, puts the applied text back and
/// stays; otherwise closes the window.
fn escape_pressed(inner: &mut SettingsInner) {
    escape_with(inner, unsafe { GetFocus() });
}

/// [`escape_pressed`] with the window that has the focus given.
fn escape_with(inner: &mut SettingsInner, focus: HWND) {
    let id = if focus.0.is_null() {
        0
    } else {
        unsafe { GetDlgCtrlID(focus) }
    };
    if let Some(field) = field_of_control(id).filter(|f| f.is_text_box()) {
        if text_pending(field, &get_text(focus), &inner.original) {
            let applied = inner.original.clone();
            show_field(inner, field, &applied);
            // The bad or half-typed text is gone, so a row failed for it is fine.
            if inner.tracker.failed(field).is_some() {
                inner.tracker.clear_failed(field);
                set_status(inner, "", false);
            }
            if inner.deferred_key == Some(field) {
                inner.deferred_key = None;
            }
            return;
        }
    }
    unsafe {
        let _ = PostMessageW(Some(inner.hwnd), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

const CB_FINDSTRINGEXACT: u32 = 0x0158;
const CB_GETCOUNT: u32 = 0x0146;
const CB_GETLBTEXT: u32 = 0x0148;
const CB_GETLBTEXTLEN: u32 = 0x0149;
const CB_INSERTSTRING: u32 = 0x014A;

/// Selects the item that is exactly `text`, adding it first when the list does
/// not have it (a value written by hand that is not one of the presets).
fn combo_show(hwnd: HWND, id: i32, text: &str) {
    let Some(combo) = get_dlg_item(hwnd, id) else {
        return;
    };
    let w = wide_z(text);
    unsafe {
        let at = SendMessageW(
            combo,
            CB_FINDSTRINGEXACT,
            Some(WPARAM(usize::MAX)),
            Some(LPARAM(w.as_ptr() as isize)),
        )
        .0;
        if at >= 0 {
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(at as usize)), None);
        } else {
            combo_add(combo, text);
            let count = SendMessageW(combo, CB_GETCOUNT, None, None).0;
            SendMessageW(
                combo,
                CB_SETCURSEL,
                Some(WPARAM((count - 1).max(0) as usize)),
                None,
            );
        }
    }
}

/// Selects the Ollama model `model` in its box, whose items may carry a
/// "(vision)" badge; adds it at the top, unbadged, when it is not pulled.
fn combo_show_model(hwnd: HWND, id: i32, model: &str) {
    let Some(combo) = get_dlg_item(hwnd, id) else {
        return;
    };
    unsafe {
        let count = SendMessageW(combo, CB_GETCOUNT, None, None).0.max(0) as usize;
        for i in 0..count {
            let len = SendMessageW(combo, CB_GETLBTEXTLEN, Some(WPARAM(i)), None).0;
            if len < 0 {
                continue;
            }
            let mut buf = vec![0u16; len as usize + 1];
            SendMessageW(
                combo,
                CB_GETLBTEXT,
                Some(WPARAM(i)),
                Some(LPARAM(buf.as_mut_ptr() as isize)),
            );
            let item = String::from_utf16_lossy(&buf[..len as usize]);
            if strip_vision_badge(&item) == model {
                SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(i)), None);
                return;
            }
        }
        let w = wide_z(model);
        SendMessageW(
            combo,
            CB_INSERTSTRING,
            Some(WPARAM(0)),
            Some(LPARAM(w.as_ptr() as isize)),
        );
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), None);
    }
}

/// Shows `field` as `cfg` has it: the way a failed write puts a control back, a
/// hand edit reaches a row, and Undo shows the old value. No notification is
/// sent by any of these calls, so nothing echoes back as a new change.
fn show_field(inner: &mut SettingsInner, field: Field, cfg: &Config) {
    let hwnd = inner.hwnd;
    let p = &cfg.providers;
    match field {
        Field::ActiveProvider => {
            if let Some(combo) = get_dlg_item(hwnd, ID_ACTIVE_PROVIDER) {
                combo_select_index(combo, combo_index_for_order(&p.order));
            }
            unsafe { sync_models_flow(inner) };
        }
        Field::OpenAiKey => set_key_box(hwnd, ID_OPENAI_KEY, &p.openai.api_key),
        Field::AnthropicKey => set_key_box(hwnd, ID_ANTHROPIC_KEY, &p.anthropic.api_key),
        Field::GeminiKey => set_key_box(hwnd, ID_GEMINI_KEY, &p.gemini.api_key),
        Field::OpenAiModel => combo_show(hwnd, ID_OPENAI_MODEL, &p.openai.model),
        Field::AnthropicModel => combo_show(hwnd, ID_ANTHROPIC_MODEL, &p.anthropic.model),
        Field::GeminiModel => {
            combo_show(hwnd, ID_GEMINI_MODEL, &p.gemini.model);
            sync_gemini_thinking_enabled(hwnd);
        }
        Field::OpenAiEffort => {
            combo_show(hwnd, ID_OPENAI_EFFORT, &effort_display(&p.openai.effort))
        }
        Field::AnthropicEffort => combo_show(
            hwnd,
            ID_ANTHROPIC_EFFORT,
            &effort_display(&p.anthropic.effort),
        ),
        Field::GeminiEffort => {
            combo_show(hwnd, ID_GEMINI_EFFORT, &effort_display(&p.gemini.effort))
        }
        Field::OllamaModel => combo_show_model(hwnd, ID_OLLAMA_MODEL, &p.ollama.model),
        Field::PrimaryKey => {
            inner.hotkeys.primary = cfg.hotkeys.primary;
            set_hotkey_labels(hwnd, &inner.hotkeys);
        }
        Field::SecondaryKey => {
            inner.hotkeys.secondary = cfg.hotkeys.secondary;
            set_hotkey_labels(hwnd, &inner.hotkeys);
        }
        Field::PauseKey => {
            inner.hotkeys.pause = cfg.hotkeys.pause;
            set_hotkey_labels(hwnd, &inner.hotkeys);
        }
        Field::KeyMode => combo_show(
            hwnd,
            ID_KEY_MODE,
            key_mode_label(cfg.hotkeys.key_mode_value()),
        ),
        Field::DoublePressMs => {
            if let Some(edit) = get_dlg_item(hwnd, ID_DOUBLE_PRESS) {
                set_text(edit, &cfg.hotkeys.double_press_ms.to_string());
            }
        }
        Field::ClickAway => {
            if let Some(box_) = get_dlg_item(hwnd, ID_CLICK_AWAY) {
                set_checkbox(box_, cfg.options.click_away_dismiss);
            }
        }
        Field::Autostart => start_autostart(inner, AutostartJob::Read),
        Field::CardSeconds => {
            if let Some(edit) = get_dlg_item(hwnd, ID_CARD_SECONDS) {
                set_text(edit, &cfg.ui.card_seconds.to_string());
            }
        }
        Field::TextScale => {
            if let Some(track) = get_dlg_item(hwnd, ID_TEXT_SCALE_TRACK) {
                let pos = (clamp_text_scale(cfg.ui.text_scale) * 100.0).round() as isize;
                unsafe {
                    SendMessageW(track, TBM_SETPOS, Some(WPARAM(1)), Some(LPARAM(pos)));
                }
            }
            update_text_scale_label(hwnd);
        }
        Field::CardCorner => {
            combo_show(hwnd, ID_CARD_CORNER, card_corner_label(cfg.ui.card_corner));
            sync_card_display_enabled(hwnd);
        }
        Field::CardMonitor => combo_show(
            hwnd,
            ID_CARD_MONITOR,
            &card_monitor_display(&cfg.ui.card_monitor, crate::ui::card::monitor_count()),
        ),
        Field::MaxEdge => combo_show(hwnd, ID_MAX_EDGE, &max_edge_display(cfg.capture.max_edge)),
        Field::Monitor => combo_show(hwnd, ID_MONITOR, &monitor_display(&cfg.capture.monitor)),
        Field::Redact => {
            if let Some(box_) = get_dlg_item(hwnd, ID_REDACT) {
                set_checkbox(box_, cfg.capture.redact);
            }
        }
        Field::RequireTickFor => combo_show(
            hwnd,
            ID_REQUIRE_TICK_FOR,
            require_tick_for_label(cfg.forms.require_tick_for),
        ),
        Field::Prompt => {
            if let Some(edit) = get_dlg_item(hwnd, ID_PROMPT_EDIT) {
                set_text(edit, &prompt_to_edit(&cfg.ui.prompt));
            }
        }
    }
}

fn set_key_box(hwnd: HWND, id: i32, key: &str) {
    if let Some(edit) = get_dlg_item(hwnd, id) {
        set_text(edit, &key_field_display(key));
    }
}

/// The card for `count` settings still failed when Settings closed (#875).
pub fn unsaved_card(count: usize) -> (String, String) {
    commit::unsaved_card(count)
}

/// Coming back to the window (#875): read the file again if it moved since the
/// window last read it. The stat and the parse run on a worker; the answer is
/// `WM_SETTINGS_REFRESH`. Nothing runs while the window is idle.
fn start_refresh(inner: &mut SettingsInner) {
    let Some(path) = inner.file_path.clone() else {
        return;
    };
    if inner.refreshing || inner.closing {
        return;
    }
    inner.refreshing = true;
    let seen = inner.file_mtime;
    let slot = inner.refresh_slot.clone();
    let alive = inner.alive.clone();
    let hwnd = inner.hwnd.0 as isize;
    let spawned = std::thread::Builder::new()
        .name("settings-refresh".into())
        .spawn(move || {
            let now = crate::config::file_mtime(&path);
            let config = if now == seen {
                None
            } else {
                read_file_config(&path)
            };
            if let Ok(mut g) = slot.lock() {
                *g = Some(Refreshed { mtime: now, config });
            }
            post_while_open(&alive, || unsafe {
                let _ = PostMessageW(
                    Some(HWND(hwnd as *mut c_void)),
                    WM_SETTINGS_REFRESH,
                    WPARAM(0),
                    LPARAM(0),
                );
            });
        });
    if spawned.is_err() {
        inner.refreshing = false;
    }
}

/// The file as Wingman would load it, defaults for keys it does not spell out;
/// `None` when it is missing or does not parse, so a half-edited file never
/// turns the rows into defaults.
fn read_file_config(path: &std::path::Path) -> Option<Config> {
    let text = std::fs::read_to_string(path).ok()?;
    toml::from_str::<Config>(&text).ok()?;
    Some(crate::config_writer::baseline_from_text(&text))
}

fn take_refresh(inner: &mut SettingsInner) {
    inner.refreshing = false;
    let Some(found) = inner.refresh_slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    let Some(fresh) = found.config else {
        return;
    };
    // A write in flight is about to change the file: what was read is already
    // out of date, and the next activation reads again (the time is not taken).
    if inner.tracker.writes_in_flight() > 0 || inner.closing {
        return;
    }
    inner.file_mtime = found.mtime;
    adopt_file(inner, &fresh);
}

/// Shows a hand edit: every row whose value in `fresh` differs from what the
/// window has applied takes the file's value, unless it is being typed in. Keys
/// are never refreshed (they are not in the file), and nothing is written.
fn adopt_file(inner: &mut SettingsInner, fresh: &Config) {
    inner.baseline = fresh.clone();
    let focus = unsafe { GetFocus() };
    let focused = if focus.0.is_null() {
        0
    } else {
        unsafe { GetDlgCtrlID(focus) }
    };
    for field in Field::ALL {
        if field.key_vendor().is_some() || field == Field::Autostart {
            continue;
        }
        let capture = trigger_of(field) == Trigger::Capture;
        if capture && inner.learning.is_some() {
            continue;
        }
        let id = control_of_field(field);
        if focused == id && !capture {
            continue;
        }
        if field.is_text_box() {
            let typed = get_dlg_item(inner.hwnd, id)
                .map(get_text)
                .unwrap_or_default();
            if text_pending(field, &typed, &inner.original) {
                continue;
            }
        }
        if !commit::differs(field, &inner.original, fresh) {
            continue;
        }
        let Ok(next) = commit::copy_field(field, fresh, &inner.original) else {
            continue;
        };
        inner.original = next.clone();
        show_field(inner, field, &next);
        if inner
            .tracker
            .undo
            .as_ref()
            .is_some_and(|u| u.field == field)
        {
            inner.tracker.undo = None;
            set_undo_shown(inner, false);
        }
    }
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
const ID_AUTOSTART: i32 = 118;
/// Read-only status line combining #15's health check and #14's GPU/CPU
/// indicator (see [`ollama_status_line`]). Not a form field -- never read
/// back in `read_form`/`build_config`.
const ID_OLLAMA_STATUS: i32 = 120;
/// #344, #875: the status line in the footer: "Saved: Card corner", what is wrong
/// with a row, or why a change was not saved. Not a form field -- never read
/// back in `read_form`/`build_config`.
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
/// #874: "Add another provider", which shows the vendors that are hidden.
const ID_ADD_PROVIDER: i32 = 156;
/// #875: the Undo button in the footer, hidden until a change was saved.
const ID_UNDO: i32 = 157;
/// #875: two commands the window sends itself, not controls: apply the box that
/// has the focus (Enter) and the Esc key.
const ID_APPLY_FOCUSED: i32 = 158;
const ID_ESCAPE: i32 = 159;
/// A trackbar's "the user let go" notification code.
/// #877: the line of words under a row's title. Every description shares the
/// id, which no code reads back: it only says "paint me as a helper line".
const ID_ROW_DESC: i32 = 160;
/// #877: the hairline above a row. Shared the same way.
const ID_ROW_SEPARATOR: i32 = 161;
/// #1152: the Actions page's buttons. Each asks the app for what the tray's
/// item does (Open actions file has no tray item).
const ID_OPEN_ACTIONS: i32 = 162;
const ID_IMPORT_ACTION: i32 = 163;
const ID_EXPORT_ACTION: i32 = 164;
/// #393: the Wingman key's behaviour on a card (`hotkeys.key_mode`), its double
/// press time (`hotkeys.double_press_ms`) and `options.click_away_dismiss`, in
/// the key group on General. They apply at once like every other row.
const ID_KEY_MODE: i32 = 165;
const ID_DOUBLE_PRESS: i32 = 166;
const ID_CLICK_AWAY: i32 = 167;
const TB_ENDTRACK: u32 = 8;

/// One cloud vendor's three rows on the Models page (#877): the ids of its
/// controls and the titles, each carrying the vendor so a screen reader hears
/// "OpenAI model" and not a third "Model" (#781). The `&` letters are unique on
/// the page (a test checks).
struct CloudRows {
    slot: Slot,
    key_title: &'static str,
    model_title: &'static str,
    effort_title: &'static str,
    key_id: i32,
    show_id: i32,
    model_id: i32,
    effort_id: i32,
    /// The thinking row's title id, when something reads it back.
    effort_title_id: i32,
}

const CLOUD_ROWS: [CloudRows; 3] = [
    CloudRows {
        slot: Slot::OpenAi,
        key_title: "&OpenAI API key",
        model_title: "OpenAI &model",
        effort_title: "OpenAI &thinking time",
        key_id: ID_OPENAI_KEY,
        show_id: ID_OPENAI_SHOW_KEY,
        model_id: ID_OPENAI_MODEL,
        effort_id: ID_OPENAI_EFFORT,
        effort_title_id: 0,
    },
    CloudRows {
        slot: Slot::Anthropic,
        key_title: "Anth&ropic API key",
        model_title: "Anthropic mo&del",
        effort_title: "Anthropic thin&king time",
        key_id: ID_ANTHROPIC_KEY,
        show_id: ID_ANTHROPIC_SHOW_KEY,
        model_id: ID_ANTHROPIC_MODEL,
        effort_id: ID_ANTHROPIC_EFFORT,
        effort_title_id: 0,
    },
    CloudRows {
        slot: Slot::Gemini,
        key_title: "&Gemini API key",
        model_title: "Gemini mode&l",
        effort_title: "Gemini th&inking time",
        key_id: ID_GEMINI_KEY,
        show_id: ID_GEMINI_SHOW_KEY,
        model_id: ID_GEMINI_MODEL,
        effort_id: ID_GEMINI_EFFORT,
        effort_title_id: ID_GEMINI_EFFORT_LABEL,
    },
];

/// Buttons whose text alone repeats on a page get a longer accessible name, so
/// a screen reader says which one it is (#781, #877). Their text stays short,
/// as it sits in a narrow column. Everything else is named by its own text or,
/// for a box, by the title before it.
const ACCESSIBLE_NAMES: [(i32, &str); 9] = [
    (ID_OPEN_ACTIONS, "Open your actions file"),
    (ID_IMPORT_ACTION, "Import an action from a file"),
    (ID_EXPORT_ACTION, "Export one of your actions to a file"),
    (ID_OPENAI_SHOW_KEY, "Show OpenAI key"),
    (ID_ANTHROPIC_SHOW_KEY, "Show Anthropic key"),
    (ID_GEMINI_SHOW_KEY, "Show Gemini key"),
    (ID_CHANGE_PRIMARY, "Change Copilot key"),
    (ID_CHANGE_SECONDARY, "Change shortcut for any keyboard"),
    (ID_CHANGE_PAUSE, "Change pause shortcut"),
];

/// Gives the buttons of [`ACCESSIBLE_NAMES`] their names, through the system's
/// accessibility annotation service, which keeps the button's text and role.
/// Best effort: with no service the buttons keep their short text, and the
/// window works the same. Runs once as the window is built, never on a paint.
fn annotate_accessible_names(hwnd: HWND) {
    use windows::Win32::UI::Accessibility::PROPID_ACC_NAME;
    with_prop_service(|service| unsafe {
        for (id, name) in ACCESSIBLE_NAMES {
            if let Some(button) = get_dlg_item(hwnd, id) {
                let name = wide_z(name);
                let _ = service.SetHwndPropStr(
                    button,
                    ACC_OBJID_CLIENT,
                    0,
                    PROPID_ACC_NAME,
                    PCWSTR(name.as_ptr()),
                );
            }
        }
        // A screen reader announces a live region change only for a control
        // whose UIA LiveSetting is set, so the status lines say Polite.
        let polite = windows::Win32::System::Variant::VARIANT::from(
            windows::Win32::UI::Accessibility::Polite.0,
        );
        for id in LIVE_STATUS_IDS {
            if let Some(line) = get_dlg_item(hwnd, id) {
                let _ = service.SetHwndProp(
                    line,
                    ACC_OBJID_CLIENT,
                    0,
                    windows::Win32::UI::Accessibility::LiveSetting_Property_GUID,
                    &polite,
                );
            }
        }
    });
}

/// The status lines that announce what they say: the footer's and the
/// shortcut line's.
const LIVE_STATUS_IDS: [i32; 2] = [ID_VALIDATION_MESSAGE, ID_HOTKEY_STATUS];

/// Takes the names of [`annotate_accessible_names`] back as the window goes:
/// the service keeps them by window handle, and Windows recycles handles, so a
/// later control must not inherit "Show OpenAI key".
fn clear_accessible_names(hwnd: HWND) {
    use windows::Win32::UI::Accessibility::PROPID_ACC_NAME;
    with_prop_service(|service| unsafe {
        for (id, _) in ACCESSIBLE_NAMES {
            if let Some(button) = get_dlg_item(hwnd, id) {
                let _ = service.ClearHwndProps(button, ACC_OBJID_CLIENT, 0, &[PROPID_ACC_NAME]);
            }
        }
        for id in LIVE_STATUS_IDS {
            if let Some(line) = get_dlg_item(hwnd, id) {
                let _ = service.ClearHwndProps(
                    line,
                    ACC_OBJID_CLIENT,
                    0,
                    &[windows::Win32::UI::Accessibility::LiveSetting_Property_GUID],
                );
            }
        }
    });
}

const ACC_OBJID_CLIENT: u32 = 0xFFFF_FFFC;

/// Runs `f` with the accessibility annotation service, when there is one.
fn with_prop_service(f: impl FnOnce(&windows::Win32::UI::Accessibility::IAccPropServices)) {
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Accessibility::{CAccPropServices, IAccPropServices};
    unsafe {
        // Balanced below when it took: a thread that already has COM keeps it.
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        if let Ok(service) =
            CoCreateInstance::<_, IAccPropServices>(&CAccPropServices, None, CLSCTX_INPROC_SERVER)
        {
            f(&service);
        }
        if init.is_ok() {
            CoUninitialize();
        }
    }
}

/// The Mode row's words: where a screenshot can go in each mode.
const MODE_DESC: &str = "Cloud and Auto may send screenshots to a cloud model. Local and Offline keep everything on this PC.";
/// The Advanced page's heading.
const PROMPT_TITLE: &str = "Check my work prompt";
const PROMPT_NOTE: &str = "The instructions the model gets with every check.";

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
const ACTIONS_NOTE: &str = "Actions are what Wingman does once you confirm: reply to an email, fill in a form, add a calendar event. You can write your own, and share them as files.";

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
            ID_KEY_MODE,
            ID_DOUBLE_PRESS,
            ID_CLICK_AWAY,
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
            ID_ADD_PROVIDER,
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
    (
        Page::Actions,
        &[
            ID_ACTIONS_NOTE,
            ID_OPEN_ACTIONS,
            ID_IMPORT_ACTION,
            ID_EXPORT_ACTION,
        ],
    ),
    (
        Page::Advanced,
        &[ID_PROMPT_TOGGLE, ID_PROMPT_EDIT, ID_RESET_PROMPT],
    ),
];

/// The ids that belong to no page: the rail, the scrolling body that holds the
/// pages, the footer, and the two commands the window sends itself (no control).
#[cfg(test)]
const CHROME_IDS: &[i32] = &[
    ID_RAIL,
    ID_BODY,
    ID_UNDO,
    ID_VALIDATION_MESSAGE,
    ID_APPLY_FOCUSED,
    ID_ESCAPE,
    // Repeated on every page, so not one page's control (#877).
    ID_ROW_DESC,
    ID_ROW_SEPARATOR,
];

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
/// the other's handler (issue #144: `ID_AUTOSTART` and a Cancel button both being
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
    ("ID_AUTOSTART", ID_AUTOSTART),
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
    ("ID_OPEN_ACTIONS", ID_OPEN_ACTIONS),
    ("ID_IMPORT_ACTION", ID_IMPORT_ACTION),
    ("ID_EXPORT_ACTION", ID_EXPORT_ACTION),
    ("ID_MODE", ID_MODE),
    ("ID_PAUSE_STATUS", ID_PAUSE_STATUS),
    ("ID_PAUSE_BUTTON", ID_PAUSE_BUTTON),
    ("ID_RESUME_BUTTON", ID_RESUME_BUTTON),
    ("ID_PAUSE_LABEL", ID_PAUSE_LABEL),
    ("ID_CHANGE_PAUSE", ID_CHANGE_PAUSE),
    ("ID_ADD_PROVIDER", ID_ADD_PROVIDER),
    ("ID_UNDO", ID_UNDO),
    ("ID_APPLY_FOCUSED", ID_APPLY_FOCUSED),
    ("ID_ESCAPE", ID_ESCAPE),
    ("ID_ROW_DESC", ID_ROW_DESC),
    ("ID_ROW_SEPARATOR", ID_ROW_SEPARATOR),
    ("ID_KEY_MODE", ID_KEY_MODE),
    ("ID_DOUBLE_PRESS", ID_DOUBLE_PRESS),
    ("ID_CLICK_AWAY", ID_CLICK_AWAY),
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
const CBS_OWNERDRAWFIXED: i32 = 0x0010;
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
const BS_AUTOCHECKBOX: i32 = 0x0003;
const BS_PUSHBUTTON: i32 = 0x0000;
const TBS_HORZ: i32 = 0x0000;

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

/// The gap above and below the status line inside the footer, in dp (#875).
const STATUS_PAD_DP: i32 = 4;
/// Space kept below the last group inside the scrolling body, in dp.
const BODY_PAD_BOTTOM: i32 = MARGIN / 2;

const MARGIN: i32 = crate::ui::theme::MARGIN_DP;
/// Where a page's first row starts, in dp from the top of the body (#872):
/// level with the rail's first item.
const PAGE_TOP_PAD_DP: i32 = crate::ui::theme::SPACE_SM_DP;
/// The row measures (#877, #828): every one is a `theme.rs` token.
const ROW_H: i32 = crate::ui::theme::ROW_CONTROL_H_DP;
const ROW_PAD: i32 = crate::ui::theme::ROW_PAD_DP;
const ROW_TITLE_H: i32 = crate::ui::theme::ROW_TITLE_H_DP;
const DESC_LINE_H: i32 = crate::ui::theme::ROW_DESC_LINE_DP;
const CONTROL_W: i32 = crate::ui::theme::ROW_CONTROL_W_DP;
/// The gap between a row's text and its control column, and between two
/// controls side by side (half of it).
const COL_GAP: i32 = crate::ui::theme::SPACE_LG_DP;
/// A one-line row: the control with its padding above and below.
const ROW_PITCH: i32 = ROW_H + 2 * ROW_PAD;
/// The gap between the prompt box and the button under it.
const ROW_GAP: i32 = ROW_PAD;
/// The Advanced page's heading: the title and one line of what it is for.
const PROMPT_HEAD_H: i32 = 2 * ROW_PAD + ROW_TITLE_H + DESC_LINE_H;
const BTN_W: i32 = 100;
const BTN_H: i32 = 28;
/// The width of a "Show" key toggle, and of the text scale's value.
const SHOW_KEY_W: i32 = 62;
const TEXT_SCALE_LABEL_W: i32 = 58;
/// The Ollama status text (two lines) and the pull progress line.
const OLLAMA_STATUS_H: i32 = 2 * DESC_LINE_H + 4;
const PULL_STATUS_H: i32 = 2 * DESC_LINE_H;

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
thread_local! {
    /// The Settings window's real DPI while the gallery simulates another one.
    static NATIVE_DPI: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

#[cfg(any(test, debug_assertions))]
fn forced_dpi() -> Option<u32> {
    FORCED_DPI.with(std::cell::Cell::get)
}

fn to_px(dp: i32, dpi: u32) -> i32 {
    ((dp as i64 * dpi as i64 + 48) / 96) as i32
}

#[cfg(any(test, debug_assertions))]
thread_local! {
    /// The gallery's forced theme for this thread's Settings window, if any
    /// (#582). A forced theme ignores the system and bypasses the dark gate.
    static FORCED_THEME: std::cell::Cell<Option<crate::ui::theme::Theme>> =
        const { std::cell::Cell::new(None) };
}

/// Forces `theme` (or none) and drops the cached styles built under the old
/// one, so the next paint resolves again.
#[cfg(any(test, debug_assertions))]
fn force_theme(theme: Option<crate::ui::theme::Theme>) {
    FORCED_THEME.with(|f| f.set(theme));
    unsafe {
        invalidate_edit_style();
        invalidate_rail_style();
    }
}

/// What Settings draws with right now: the theme, the system colours that
/// theme reads (high contrast), and whether the gallery forced it. Resolved
/// once per change by [`edit_style`]; nothing else resolves the theme.
#[derive(Debug, Clone, Copy)]
struct Look {
    theme: crate::ui::theme::Theme,
    sys: crate::ui::theme::SysColors,
    forced: bool,
}

/// Resolves the look: the forced theme (the gallery, which draws high contrast
/// with Windows' black colours since it cannot switch the system), else the
/// detected one, with dark gated off for real users (see
/// `settings_theme::SETTINGS_DARK_ENABLED`).
fn current_look() -> Look {
    use crate::ui::theme::{detect_theme, SysColors};
    #[cfg(any(test, debug_assertions))]
    if let Some(theme) = FORCED_THEME.with(std::cell::Cell::get) {
        let sys = if theme == crate::ui::theme::Theme::HighContrast {
            crate::ui::theme::HC_BLACK
        } else {
            SysColors::read()
        };
        return Look {
            theme,
            sys,
            forced: true,
        };
    }
    Look {
        theme: crate::ui::settings_theme::gate(
            detect_theme(),
            crate::ui::settings_theme::SETTINGS_DARK_ENABLED,
        ),
        sys: SysColors::read(),
        forced: false,
    }
}

/// Sets the title bar to the theme (`DWMWA_USE_IMMERSIVE_DARK_MODE`, documented
/// since Windows 10 20H1). Left alone in high contrast, where the system draws
/// the caption. A failure leaves the title bar as it was: the body is still
/// right, so it fails safe. Light is only written back after a dark one was
/// applied, so a light-only run never touches the attribute.
unsafe fn apply_title_bar(hwnd: HWND) {
    use windows::core::BOOL;
    use windows::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
    thread_local! {
        static DARK_APPLIED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if let Some(dark) = crate::ui::settings_theme::title_bar_dark(edit_style().look.theme) {
        if !dark && !DARK_APPLIED.get() {
            return;
        }
        DARK_APPLIED.set(dark);
        let value = BOOL::from(dark);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &value as *const BOOL as *const c_void,
            std::mem::size_of::<BOOL>() as u32,
        );
    }
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
    let prompt_label_bottom = prompt_top + PROMPT_HEAD_H;

    // The smallest the Prompt group can be and still hold a usable edit box
    // and the reset button: label, minimum edit height, the gap before the
    // reset button, the reset button itself, and a bottom margin before the
    // group's own border.
    let min_prompt_group_h = PROMPT_HEAD_H + MIN_PROMPT_EDIT_H + ROW_GAP + reset_btn_h + MARGIN;

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
const COLLAPSED_PROMPT_GROUP_H: i32 = PROMPT_HEAD_H + BTN_H + MARGIN;

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
        reset_btn_y: content_top + PROMPT_HEAD_H,
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
    /// A hairline between rows (#877): one physical pixel thick at any scale.
    Hairline,
    PromptEdit,
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
        Anchor::Fixed | Anchor::Hairline => {}
        Anchor::PromptEdit => h += new.prompt_edit_h - old.prompt_edit_h,
        Anchor::ResetBtn => y += new.reset_btn_y - old.reset_btn_y,
        Anchor::Footer => y += new.buttons_y - old.buttons_y,
    }
    let h_px = if c.anchor == Anchor::Hairline {
        1
    } else {
        to_px(h, dpi)
    };
    (to_px(c.x, dpi), to_px(y, dpi), to_px(c.w, dpi), h_px)
}

/// `WM_DPICHANGED`: the window moved to a monitor with a different scale.
/// Takes the new DPI, honors the system's suggested rect, rebuilds the font
/// and re-lays-out every child at the new scale.
unsafe fn on_dpi_changed(inner: &mut SettingsInner, wparam: WPARAM, lparam: LPARAM) {
    inner.dpi = dpi_from_wparam(wparam.0);
    invalidate_rail_style();
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
    apply_layout(inner, true);
}

/// [`apply_dpi`], optionally keeping the current font. A layout change that
/// is not a DPI or font change (a provider switch, #874) passes `false`, so no
/// new `HFONT` is made for it.
unsafe fn apply_layout(inner: &mut SettingsInner, rebuild_font: bool) {
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

    let new_font = if rebuild_font || inner.font.0.is_null() {
        build_font(dpi)
    } else {
        inner.font
    };
    // #1015: the helper-line font follows the body font: kept when the body
    // font is kept, rebuilt when it is.
    let new_hint_font = if new_font == inner.font && !inner.hint_font.0.is_null() {
        inner.hint_font
    } else {
        build_hint_font(new_font, dpi)
    };
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
        if is_combo(c.hwnd) {
            set_combo_heights(c.hwnd, dpi);
        }
        let _ = SetWindowPos(c.hwnd, None, x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
        // #1015: helper lines are set smaller than the labels.
        let font = if !new_hint_font.0.is_null() && is_hint_id(GetDlgCtrlID(c.hwnd)) {
            new_hint_font
        } else {
            new_font
        };
        SendMessageW(
            c.hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    if have_body {
        set_body_scroll(inner.body, content_px, view_px, pos);
    }
    // Every child now uses the new font, so the old one can go.
    if new_font != inner.font && !inner.font.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(inner.font.0));
    }
    inner.font = new_font;
    if new_hint_font != inner.hint_font && !inner.hint_font.0.is_null() {
        let _ = DeleteObject(HGDIOBJ(inner.hint_font.0));
    }
    inner.hint_font = new_hint_font;
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
    } else if inner.page == Page::Models {
        inner.page_content_dp[Page::Models.index()] - inner.models_shrink_dp
    } else {
        inner.page_content_dp[inner.page.index()]
    }
}

/// The height of the fullest page in dp, with the prompt closed: what the
/// window is made tall enough for (#872).
fn tallest_page_dp(inner: &SettingsInner) -> i32 {
    Page::ALL
        .iter()
        .map(|p| page_max_dp(inner, *p))
        .max()
        .unwrap_or(0)
}

/// The tallest `page` gets in dp. The Models page is longest with every
/// provider showing, and then "Add another provider" has gone (#874).
fn page_max_dp(inner: &SettingsInner, page: Page) -> i32 {
    let full = inner.page_content_dp[page.index()];
    if page == Page::Models {
        full - ROW_PITCH
    } else {
        full
    }
}

/// Whether the child `c` should be visible: it is on the page that shows, and
/// the few controls with a state of their own are in the state that shows them
/// (the Ollama button only has something to offer sometimes, the prompt box is
/// folded away until asked for). The footer is on every page.
unsafe fn child_visible(inner: &SettingsInner, c: &ChildLayout) -> bool {
    let Some(page) = c.page else {
        // The footer is on every page; Undo shows only after a saved change.
        return GetDlgCtrlID(c.hwnd) != ID_UNDO || inner.undo_shown;
    };
    if page != inner.page {
        return false;
    }
    if page == Page::Models {
        if let Some(f) = inner.models_flow.iter().find(|f| f.hwnd == c.hwnd) {
            if !settings_models::slot_shown(f.slot, inner.models_shown, inner.models_add_shown) {
                return false;
            }
        }
    }
    match GetDlgCtrlID(c.hwnd) {
        ID_OLLAMA_LINK => inner.ollama_link_shown,
        ID_PROMPT_EDIT | ID_RESET_PROMPT => inner.prompt_expanded,
        _ => true,
    }
}

/// #874: works out which provider blocks the Models page shows (the active
/// one, any with a key typed or stored, everything once "Add another provider"
/// was pressed), moves the blocks under a hidden one up, and re-lays-out. The
/// hidden controls keep their text, so Save and a switch back lose nothing.
/// It reads two combo selections and a few text lengths: nothing slow, so it
/// is safe on the UI thread, and it only runs on a choice, never on a paint.
unsafe fn sync_models_flow(inner: &mut SettingsInner) {
    let hwnd = inner.hwnd;
    let active = get_dlg_item(hwnd, ID_ACTIVE_PROVIDER)
        .map(|c| SendMessageW(c, CB_GETCURSEL, None, None).0)
        .filter(|i| *i >= 0)
        .map(|i| ActiveProvider::from_index(i as usize));
    let typed = |id: i32| {
        get_dlg_item(hwnd, id)
            .is_some_and(|h| windows::Win32::UI::WindowsAndMessaging::GetWindowTextLengthW(h) > 0)
    };
    let configured = [
        typed(ID_OPENAI_KEY),
        typed(ID_ANTHROPIC_KEY),
        typed(ID_GEMINI_KEY),
        inner.original.providers.order.iter().any(|p| p == "ollama"),
    ];
    let shown = settings_models::shown_providers(active, configured, inner.models_revealed);
    let add = settings_models::add_button_shown(shown, inner.models_revealed);
    inner.models_shown = shown;
    inner.models_add_shown = add;
    let pitch = ROW_PITCH;
    for f in &inner.models_flow {
        let dy = settings_models::slot_dy(f.slot, shown, add, pitch);
        if let Some(c) = inner.children.iter_mut().find(|c| c.hwnd == f.hwnd) {
            c.y = f.y + dy;
        }
    }
    // A hidden Ollama card also takes its own height off the page: from the
    // hairline above its status to the bottom of its last line and the padding
    // under it, which the page's end counts like any row's.
    let card_top = inner
        .models_flow
        .iter()
        .filter(|f| f.slot == Slot::OllamaCard)
        .map(|f| f.y)
        .min()
        .unwrap_or(0);
    let card_bottom = inner
        .models_flow
        .iter()
        .filter(|f| f.slot == Slot::OllamaCard)
        .map(|f| f.y + f.h)
        .max()
        .unwrap_or(0);
    let card_gone = if shown[3] {
        0
    } else {
        card_bottom + ROW_PAD - card_top
    };
    inner.models_shrink_dp = -settings_models::slot_dy(Slot::Frame, shown, add, pitch) + card_gone;
    if !inner.hwnd.0.is_null() {
        // One repaint for the whole change, as `show_page` does: the layout
        // pass below turns redraw back on and repaints.
        SendMessageW(inner.hwnd, WM_SETREDRAW, Some(WPARAM(0)), None);
    }
    sync_visibility(inner);
    if !inner.hwnd.0.is_null() {
        apply_layout(inner, false);
        // Focus never stays on a control that just went away.
        let focus = GetFocus();
        if !focus.0.is_null()
            && IsChild(inner.hwnd, focus).as_bool()
            && !IsWindowVisible(focus).as_bool()
        {
            if let Some(combo) = get_dlg_item(inner.hwnd, ID_ACTIVE_PROVIDER) {
                let _ = SetFocus(Some(combo));
            }
        }
    }
}

/// The control that takes focus when "Add another provider" shows the blocks
/// that were hidden `before`: the first field of the first one that appeared.
fn reveal_focus_id(before: [bool; 4], after: [bool; 4]) -> i32 {
    const FIRST_FIELD: [i32; 4] = [
        ID_OPENAI_KEY,
        ID_ANTHROPIC_KEY,
        ID_GEMINI_KEY,
        ID_OLLAMA_MODEL,
    ];
    settings_models::first_revealed(before, after)
        .map(|i| FIRST_FIELD[i])
        .unwrap_or(ID_ACTIVE_PROVIDER)
}

/// "Add another provider": shows every vendor and puts focus on the first one
/// that was hidden, since the button that had focus is gone.
unsafe fn reveal_providers(inner: &mut SettingsInner) {
    let before = inner.models_shown;
    inner.models_revealed = true;
    sync_models_flow(inner);
    let target = reveal_focus_id(before, inner.models_shown);
    if let Some(h) = get_dlg_item(inner.hwnd, target) {
        let _ = SetFocus(Some(h));
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
const CARD_SECONDS_LABEL: &str = "Hide the card &after";
const CARD_SECONDS_UNIT: &str = "seconds";
const CARD_SECONDS_NOTE: &str = "0 keeps the card open until you close it.";

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
        ID_HOTKEY_STATUS
            | ID_MODEL_LIST_HINT
            | ID_CAPTURE_DETAIL_HINT
            | ID_REDACT_HINT
            | ID_ROW_DESC
    )
}

/// The text colour of the helper lines (#824, #972). Secondary text, never the
/// disabled grey: the theme's `text_dim` token, and the system's own button
/// text in high contrast (where grey means disabled).
fn hint_text_color(
    theme: crate::ui::theme::Theme,
    tokens: &crate::ui::theme::Tokens,
    btn_text: u32,
) -> u32 {
    if theme == crate::ui::theme::Theme::HighContrast {
        btn_text
    } else {
        tokens.text_dim
    }
}

/// #934: the colours of a text box. Tokens in a normal theme, system colours in
/// high contrast. `theme` is the look's: light for a real window until dark
/// ships (#582).
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
    /// #1109: a hovered combo's border (the system highlight in high contrast).
    hover_border: u32,
    /// #1169: the row under the pointer or cursor in a dropped-open list (the
    /// system highlight in high contrast), and the ink on it.
    item_hover: u32,
    item_text_hot: u32,
}

/// How far a disabled box's fill moves from the text box fill toward the
/// face behind it, in percent.
const EDIT_DISABLED_FILL_PCT: u32 = 60;

fn edit_colors(
    theme: crate::ui::theme::Theme,
    sys: &crate::ui::theme::SysColors,
    accent: u32,
    face: u32,
) -> EditColors {
    use crate::ui::card_buttons::blend;
    use crate::ui::theme::{menu_hover_fill, menu_text_color, Theme, Tokens};
    let high_contrast = theme == Theme::HighContrast;
    let t = Tokens::for_theme(theme, sys);
    // Dark draws the control edge token (3:1); light keeps its hairline until
    // light moves onto the new paint (spec: no change to light in this slice).
    let edge = if theme == Theme::Dark {
        t.control_border
    } else {
        t.border
    };
    // A list's rows lie on the text box fill, so the hover tint is taken from it.
    let on_list = Tokens {
        surface: t.control_fill,
        ..t
    };
    let (disabled_fill, disabled_border) = if high_contrast {
        // No tints in high contrast: the window colour, and GrayText ink.
        (sys.window, sys.gray_text)
    } else {
        (blend(t.control_fill, face, EDIT_DISABLED_FILL_PCT), edge)
    };
    EditColors {
        fill: t.control_fill,
        border: edge,
        text: t.text,
        focus_line: if high_contrast { sys.highlight } else { accent },
        face,
        disabled_fill,
        disabled_border,
        disabled_text: menu_text_color(theme, &t, sys, false, true),
        hover_border: if high_contrast {
            sys.highlight
        } else {
            blend(edge, t.text, EDIT_HOVER_BORDER_PCT)
        },
        item_hover: menu_hover_fill(theme, &on_list, sys),
        item_text_hot: menu_text_color(theme, &on_list, sys, true, false),
    }
}

/// How far a hovered combo's border moves from the border colour toward the
/// text colour, in percent. The tint of the field is left out: the system draws
/// the arrow button, so a tinted field would sit beside a white button.
const EDIT_HOVER_BORDER_PCT: u32 = 30;

/// The colours the frame is drawn with: the border of a hovered box is firmer.
fn frame_colors(colors: &EditColors, hovered: bool) -> EditColors {
    EditColors {
        border: if hovered {
            colors.hover_border
        } else {
            colors.border
        },
        ..*colors
    }
}

#[cfg(test)]
thread_local! {
    /// How many times the colours were rebuilt (a registry read each) on
    /// this thread: painting must not move it.
    static EDIT_STYLE_BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
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

/// `WM_DWMCOLORIZATIONCOLORCHANGED`: the accent changed (#1055).
const WM_DWMCOLORIZATIONCOLORCHANGED_MSG: u32 = 0x0320;

/// Everything a text box paint needs, built once per theme change: the
/// colours and the two brushes `WM_CTLCOLOREDIT` / `WM_CTLCOLORSTATIC` hand
/// back. Owned here and freed by [`invalidate_edit_style`].
#[derive(Clone, Copy)]
struct EditStyle {
    /// #582: the one place the theme is resolved; the rail, the divider, the
    /// title bar and the colour messages all read it from here.
    look: Look,
    /// Whether Settings fills its own face (dark, or the gallery's forced high
    /// contrast). False means the system draws it, as before.
    paints_face: bool,
    /// The window face: the surface token where Settings paints it, else the
    /// system's.
    face: u32,
    face_brush: windows::Win32::Graphics::Gdi::HBRUSH,
    /// The helper lines' ink (#824, #972).
    hint_text: u32,
    colors: EditColors,
    brush: windows::Win32::Graphics::Gdi::HBRUSH,
    disabled_brush: windows::Win32::Graphics::Gdi::HBRUSH,
    /// The line between rows (#877): the border token, or the system's window
    /// text in high contrast.
    hairline_brush: windows::Win32::Graphics::Gdi::HBRUSH,
    /// The hovered row of a combo's drop list (#1169).
    item_hover_brush: windows::Win32::Graphics::Gdi::HBRUSH,
}

thread_local! {
    static EDIT_STYLE: std::cell::Cell<Option<EditStyle>> = const { std::cell::Cell::new(None) };
}

/// The cached style, built on first use after a theme change. Painting and
/// colour messages run on the UI thread, so they never reach the registry.
unsafe fn edit_style() -> EditStyle {
    use crate::ui::settings_theme::{face_color, paints_face};
    use crate::ui::theme::{Theme, Tokens};
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, GetSysColor, COLOR_3DFACE, COLOR_BTNTEXT,
    };
    if let Some(style) = EDIT_STYLE.get() {
        return style;
    }
    #[cfg(test)]
    EDIT_STYLE_BUILDS.set(EDIT_STYLE_BUILDS.get() + 1);
    let look = current_look();
    let tokens = Tokens::for_theme(look.theme, &look.sys);
    let face = face_color(look.theme, look.forced, &tokens, GetSysColor(COLOR_3DFACE));
    let accent = if look.theme == Theme::HighContrast {
        face
    } else {
        crate::ui::card_buttons::system_accent(look.theme, tokens.surface)
    };
    let colors = edit_colors(look.theme, &look.sys, accent, face);
    let btn_text = if look.forced {
        look.sys.window_text
    } else {
        GetSysColor(COLOR_BTNTEXT)
    };
    let solid = |c: u32| CreateSolidBrush(windows::Win32::Foundation::COLORREF(c));
    let style = EditStyle {
        look,
        paints_face: paints_face(look.theme, look.forced),
        face,
        face_brush: solid(face),
        hint_text: hint_text_color(look.theme, &tokens, btn_text),
        colors,
        brush: solid(colors.fill),
        disabled_brush: solid(colors.disabled_fill),
        hairline_brush: solid(colors.border),
        item_hover_brush: solid(colors.item_hover),
    };
    EDIT_STYLE.set(Some(style));
    style
}

/// Drops the cached style and its brushes; the next paint rebuilds it. Called
/// on a theme, accent or system colour change, and when Settings closes.
unsafe fn invalidate_edit_style() {
    if let Some(style) = EDIT_STYLE.take() {
        let _ = DeleteObject(HGDIOBJ(style.face_brush.0));
        let _ = DeleteObject(HGDIOBJ(style.brush.0));
        let _ = DeleteObject(HGDIOBJ(style.disabled_brush.0));
        let _ = DeleteObject(HGDIOBJ(style.hairline_brush.0));
        let _ = DeleteObject(HGDIOBJ(style.item_hover_brush.0));
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

/// The brush the window's face is, for the controls it shows through: the
/// surface token where Settings paints its face (#582), else the system's.
unsafe fn face_brush_now() -> windows::Win32::Graphics::Gdi::HBRUSH {
    let style = edit_style();
    if style.paints_face {
        style.face_brush
    } else {
        windows::Win32::Graphics::Gdi::GetSysColorBrush(windows::Win32::Graphics::Gdi::COLOR_3DFACE)
    }
}

/// Answers a label's colour request when Settings paints its own face: the
/// text token on a transparent background, with the face brush behind it. The
/// caller only asks while `paints_face` is set, so light and the system's high
/// contrast keep the default answer.
unsafe fn answer_label_colors(hdc: windows::Win32::Graphics::Gdi::HDC) -> LRESULT {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{SetBkMode, SetTextColor, TRANSPARENT};
    let style = edit_style();
    SetTextColor(hdc, COLORREF(style.colors.text));
    SetBkMode(hdc, TRANSPARENT);
    LRESULT(face_brush_now().0 as isize)
}

/// `WM_NOTIFY` from a child when Settings paints its own look (#582): a push
/// button or checkbox asks before painting (`NM_CUSTOMDRAW`, common controls
/// v6) and is drawn from the tokens. Everything else, and every notification
/// while the system draws (light, real high contrast), gets `None` and the
/// default answer, exactly as before. The brush and colours are the cached
/// style's, so nothing is created per message.
unsafe fn answer_button_draw(hwnd: HWND, lparam: LPARAM) -> Option<LRESULT> {
    use crate::ui::settings_buttons::{handle_custom_draw, Palette, NM_CUSTOMDRAW_CODE};
    let style = edit_style();
    if !style.paints_face || lparam.0 == 0 {
        return None;
    }
    let header = &*(lparam.0 as *const windows::Win32::UI::Controls::NMHDR);
    if header.code != NM_CUSTOMDRAW_CODE {
        return None;
    }
    let palette = Palette::new(style.look.theme, &style.look.sys, style.colors.focus_line);
    Some(LRESULT(handle_custom_draw(
        lparam,
        dpi_for(hwnd),
        &palette,
        style.face_brush,
    )))
}

/// `WM_ERASEBKGND` when Settings paints its own face: fills the client area
/// with the face colour. The class brush is the system face, which would show
/// light in dark.
unsafe fn erase_with_face(hwnd: HWND, wparam: WPARAM) -> LRESULT {
    let mut rc = windows::Win32::Foundation::RECT::default();
    if GetClientRect(hwnd, &mut rc).is_ok() {
        windows::Win32::Graphics::Gdi::FillRect(
            windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void),
            &rc,
            face_brush_now(),
        );
    }
    LRESULT(1)
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
        &frame_colors(&edit_style().colors, combo_is_hovered(hwnd)),
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
        _ => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            if edit_may_change_scroll_need(msg) && GetDlgCtrlID(hwnd) == ID_PROMPT_EDIT {
                sync_prompt_scrollbar(hwnd);
            }
            result
        }
    }
}

/// Messages after which the prompt box may need a scroll bar, or no longer
/// need one: its size, its text and the keys and clipboard that change it.
fn edit_may_change_scroll_need(msg: u32) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{
        WM_CHAR, WM_CLEAR, WM_CUT, WM_KEYDOWN, WM_LBUTTONUP, WM_PASTE, WM_SETFONT, WM_SETTEXT,
        WM_SHOWWINDOW, WM_SIZE, WM_UNDO,
    };
    const EM_REPLACESEL: u32 = 0x00C2;
    matches!(
        msg,
        WM_SIZE
            | WM_SETTEXT
            | WM_SHOWWINDOW
            | WM_CHAR
            // Delete and Backspace edit on the key, not through WM_CHAR (Delete
            // sends none); a new font (apply_dpi) re-wraps; a mouse drop of
            // text ends on the button release.
            | WM_KEYDOWN
            | WM_SETFONT
            | WM_LBUTTONUP
            | WM_CUT
            | WM_PASTE
            | WM_CLEAR
            | WM_UNDO
            | EM_REPLACESEL
    )
}

/// #1014: whether `lines` wrapped lines of height `line_h` need a bar in a box
/// whose text area is `rect_h` tall (all px). Unmeasurable (no height yet) keeps
/// the bar, so the safe outcome of an unreadable box is the bar. The scroll
/// range cannot be used: an edit without WS_VSCROLL stops keeping it, so a
/// hidden bar would never come back.
fn scrollbar_needed(lines: i32, rect_h: i32, line_h: i32) -> bool {
    line_h <= 0 || rect_h <= 0 || lines > rect_h / line_h
}

thread_local! {
    /// Set while [`sync_prompt_scrollbar`] runs: showing or hiding the bar
    /// resizes the box, which comes back through the subclass.
    static SYNCING_PROMPT_BAR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// #1014: the prompt box shows a vertical scroll bar only while its text is
/// taller than the box. An edit control keeps a disabled bar when the text
/// fits, which reads as an empty strip down the right side.
unsafe fn sync_prompt_scrollbar(hwnd: HWND) {
    use windows::Win32::UI::Controls::ShowScrollBar;
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_STYLE};
    if SYNCING_PROMPT_BAR.get() {
        return;
    }
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetDC, GetTextMetricsW, ReleaseDC, SelectObject, HFONT, TEXTMETRICW,
    };
    use windows::Win32::UI::WindowsAndMessaging::WM_GETFONT;
    const EM_GETRECT: u32 = 0x00B2;
    const EM_GETLINECOUNT: u32 = 0x00BA;
    let lines = SendMessageW(hwnd, EM_GETLINECOUNT, None, None).0 as i32;
    let mut rect = RECT::default();
    SendMessageW(
        hwnd,
        EM_GETRECT,
        None,
        Some(LPARAM(&mut rect as *mut RECT as isize)),
    );
    let mut line_h = 0;
    let hdc = GetDC(Some(hwnd));
    if !hdc.is_invalid() {
        let font = HFONT(SendMessageW(hwnd, WM_GETFONT, None, None).0 as *mut _);
        let old = (!font.is_invalid()).then(|| SelectObject(hdc, font.into()));
        let mut tm = TEXTMETRICW::default();
        if GetTextMetricsW(hdc, &mut tm).as_bool() {
            line_h = tm.tmHeight;
        }
        if let Some(old) = old {
            SelectObject(hdc, old);
        }
        ReleaseDC(Some(hwnd), hdc);
    }
    let need = scrollbar_needed(lines, rect.bottom - rect.top, line_h);
    let shown = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VSCROLL.0 != 0;
    if need != shown {
        SYNCING_PROMPT_BAR.set(true);
        let _ = ShowScrollBar(hwnd, SB_VERT, need);
        SYNCING_PROMPT_BAR.set(false);
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
/// not show.
fn footer_divider_color(theme: crate::ui::theme::Theme, sys: &crate::ui::theme::SysColors) -> u32 {
    crate::ui::theme::Tokens::for_theme(theme, sys).border
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
        let look = edit_style().look;
        let ink = footer_divider_color(look.theme, &look.sys);
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
    // #582: push buttons and checkboxes draw from the tokens in dark.
    if msg == windows::Win32::UI::WindowsAndMessaging::WM_NOTIFY {
        if let Some(answer) = answer_button_draw(hwnd, lparam) {
            return answer;
        }
    }
    match msg {
        WM_COMMAND | WM_HSCROLL => {
            let parent = GetParent(hwnd).unwrap_or_default();
            SendMessageW(parent, msg, Some(wparam), Some(lparam))
        }
        // #854: the dropdown combos draw their own text.
        WM_DRAWITEM if draw_combo_item(lparam) => LRESULT(1),
        WM_MEASUREITEM if measure_combo_item(lparam, dpi_for(hwnd)) => LRESULT(1),
        // #824, #972: a helper line is secondary text, not the disabled grey.
        // Every other static keeps the default colours.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            if is_hint_id(GetDlgCtrlID(HWND(lparam.0 as *mut c_void))) =>
        {
            use windows::Win32::Graphics::Gdi::{SetBkMode, SetTextColor, TRANSPARENT};
            let hdc = windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void);
            SetTextColor(
                hdc,
                windows::Win32::Foundation::COLORREF(edit_style().hint_text),
            );
            SetBkMode(hdc, TRANSPARENT);
            LRESULT(face_brush_now().0 as isize)
        }
        // #582: the body's own face, where Settings paints it (dark).
        windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND if edit_style().paints_face => {
            erase_with_face(hwnd, wparam)
        }
        // #877: a hairline is a one pixel static filled with the border token.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC
            if GetDlgCtrlID(HWND(lparam.0 as *mut c_void)) == ID_ROW_SEPARATOR =>
        {
            LRESULT(edit_style().hairline_brush.0 as isize)
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
        // #582: every other label (and a group box's caption) in the text
        // token on the painted face. Light and the system's high contrast take
        // the default answer, as before.
        windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC if edit_style().paints_face => {
            answer_label_colors(windows::Win32::Graphics::Gdi::HDC(wparam.0 as *mut c_void))
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

/// The width of a vertical scrollbar at `dpi`. In the gallery at a simulated
/// scale the scrollbar is the window's real one, so that is measured instead.
fn scrollbar_width_px(dpi: u32) -> i32 {
    #[cfg(any(test, debug_assertions))]
    let dpi = NATIVE_DPI.with(std::cell::Cell::get).unwrap_or(dpi);
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
    // Room above for the row's title and its hairline (and the window's top
    // margin for the first row), a little below.
    let dpi = dpi_for(body);
    let pad_top = to_px(ROW_PAD + MARGIN, dpi);
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
        // The face comes from the system; the size from the type ramp, so labels
        // are the 14 px body and helper lines the 12 px caption (#1015).
        lf.lfHeight = body_font_height(dpi);

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

/// The labels' `lfHeight` at `dpi`: the type ramp's 14 px body (#1015).
fn body_font_height(dpi: u32) -> i32 {
    crate::ui::theme::type_height_px(crate::ui::theme::TYPE_BODY_PX, dpi, 1.0)
}

/// #1015: a helper line's `lfHeight` at `dpi`: the ramp's 12 px caption, the
/// smallest size Windows 11 uses for text, so never below 12 px at 96 DPI.
fn hint_font_height(dpi: u32) -> i32 {
    crate::ui::theme::type_height_px(crate::ui::theme::TYPE_CAPTION_PX, dpi, 1.0)
}

/// The helper-line font: the body font `body` with the smaller height of
/// [`hint_font_height`]. Null when the body font cannot be read, so the caller
/// keeps the body font. The caller deletes it.
unsafe fn build_hint_font(body: HFONT, dpi: u32) -> HFONT {
    use windows::Win32::Graphics::Gdi::GetObjectW;
    let mut lf = LOGFONTW::default();
    let got = GetObjectW(
        HGDIOBJ(body.0),
        std::mem::size_of::<LOGFONTW>() as i32,
        Some(&mut lf as *mut LOGFONTW as *mut c_void),
    );
    if got <= 0 {
        return HFONT(std::ptr::null_mut());
    }
    lf.lfHeight = hint_font_height(dpi);
    CreateFontIndirectW(&lf)
}

// ---------------------------------------------------------------------------
// UI construction
// ---------------------------------------------------------------------------

/// A cursor for stacking rows top-to-bottom down a page. All coordinates are
/// logical (96dpi) units, scaled at creation time.
struct Rows {
    x: i32,
    y: i32,
    w: i32,
    /// Whether the next row is the page's first: it has no hairline above it.
    first: bool,
    /// The width of the control column. Rows whose controls are small buttons
    /// narrow it, so a long title (a key combination) has the room.
    ctl_w: i32,
}

impl Rows {
    fn new(x: i32, y: i32, w: i32) -> Self {
        Rows {
            x,
            y,
            w,
            first: true,
            ctl_w: CONTROL_W,
        }
    }

    /// The width of the title and description column.
    fn left_w(&self) -> i32 {
        self.w - self.ctl_w - COL_GAP
    }

    /// Where the control column starts.
    fn control_x(&self) -> i32 {
        self.right() - self.ctl_w
    }

    /// The right edge of the rows: where the control column ends.
    fn right(&self) -> i32 {
        self.x + self.w
    }
}

/// How tall a row is for `desc_lines` lines of description (#877). A wide row
/// adds the height its control takes from the title's line.
fn row_height(desc_lines: i32) -> i32 {
    if desc_lines == 0 {
        ROW_PITCH
    } else {
        2 * ROW_PAD + ROW_TITLE_H + desc_lines * DESC_LINE_H
    }
}

/// A row that has been started: where it sits and how tall it is.
struct RowGeom {
    y: i32,
    h: i32,
    desc_lines: i32,
    /// Whether the description runs the page's width, under the control, so
    /// the control sits on the title's line instead of the row's middle.
    wide: bool,
}

impl RowGeom {
    /// The top of a control of height [`ROW_H`]: centred in the row, or level
    /// with the title when the description runs under it.
    fn control_y(&self) -> i32 {
        if self.wide {
            self.y + ROW_PAD
        } else {
            self.y + (self.h - ROW_H) / 2
        }
    }

    /// The top of the description.
    fn desc_y(&self) -> i32 {
        self.y
            + ROW_PAD
            + if self.wide {
                ROW_H
            } else if self.desc_lines == 0 {
                0
            } else {
                ROW_TITLE_H
            }
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
    /// #874: the part of the Models page the next control belongs to, if it
    /// is one that moves or hides when the active provider changes.
    slot: std::cell::Cell<Option<Slot>>,
    /// Every control made under a slot, at the place it has when all show.
    flow: std::cell::RefCell<Vec<FlowChild>>,
}

/// A Models page control that moves or hides with the provider choice (#874).
struct FlowChild {
    hwnd: HWND,
    slot: Slot,
    /// Its design `y` and `h` when every provider shows.
    y: i32,
    h: i32,
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
        // #854: every dropdown paints its own text, so it can sit 8 dp in from
        // the border like a Windows 11 combo. The control keeps its role.
        let style = if class == WC_COMBOBOX {
            style | CBS_OWNERDRAWFIXED as u32
        } else {
            style
        };
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
            if class == WC_COMBOBOX {
                unsafe {
                    set_combo_heights(hwnd, self.dpi);
                    let _ = windows::Win32::UI::Shell::SetWindowSubclass(
                        hwnd,
                        Some(combo_subclass),
                        COMBO_SUBCLASS_ID,
                        0,
                    );
                    subclass_combo_list(hwnd);
                }
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
            if let Some(slot) = self.slot.get() {
                self.flow.borrow_mut().push(FlowChild { hwnd, slot, y, h });
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
    /// A hairline across the row that starts at `r.y` (#877).
    fn hairline(&self, r: &Rows) {
        let h = self.create(WC_STATIC, "", 0, 0, r.x, r.y, r.w, 1, ID_ROW_SEPARATOR);
        self.set_anchor(h, Anchor::Hairline);
    }

    /// Starts a row (#877): the hairline above it (all but a page's first
    /// row), then its title at the left. The caller makes the control, then
    /// calls [`Ctx::end_row`]. The title comes right before the control in
    /// z-order, which is what makes a screen reader name the control by it.
    fn begin_row(&self, r: &mut Rows, title: &str, title_id: i32, desc_lines: i32) -> RowGeom {
        self.begin_row_as(r, title, title_id, desc_lines, false)
    }

    /// [`Ctx::begin_row`] for a row whose description runs the page's width.
    fn begin_wide_row(&self, r: &mut Rows, title: &str, title_id: i32, desc_lines: i32) -> RowGeom {
        self.begin_row_as(r, title, title_id, desc_lines, true)
    }

    fn begin_row_as(
        &self,
        r: &mut Rows,
        title: &str,
        title_id: i32,
        desc_lines: i32,
        wide: bool,
    ) -> RowGeom {
        if !r.first {
            self.hairline(r);
        }
        r.first = false;
        let h = row_height(desc_lines) + if wide { ROW_H - ROW_TITLE_H } else { 0 };
        let title_h = if desc_lines == 0 || wide {
            ROW_H
        } else {
            ROW_TITLE_H
        };
        self.create(
            WC_STATIC,
            title,
            0,
            0,
            r.x,
            r.y + ROW_PAD,
            r.left_w(),
            title_h,
            title_id,
        );
        RowGeom {
            y: r.y,
            h,
            desc_lines,
            wide,
        }
    }

    /// Ends a row: its description under the title, made after the control so
    /// the control is named by the title and not by the description (`wide`
    /// lets the line run under the control column too), then the cursor moves
    /// to the next row.
    fn end_row(&self, r: &mut Rows, g: &RowGeom, desc: Option<(&str, i32)>) {
        if let Some((text, id)) = desc {
            let w = if g.wide { r.w } else { r.left_w() };
            self.create(
                WC_STATIC,
                text,
                0,
                0,
                r.x,
                g.desc_y(),
                w,
                g.desc_lines * DESC_LINE_H,
                id,
            );
        }
        r.y += g.h;
    }

    /// A row whose control is a check box that carries the title itself, with
    /// the description under it, lined up with the box's text (#972).
    fn check_row(
        &self,
        r: &mut Rows,
        text: &str,
        id: i32,
        desc: Option<(&str, i32)>,
        desc_lines: i32,
    ) -> HWND {
        if !r.first {
            self.hairline(r);
        }
        r.first = false;
        let h = 2 * ROW_PAD + ROW_H + desc_lines * DESC_LINE_H;
        let check = self.create(
            WC_BUTTON,
            text,
            BS_AUTOCHECKBOX as u32 | WS_TABSTOP.0,
            0,
            r.x,
            r.y + ROW_PAD,
            r.w,
            ROW_H,
            id,
        );
        if let Some((text, id)) = desc {
            self.create(
                WC_STATIC,
                text,
                0,
                0,
                r.x + CHECKBOX_TEXT_INDENT_DP,
                r.y + ROW_PAD + ROW_H,
                r.w - CHECKBOX_TEXT_INDENT_DP,
                desc_lines * DESC_LINE_H,
                id,
            );
        }
        r.y += h;
        check
    }

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

/// Posted to the Settings window when a vendor has answered for a typed key:
/// `wparam` is the vendor's index in [`CLOUD_VENDORS`], `lparam` the check's
/// number and the verdict (`commit::encode_verdict`).
const WM_KEY_CHECKED: u32 = 0x8000 + 48; // WM_APP + 48

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
            true,
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
    /// The newest fetch number each vendor's list was saved to disk for
    /// (#585). Held while the file is written, so the newer list wins. The UI
    /// thread never takes it, so a slow disk cannot stall it.
    saved_generation: std::sync::Mutex<[u64; 3]>,
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
        false,
        inner.cloud_models.clone(),
        inner.cloud_sync.clone(),
        inner.alive.clone(),
    );
}

/// Keeps a saved list for vendor `index` while nothing better is there: not
/// over a fetched list, and not when a newer fetch has started. Does not mark
/// a key as fetched, so a failed refresh is still retried. Returns whether it
/// stored.
fn store_cached_list(
    slots: &CloudModels,
    sync: &CloudSync,
    index: usize,
    mine: u64,
    ids: Vec<String>,
) -> bool {
    let Ok(mut guard) = slots.lock() else {
        return false;
    };
    if !is_current_generation(sync.generation[index].load(Ordering::SeqCst), mine)
        || guard[index].is_some()
    {
        return false;
    }
    guard[index] = Some(ids);
    true
}

fn spawn_one_cloud_fetch(
    hwnd: HWND,
    index: usize,
    key: String,
    use_cache: bool,
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
            // #585: the saved list first, so the dropdown is full before the
            // network answers. The vendor is asked again either way.
            if use_cache {
                if let Some(ids) = crate::provider::model_cache::load(vendor) {
                    if store_cached_list(&slots, &sync, index, mine, ids) {
                        post_while_open(&alive, || unsafe {
                            let _ = PostMessageW(
                                Some(HWND(hwnd_raw as *mut c_void)),
                                WM_CLOUD_MODELS,
                                WPARAM(index),
                                LPARAM(0),
                            );
                        });
                    }
                }
            }
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
            // A failed fetch returned above and left the last good list (the
            // slot and the saved copy) as it was.
            if !store_fetched_list(&slots, &sync, index, mine, &key, ids) {
                return;
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

/// Keeps a fetched list for vendor `index`, unless a newer fetch for that vendor
/// started while this one ran: its result wins, so this one is dropped. The check
/// and the store happen under one lock, so an older worker cannot pass the check
/// and then store after a newer one finished (#678). Returns whether it stored.
///
/// An accepted list is also saved to disk (#585), after the slots lock is
/// released (the UI thread takes that lock). Called from worker threads only.
fn store_fetched_list(
    slots: &CloudModels,
    sync: &CloudSync,
    index: usize,
    mine: u64,
    key: &str,
    ids: Vec<String>,
) -> bool {
    let saved = ids.clone();
    {
        let Ok(mut guard) = slots.lock() else {
            return false;
        };
        if !is_current_generation(sync.generation[index].load(Ordering::SeqCst), mine) {
            return false;
        }
        guard[index] = Some(ids);
        // The fetched-key record is written under the same slots lock (always
        // slots first, then keys), so it cannot go stale.
        if let Ok(mut keys) = sync.fetched_keys.lock() {
            keys[index] = key.trim().to_string();
        }
    }
    save_accepted_list(sync, index, mine, &saved);
    true
}

/// Saves an accepted list to disk unless a newer fetch's list was saved
/// already, so the newer list wins even when the workers finish out of order.
fn save_accepted_list(sync: &CloudSync, index: usize, mine: u64, ids: &[String]) {
    let Ok(mut saved) = sync.saved_generation.lock() else {
        return;
    };
    if saved[index] > mine {
        return;
    }
    crate::provider::model_cache::store(CLOUD_VENDORS[index], ids);
    saved[index] = mine;
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
    format!("&Shortcut for any keyboard: {}", chord_to_string(chord))
}

/// The Copilot key's line (#823).
fn primary_label(chord: &crate::hotkey::Chord) -> String {
    format!("Copilot &key: {}", chord_to_string(chord))
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
        Some(c) => format!("Pause s&hortcut: {}", chord_to_string(c)),
        None => format!(
            "Pause s&hortcut: {}",
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
        say(
            hwnd,
            ID_VALIDATION_MESSAGE,
            "Wingman's main window is not running.",
        );
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
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner;
    if state.is_null() {
        return;
    }
    let owner = (*state).owner;
    let labels: Vec<String> = PAUSE_MENU.iter().map(|(_, l)| l.to_string()).collect();
    let Some(i) = pick_from_menu(hwnd, ID_PAUSE_BUTTON, &labels) else {
        return;
    };
    if let Some((choice, _)) = PAUSE_MENU.get(i) {
        send_command_to(hwnd, owner, crate::app::SettingsCommand::Pause(*choice));
    }
}

/// A small menu under the button `button_id` with one item per label; the
/// index picked, or `None` when it was dismissed or the window closed while
/// the menu was up. Holds no borrow of the window's state.
unsafe fn pick_from_menu(hwnd: HWND, button_id: i32, labels: &[String]) -> Option<usize> {
    use windows::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, DestroyMenu, IsWindow, TrackPopupMenu, MF_STRING,
        TPM_LEFTALIGN, TPM_RETURNCMD, TPM_TOPALIGN,
    };
    let button = get_dlg_item(hwnd, button_id)?;
    let menu = unsafe { CreatePopupMenu() }.ok()?;
    for (i, label) in labels.iter().enumerate() {
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
        return None;
    }
    usize::try_from(picked.0)
        .ok()
        .and_then(|n| n.checked_sub(1))
        .filter(|i| *i < labels.len())
}

/// Posted by the Export button to the window itself, once the worker has the
/// list of actions to offer (#1152). Same reason as [`WM_PAUSE_MENU`].
const WM_EXPORT_MENU: u32 = 0x8000 + 0x874;

/// The Export menu: the user's actions under the button; the pick goes to the
/// app, which asks for the file name. The list is copied out before the menu.
unsafe fn run_export_menu(hwnd: HWND) {
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SettingsInner;
    if state.is_null() {
        return;
    }
    let owner = (*state).owner;
    let choices = (*state).export_choices.clone();
    let labels: Vec<String> = choices.iter().map(|(_, name)| menu_label(name)).collect();
    let Some(i) = pick_from_menu(hwnd, ID_EXPORT_ACTION, &labels) else {
        return;
    };
    let Some((id, _)) = choices.into_iter().nth(i) else {
        return;
    };
    if owner.0.is_null() {
        return;
    }
    if !crate::app::post_export_pick(owner.0 as isize, id) {
        say(hwnd, ID_VALIDATION_MESSAGE, MAIN_WINDOW_GONE);
    }
}

/// A menu item's text with each `&` doubled, so a name shows as written.
fn menu_label(name: &str) -> String {
    name.replace('&', "&&")
}

const MAIN_WINDOW_GONE: &str = "Wingman's main window is not running.";

/// What the Actions page's worker can be asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ActionsJob {
    /// Find the actions file, to open it.
    Open,
    /// List the actions the user wrote, for the Export menu.
    Export,
}

/// What the worker found. The window acts on it: the shell and the menu are
/// not for a worker thread.
enum ActionsDone {
    OpenPath(std::path::PathBuf),
    Say(String),
    Export(Result<Vec<(String, String)>, String>),
}

type ActionsSlot = std::sync::Arc<std::sync::Mutex<Option<ActionsDone>>>;

/// Shown when there is no actions file yet (rule 7: a line, not a no-op).
const NO_ACTIONS_FILE: &str = "There is no actions file yet. Import an action to start one.";
const OPEN_ACTIONS_FAILED: &str = "Could not open the actions file.";
const NO_ACTIONS_TO_EXPORT: &str =
    "You have not written any actions yet, so there is nothing to export.";
const ACTIONS_NOT_STARTED: &str = "Could not start. Try again.";

/// The worker's whole job (#1152): the only place the Actions page reads the disk.
fn run_actions_job(job: ActionsJob) -> ActionsDone {
    match job {
        ActionsJob::Open => match crate::actions::path() {
            Ok(path) if path.exists() => ActionsDone::OpenPath(path),
            Ok(_) => ActionsDone::Say(NO_ACTIONS_FILE.to_string()),
            Err(_) => ActionsDone::Say(OPEN_ACTIONS_FAILED.to_string()),
        },
        ActionsJob::Export => ActionsDone::Export(
            crate::actions::load_actions()
                .map(|all| crate::actions::share::export_candidates(&all))
                .map_err(|e| format!("{e:#}")),
        ),
    }
}

/// What the Export button does with the worker's list.
#[derive(Debug, PartialEq, Eq)]
enum ExportOutcome {
    Menu(Vec<(String, String)>),
    Say(String),
}

fn export_outcome(list: Result<Vec<(String, String)>, String>) -> ExportOutcome {
    match list {
        Ok(list) if list.is_empty() => ExportOutcome::Say(NO_ACTIONS_TO_EXPORT.to_string()),
        Ok(list) => ExportOutcome::Menu(list),
        Err(e) => ExportOutcome::Say(format!("Could not read your actions: {e}")),
    }
}

/// Hands `job` to a worker thread; the answer comes back as
/// `WM_SETTINGS_ACTIONS`. One at a time: a click while one runs does nothing,
/// as the line it will print is on its way.
fn start_actions_job(inner: &mut SettingsInner, job: ActionsJob) {
    if inner.closing || inner.actions_busy.load(Ordering::SeqCst) {
        return;
    }
    inner.actions_busy.store(true, Ordering::SeqCst);
    let slot = inner.actions_slot.clone();
    let alive = inner.alive.clone();
    let busy = inner.actions_busy.clone();
    let hwnd = inner.hwnd.0 as isize;
    let spawned = std::thread::Builder::new()
        .name("settings-actions".into())
        .spawn(move || {
            let done = run_actions_job(job);
            if let Ok(mut g) = slot.lock() {
                *g = Some(done);
            }
            let mut posted = false;
            post_while_open(&alive, || {
                posted = unsafe {
                    PostMessageW(
                        Some(HWND(hwnd as *mut c_void)),
                        WM_SETTINGS_ACTIONS,
                        WPARAM(0),
                        LPARAM(0),
                    )
                }
                .is_ok();
            });
            // Nobody will answer: free the page for the next click.
            if !posted {
                busy.store(false, Ordering::SeqCst);
            }
        });
    if spawned.is_err() {
        inner.actions_busy.store(false, Ordering::SeqCst);
        say(inner.hwnd, ID_VALIDATION_MESSAGE, ACTIONS_NOT_STARTED);
    }
}

/// The worker's answer: open the file, say what is wrong, or ask the window to
/// show the Export menu (not from here: this runs in the window's handler).
fn take_actions(inner: &mut SettingsInner) {
    inner.actions_busy.store(false, Ordering::SeqCst);
    let Some(done) = inner.actions_slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    if inner.closing {
        return;
    }
    match done {
        ActionsDone::OpenPath(path) => {
            if !open_url(&path.to_string_lossy()) {
                say(inner.hwnd, ID_VALIDATION_MESSAGE, OPEN_ACTIONS_FAILED);
            }
        }
        ActionsDone::Say(line) => say(inner.hwnd, ID_VALIDATION_MESSAGE, &line),
        ActionsDone::Export(list) => match export_outcome(list) {
            ExportOutcome::Say(line) => say(inner.hwnd, ID_VALIDATION_MESSAGE, &line),
            ExportOutcome::Menu(list) => {
                inner.export_choices = list;
                unsafe {
                    let _ = PostMessageW(Some(inner.hwnd), WM_EXPORT_MENU, WPARAM(0), LPARAM(0));
                }
            }
        },
    }
}

/// Width of the "Open config file" button, in dp (#823).
const OPEN_CONFIG_W: i32 = 120;
/// #1152: the width of the Actions page's buttons, in dp.
const ACTIONS_BUTTON_W: i32 = 140;
/// #874: the width of "Add another provider".
const ADD_PROVIDER_W: i32 = 170;

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
        say(inner.hwnd, ID_VALIDATION_MESSAGE, OPEN_CONFIG_FAILED);
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

/// #828: the timer that moves the dots of [`OLLAMA_CHECKING_TEXT`] while the
/// check runs. Set when the window opens, killed when the result arrives or
/// the window closes, so nothing ticks once the check is done (AGENTS.md
/// rule 5).
const CHECK_TIMER_ID: usize = 0x346;
const CHECK_TIMER_MS: u32 = 400;

/// The cue's text for `frame`: one, two or three dots. Frame 2 is the static
/// text the window is built with.
fn checking_text(frame: usize) -> String {
    format!("Checking Ollama{}", ".".repeat(frame % 3 + 1))
}

/// True for any frame of the cue, so a tick never overwrites another message
/// (a started fix, a result).
fn is_checking_text(text: &str) -> bool {
    text.strip_prefix("Checking Ollama")
        .is_some_and(|dots| matches!(dots, "." | ".." | "..."))
}

/// Starts the cue unless the user turned client-area animation off (then the
/// static text stays).
fn start_checking_animation(inner: &mut SettingsInner, animate: bool) {
    if !animate || inner.checking_frame.is_some() {
        return;
    }
    inner.checking_frame = Some(2);
    unsafe { SetTimer(Some(inner.hwnd), CHECK_TIMER_ID, CHECK_TIMER_MS, None) };
}

fn tick_checking_animation(inner: &mut SettingsInner) {
    let Some(frame) = inner.checking_frame else {
        let _ = unsafe { KillTimer(Some(inner.hwnd), CHECK_TIMER_ID) };
        return;
    };
    let next = (frame + 1) % 3;
    inner.checking_frame = Some(next);
    if let Some(status) = get_dlg_item(inner.hwnd, ID_OLLAMA_STATUS) {
        if is_checking_text(&get_text(status)) {
            set_text(status, &checking_text(next));
        }
    }
}

fn stop_checking_animation(inner: &mut SettingsInner) {
    if inner.checking_frame.take().is_some() {
        let _ = unsafe { KillTimer(Some(inner.hwnd), CHECK_TIMER_ID) };
    }
}

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
    /// See [`SettingsInner::models_flow`].
    models_flow: Vec<FlowChild>,
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
        slot: std::cell::Cell::new(None),
        flow: std::cell::RefCell::new(Vec::new()),
    };
    let content_x = MARGIN;
    let content_w = PAGE_W_DP - 2 * MARGIN;
    let combo_style = (CBS_DROPDOWNLIST | CBS_HASSTRINGS) as u32;

    // Where each page's last row ends (dp).
    let mut page_bottom = [0i32; 5];

    // -- Models ------------------------------------------------------------
    ctx.page.set(Some(Page::Models));
    let mut r = Rows::new(content_x, PAGE_TOP_PAD_DP, content_w);

    let g = ctx.begin_row(&mut r, "Active &provider", 0, 0);
    let active_combo = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
        ROW_H * 6,
        ID_ACTIVE_PROVIDER,
    );
    combo_add(active_combo, "ChatGPT (OpenAI)");
    combo_add(active_combo, "Claude (Anthropic)");
    combo_add(active_combo, "Ollama (local)");
    combo_add(active_combo, "Gemini (Google)");
    combo_select_index(active_combo, combo_index_for_order(&config.providers.order));
    ctx.end_row(&mut r, &g, None);

    // The three cloud vendors: a key, a model and a thinking time each (#195).
    // Every title names its vendor, so a screen reader hears "OpenAI model",
    // not a third "Model" (#781).
    let vendors = [
        (
            &CLOUD_ROWS[0],
            key_field_display(&config.providers.openai.api_key),
            &config.providers.openai.models[..],
            &config.providers.openai.model[..],
            effort_display(&config.providers.openai.effort).to_string(),
        ),
        (
            &CLOUD_ROWS[1],
            key_field_display(&config.providers.anthropic.api_key),
            &config.providers.anthropic.models[..],
            &config.providers.anthropic.model[..],
            effort_display(&config.providers.anthropic.effort).to_string(),
        ),
        (
            &CLOUD_ROWS[2],
            key_field_display(&config.providers.gemini.api_key),
            &config.providers.gemini.models[..],
            &config.providers.gemini.model[..],
            effort_display(&config.providers.gemini.effort).to_string(),
        ),
    ];
    for (rows, key, models, model, effort) in vendors {
        ctx.slot.set(Some(rows.slot));
        let g = ctx.begin_row(&mut r, rows.key_title, 0, 0);
        ctx.create(
            WC_EDIT,
            &key,
            (ES_PASSWORD | ES_AUTOHSCROLL) as u32,
            WS_EX_CLIENTEDGE,
            r.control_x(),
            g.control_y(),
            CONTROL_W - SHOW_KEY_W - COL_GAP / 2,
            ROW_H,
            rows.key_id,
        );
        ctx.create(
            WC_BUTTON,
            "Show",
            BS_AUTOCHECKBOX as u32,
            0,
            r.right() - SHOW_KEY_W,
            g.control_y(),
            SHOW_KEY_W,
            ROW_H,
            rows.show_id,
        );
        ctx.end_row(&mut r, &g, None);

        let g = ctx.begin_row(&mut r, rows.model_title, 0, 0);
        let model_combo = ctx.create(
            WC_COMBOBOX,
            "",
            combo_style,
            0,
            r.control_x(),
            g.control_y(),
            CONTROL_W,
            ROW_H * 8,
            rows.model_id,
        );
        fill_model_combo(model_combo, models, model);
        ctx.end_row(&mut r, &g, None);

        let g = ctx.begin_row(&mut r, rows.effort_title, rows.effort_title_id, 0);
        let effort_combo = ctx.create(
            WC_COMBOBOX,
            "",
            combo_style,
            0,
            r.control_x(),
            g.control_y(),
            CONTROL_W,
            ROW_H * 4,
            rows.effort_id,
        );
        for e in EFFORT_LABELS {
            combo_add(effort_combo, e);
        }
        combo_select_with_custom(effort_combo, &EFFORT_LABELS, &effort);
        ctx.end_row(&mut r, &g, None);
    }
    sync_gemini_thinking_enabled(hwnd);

    // Ollama (#183): no key, only which pulled model to use. The combo starts
    // with the configured model alone; the pulled models are added when the
    // background fetch reports in (see `spawn_ollama_models_fetch`).
    ctx.slot.set(Some(Slot::Ollama));
    let g = ctx.begin_row(&mut r, "Oll&ama model", 0, 0);
    let ollama_model = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
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
    ctx.end_row(&mut r, &g, None);

    // #874: shows the providers that are hidden. Under the blocks it
    // reveals, so a reader reaches it after the rows they can already see.
    ctx.slot.set(Some(Slot::AddButton));
    let g = ctx.begin_row(&mut r, "Other providers", 0, 0);
    ctx.create(
        WC_BUTTON,
        "Add another provider",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.control_x(),
        g.control_y(),
        ADD_PROVIDER_W,
        ROW_H,
        ID_ADD_PROVIDER,
    );
    ctx.end_row(&mut r, &g, None);

    // #427: the cloud model dropdowns list what the provider offers (or
    // what is in config.toml), so say where to add one that is missing.
    ctx.slot.set(Some(Slot::Hint));
    let g = ctx.begin_row(
        &mut r,
        model_list_hint_for(config.mode),
        ID_MODEL_LIST_HINT,
        0,
    );
    ctx.create(
        WC_BUTTON,
        "Open config file",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.control_x(),
        g.control_y(),
        OPEN_CONFIG_W,
        ROW_H,
        ID_OPEN_CONFIG,
    );
    ctx.end_row(&mut r, &g, None);

    // -- Ollama status (#14, #15) ----------------------------------------
    // One read-only line: whether anything answers on the configured
    // Ollama port, whether it's the stock tray app's CPU-only server
    // (AGENTS.md's "Stock Ollama's tray app steals port 11434" pitfall),
    // and -- once it's confirmed to be a real server -- whether the
    // configured model is currently loaded on GPU or CPU and how many
    // local models support vision. See `ollama_status_line`.
    //
    // Read-only: choosing Ollama as the active provider and its model is
    // done in the rows above (#183). `base_url` stays a config.toml setting.
    // It shows with the Ollama block and moves up with the rows above it
    // (#874).
    ctx.slot.set(Some(Slot::OllamaCard));
    ctx.hairline(&r);
    // The text is a placeholder until the worker reports (#527, #568), so the
    // window never waits on Ollama or the disk. The link button sits at the
    // right of the two text rows; it is created hidden and shown when the
    // result says it has a link, so the text is always laid out narrower to
    // leave room for it.
    let link_w = OLLAMA_LINK_W + COL_GAP;
    ctx.create(
        WC_STATIC,
        OLLAMA_CHECKING_TEXT,
        0,
        0,
        r.x,
        r.y + ROW_PAD,
        r.w - link_w,
        OLLAMA_STATUS_H,
        ID_OLLAMA_STATUS,
    );
    ctx.create(
        WC_BUTTON,
        "Local models",
        BS_PUSHBUTTON as u32,
        0,
        r.x + r.w - OLLAMA_LINK_W,
        r.y + ROW_PAD + (OLLAMA_STATUS_H - BTN_H) / 2,
        OLLAMA_LINK_W,
        BTN_H,
        ID_OLLAMA_LINK,
    );
    if let Some(link) = get_dlg_item(hwnd, ID_OLLAMA_LINK) {
        unsafe {
            let _ = ShowWindow(link, SW_HIDE);
        }
    }
    r.y += 2 * ROW_PAD + OLLAMA_STATUS_H;
    // #566: pull a model by name. The name box, a Pull/Stop button, and one
    // line for progress or the reason it failed. No list of suggested
    // models: the user types the name (see docs/providers.md).
    let g = ctx.begin_row(&mut r, "P&ull model", 0, 0);
    ctx.create(
        WC_EDIT,
        "",
        ES_AUTOHSCROLL as u32,
        WS_EX_CLIENTEDGE,
        r.control_x(),
        g.control_y(),
        CONTROL_W - OLLAMA_PULL_BTN_W - COL_GAP / 2,
        ROW_H,
        ID_PULL_NAME,
    );
    ctx.create(
        WC_BUTTON,
        "Pull",
        BS_PUSHBUTTON as u32,
        0,
        r.right() - OLLAMA_PULL_BTN_W,
        g.control_y(),
        OLLAMA_PULL_BTN_W,
        ROW_H,
        ID_PULL_BUTTON,
    );
    // Progress or the reason it failed, the width of the page: it is a
    // result, not a hint, so it keeps the label colour.
    ctx.end_row(&mut r, &g, None);
    ctx.create(
        WC_STATIC,
        "",
        0,
        0,
        r.x,
        r.y - ROW_PAD,
        r.w,
        PULL_STATUS_H,
        ID_PULL_STATUS,
    );
    r.y += PULL_STATUS_H;
    ctx.slot.set(None);
    page_bottom[Page::Models.index()] = r.y;

    // -- General -------------------------------------------------------------
    ctx.page.set(Some(Page::General));
    let mut r = Rows::new(content_x, PAGE_TOP_PAD_DP, content_w);
    let change_w = 90;
    // #873: Mode and Pause act at once, through the tray's own handlers.
    let g = ctx.begin_row(&mut r, "&Mode", 0, 2);
    let mode_combo = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
        ROW_H * 5,
        ID_MODE,
    );
    for m in MODE_CHOICES {
        combo_add(mode_combo, m.label());
    }
    combo_select(mode_combo, config.mode.label());
    ctx.end_row(&mut r, &g, Some((MODE_DESC, ID_ROW_DESC)));

    // The rows with only small buttons give their title the room.
    r.ctl_w = 2 * change_w + COL_GAP / 2;
    let paused_now = crate::pause::is_paused_now();
    let (can_pause, can_resume) = pause_buttons_enabled(paused_now);
    let g = ctx.begin_row(
        &mut r,
        &pause_status_line(paused_now.then_some("Paused")),
        ID_PAUSE_STATUS,
        0,
    );
    let pause_button = ctx.create(
        WC_BUTTON,
        "&Pause...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.right() - 2 * change_w - COL_GAP / 2,
        g.control_y(),
        change_w,
        ROW_H,
        ID_PAUSE_BUTTON,
    );
    let resume_button = ctx.create(
        WC_BUTTON,
        "&Resume",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.right() - change_w,
        g.control_y(),
        change_w,
        ROW_H,
        ID_RESUME_BUTTON,
    );
    unsafe {
        let _ = EnableWindow(pause_button, can_pause);
        let _ = EnableWindow(resume_button, can_resume);
    }
    ctx.end_row(&mut r, &g, None);

    // The three key rows: what it is, and the button that changes it.
    for (label, id_label, button) in [
        (
            primary_label(&config.hotkeys.primary),
            ID_PRIMARY_LABEL,
            ID_CHANGE_PRIMARY,
        ),
        (
            secondary_label(&config.hotkeys.secondary),
            ID_SECONDARY_LABEL,
            ID_CHANGE_SECONDARY,
        ),
    ] {
        let g = ctx.begin_row(&mut r, &label, id_label, 0);
        ctx.create(
            WC_BUTTON,
            "Change...",
            BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
            0,
            r.right() - change_w,
            g.control_y(),
            change_w,
            ROW_H,
            button,
        );
        ctx.end_row(&mut r, &g, None);
    }
    // #873: the pause shortcut, moved here from the tray menu. The learn
    // flow's whole conversation happens on the line under it, inline: never a
    // dialog and never a card (rule 7).
    let g = ctx.begin_wide_row(
        &mut r,
        &pause_label(&config.hotkeys.pause),
        ID_PAUSE_LABEL,
        1,
    );
    ctx.create(
        WC_BUTTON,
        "Change...",
        BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
        0,
        r.right() - change_w,
        g.control_y(),
        change_w,
        ROW_H,
        ID_CHANGE_PAUSE,
    );
    ctx.end_row(
        &mut r,
        &g,
        Some(("Click Change, then press the new key.", ID_HOTKEY_STATUS)),
    );

    r.ctl_w = CONTROL_W;

    // #393: what the Wingman key does while a card is showing. Each row applies
    // at once and the key reads the setting on every press, so no restart.
    let g = ctx.begin_row(&mut r, "Wingman ke&y on a card", 0, 0);
    let key_mode = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
        ROW_H * 5,
        ID_KEY_MODE,
    );
    for label in KEY_MODE_LABELS {
        combo_add(key_mode, label);
    }
    combo_select(key_mode, key_mode_label(config.hotkeys.key_mode_value()));
    ctx.end_row(&mut r, &g, None);

    let g = ctx.begin_row(&mut r, DOUBLE_PRESS_LABEL, 0, 1);
    ctx.create(
        WC_EDIT,
        &config.hotkeys.double_press_ms.to_string(),
        (ES_NUMBER | ES_AUTOHSCROLL) as u32,
        WS_EX_CLIENTEDGE,
        r.control_x(),
        g.control_y(),
        CARD_SECONDS_EDIT_W,
        ROW_H,
        ID_DOUBLE_PRESS,
    );
    ctx.create(
        WC_STATIC,
        DOUBLE_PRESS_UNIT,
        0,
        0,
        r.control_x() + CARD_SECONDS_EDIT_W + COL_GAP / 2,
        g.control_y(),
        CONTROL_W - CARD_SECONDS_EDIT_W - COL_GAP / 2,
        ROW_H,
        0,
    );
    ctx.end_row(&mut r, &g, Some((DOUBLE_PRESS_NOTE, ID_ROW_DESC)));

    let click_away = ctx.check_row(&mut r, CLICK_AWAY_LABEL, ID_CLICK_AWAY, None, 0);
    set_checkbox(click_away, config.options.click_away_dismiss);

    // Autostart state is owned by Windows, not by Config. The box starts
    // unchecked: the Run key is read by a worker once the window exists (#1105),
    // so the box can never disagree with reality for long.
    let autostart = ctx.check_row(&mut r, "Start with &Windows", ID_AUTOSTART, None, 0);
    // Portable mode (#130) makes no registry writes, so the switch is greyed.
    if crate::portable::is_portable() {
        unsafe {
            let _ = EnableWindow(autostart, false);
        }
    }

    // -- Card (General) ---------------------------------------------------------
    let g = ctx.begin_row(&mut r, CARD_SECONDS_LABEL, 0, 1);
    ctx.create(
        WC_EDIT,
        &config.ui.card_seconds.to_string(),
        (ES_NUMBER | ES_AUTOHSCROLL) as u32,
        WS_EX_CLIENTEDGE,
        r.control_x(),
        g.control_y(),
        CARD_SECONDS_EDIT_W,
        ROW_H,
        ID_CARD_SECONDS,
    );
    ctx.create(
        WC_STATIC,
        CARD_SECONDS_UNIT,
        0,
        0,
        r.control_x() + CARD_SECONDS_EDIT_W + COL_GAP / 2,
        g.control_y(),
        CONTROL_W - CARD_SECONDS_EDIT_W - COL_GAP / 2,
        ROW_H,
        0,
    );
    ctx.end_row(&mut r, &g, Some((CARD_SECONDS_NOTE, ID_ROW_DESC)));

    let g = ctx.begin_row(&mut r, "&Text scale", 0, 0);
    let track = ctx.create(
        TRACKBAR_CLASS_STR,
        "",
        TBS_HORZ as u32,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W - TEXT_SCALE_LABEL_W,
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
    ctx.create(
        WC_STATIC,
        &text_scale_label(config.ui.text_scale),
        0,
        0,
        r.right() - TEXT_SCALE_LABEL_W + COL_GAP / 2,
        g.control_y(),
        TEXT_SCALE_LABEL_W - COL_GAP / 2,
        ROW_H,
        ID_TEXT_SCALE_LABEL,
    );
    ctx.end_row(&mut r, &g, None);

    // #498: where the card appears. The monitor list is read when Settings
    // opens, so a display plugged in afterwards shows up next time.
    let g = ctx.begin_row(&mut r, "Card &corner", 0, 0);
    let corner = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
        ROW_H * 7,
        ID_CARD_CORNER,
    );
    for label in CARD_CORNER_LABELS {
        combo_add(corner, label);
    }
    combo_select(corner, card_corner_label(config.ui.card_corner));
    ctx.end_row(&mut r, &g, None);

    let g = ctx.begin_row(&mut r, "Card m&onitor", ID_CARD_MONITOR_LABEL, 0);
    let card_monitor = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
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
    ctx.end_row(&mut r, &g, None);
    page_bottom[Page::General.index()] = r.y;

    // -- Privacy -------------------------------------------------------------
    ctx.page.set(Some(Page::Privacy));
    let mut r = Rows::new(content_x, PAGE_TOP_PAD_DP, content_w);
    // #341: a one-line note, since "screenshot detail" alone doesn't say what
    // changes or why it costs more.
    let g = ctx.begin_row(&mut r, "Screenshot &detail", 0, 1);
    let max_edge = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
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
    ctx.end_row(
        &mut r,
        &g,
        Some((
            "Higher reads small text better, costs more.",
            ID_CAPTURE_DETAIL_HINT,
        )),
    );

    let g = ctx.begin_row(&mut r, "Sc&reen", 0, 0);
    let monitor = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
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
    ctx.end_row(&mut r, &g, None);

    // #509: capture.redact. The note is two lines so it wraps in words rather
    // than running off the page.
    let redact = ctx.check_row(
        &mut r,
        REDACT_LABEL,
        ID_REDACT,
        Some((REDACT_HINT, ID_REDACT_HINT)),
        2,
    );
    set_checkbox(redact, config.capture.redact);

    // -- Forms (#403) --------------------------------------------------------
    let g = ctx.begin_row(&mut r, "Review &before filling", 0, 0);
    let require_tick_for = ctx.create(
        WC_COMBOBOX,
        "",
        combo_style,
        0,
        r.control_x(),
        g.control_y(),
        CONTROL_W,
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
    ctx.end_row(&mut r, &g, None);
    page_bottom[Page::Privacy.index()] = r.y;

    // -- Actions (#872): no setting yet, so one note, never an empty page ---
    ctx.page.set(Some(Page::Actions));
    let mut r = Rows::new(content_x, PAGE_TOP_PAD_DP, content_w);
    let note_h = ROW_H * 2;
    ctx.create(
        WC_STATIC,
        ACTIONS_NOTE,
        0,
        0,
        r.x,
        r.y + ROW_PAD,
        r.w,
        note_h,
        ID_ACTIONS_NOTE,
    );
    r.y += 2 * ROW_PAD + note_h;
    // #1152: the three buttons, one row each, the title before the button.
    for (title, desc, text, id) in [
        (
            "Your actions &file",
            "Where the actions you wrote live.",
            "Open actions file",
            ID_OPEN_ACTIONS,
        ),
        (
            "Import an actio&n",
            "Pick a file someone shared. You see what it does first.",
            "Import action",
            ID_IMPORT_ACTION,
        ),
        (
            "&Export an action",
            "Save one of your actions to a file to share.",
            "Export action",
            ID_EXPORT_ACTION,
        ),
    ] {
        let g = ctx.begin_row(&mut r, title, 0, 1);
        ctx.create(
            WC_BUTTON,
            text,
            BS_PUSHBUTTON as u32 | WS_TABSTOP.0,
            0,
            r.control_x(),
            g.control_y(),
            ACTIONS_BUTTON_W,
            ROW_H,
            id,
        );
        ctx.end_row(&mut r, &g, Some((desc, ID_ROW_DESC)));
    }
    page_bottom[Page::Actions.index()] = r.y;

    // -- Prompt (Advanced; grows to fill the space above the button row) ----
    ctx.page.set(Some(Page::Advanced));
    let footer = footer_layout(PAGE_TOP_PAD_DP, win_h_dp, false);
    let prompt_top = footer.prompt_top;
    let content_top_dp = footer.prompt_top;
    // The title first, so the box that follows is named by it.
    ctx.create(
        WC_STATIC,
        PROMPT_TITLE,
        0,
        0,
        content_x,
        prompt_top + ROW_PAD,
        content_w,
        ROW_TITLE_H,
        0,
    );
    let reset_btn_h = BTN_H;
    let prompt_edit_h = footer.prompt_edit_h;
    let prompt_edit = ctx.create(
        WC_EDIT,
        &prompt_to_edit(&config.ui.prompt),
        (ES_MULTILINE | ES_WANTRETURN | ES_AUTOVSCROLL) as u32,
        WS_EX_CLIENTEDGE,
        content_x,
        prompt_top + PROMPT_HEAD_H,
        content_w,
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
        content_x,
        footer.reset_btn_y,
        PROMPT_TOGGLE_W,
        reset_btn_h,
        ID_PROMPT_TOGGLE,
    );
    ctx.set_anchor(toggle_btn, Anchor::ResetBtn);
    let reset_btn = ctx.create(
        WC_BUTTON,
        "&Reset prompt to default",
        BS_PUSHBUTTON as u32,
        0,
        content_x + PROMPT_TOGGLE_W + COL_GAP,
        footer.reset_btn_y,
        200,
        reset_btn_h,
        ID_RESET_PROMPT,
    );
    ctx.set_anchor(reset_btn, Anchor::ResetBtn);
    unsafe {
        let _ = ShowWindow(reset_btn, SW_HIDE);
    }
    // What the prompt is for, under the title. Made after the box so the box
    // is named by the title and not by this line.
    ctx.create(
        WC_STATIC,
        PROMPT_NOTE,
        0,
        0,
        content_x,
        prompt_top + ROW_PAD + ROW_TITLE_H,
        content_w,
        DESC_LINE_H,
        ID_ROW_DESC,
    );

    // -- Status line and Undo ------------------------------------------
    // Pinned footer: a sibling of the body, not inside it, so it never scrolls.
    // It belongs to no page and sits under the pages, right of the rail.
    ctx.parent.set(hwnd);
    ctx.page.set(None);
    let buttons_y = footer.buttons_y;
    // #344, #875: the status line. "Saved: Card corner", what is wrong with a
    // row, or why a change was not saved; up to three lines, left of Undo.
    let validation = ctx.create(
        WC_STATIC,
        "",
        0,
        0,
        RAIL_W_DP + content_x + MARGIN,
        buttons_y - MARGIN + STATUS_PAD_DP,
        content_w - 2 * MARGIN - BTN_W - 12,
        FOOTER_H_DP - 2 * STATUS_PAD_DP,
        ID_VALIDATION_MESSAGE,
    );
    ctx.set_anchor(validation, Anchor::Footer);
    // #778: a key an environment variable overrides cannot be edited here.
    lock_env_key_fields(hwnd);
    // #875: Undo restores the latest change. It shows only once that change was
    // saved, and never for a key (whose old value is not kept).
    let undo = ctx.create(
        WC_BUTTON,
        "Undo",
        BS_PUSHBUTTON as u32,
        0,
        WIN_W_DP - MARGIN - BTN_W,
        buttons_y,
        BTN_W,
        BTN_H,
        ID_UNDO,
    );
    ctx.set_anchor(undo, Anchor::Footer);
    unsafe {
        let _ = ShowWindow(undo, SW_HIDE);
    }

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
        ID_UNDO,
        ID_AUTOSTART,
        ID_MODE,
        ID_KEY_MODE,
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
        ID_KEY_MODE,
        ID_DOUBLE_PRESS,
        ID_CLICK_AWAY,
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
        ID_UNDO,
    ] {
        if let Some(h) = get_dlg_item(hwnd, id) {
            add_style(h, WS_TABSTOP.0);
        }
    }

    annotate_accessible_names(hwnd);

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
        models_flow: ctx.flow.into_inner(),
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
const ODS_DISABLED: u32 = 0x0004;
const ODS_COMBOBOXEDIT: u32 = 0x1000;
const ODT_COMBOBOX: u32 = 3;
const ODS_NOFOCUSRECT: u32 = 0x0200;
const WC_LISTBOX: &str = "LISTBOX";

/// Posted to the window to change the page or move focus between the rail and
/// the page. `wparam` says what (the `PAGE_*` codes), `lparam` is its argument.
const WM_SETTINGS_PAGE: u32 = 0x8000 + 45; // WM_APP + 45
/// #875: the app's answer for a write, sent to the window with a
/// [`WriteAnswer`] in `lparam`. The window answers `1` when a row took it.
const WM_SETTINGS_WRITE_RESULT: u32 = 0x8000 + 46; // WM_APP + 46
/// #875: the worker that re-reads the file after an activation has its answer
/// ready in `refresh_slot`.
const WM_SETTINGS_REFRESH: u32 = 0x8000 + 47; // WM_APP + 47

/// Posted when the Start with Windows worker has stored its answer (#1105).
const WM_SETTINGS_AUTOSTART: u32 = 0x8000 + 52; // WM_APP + 52

/// Posted when the open-time read of the file has stored its answer (#1105).
const WM_SETTINGS_OPENED: u32 = 0x8000 + 53; // WM_APP + 53

/// Posted when the Actions page's worker has stored its answer (#1152).
const WM_SETTINGS_ACTIONS: u32 = 0x8000 + 54; // WM_APP + 54
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

/// #1055: everything a rail item paint needs that costs a registry read or a
/// GDI font: both colour sets and the icon font. Built once per theme, accent
/// or dpi change; owned here and freed by [`invalidate_rail_style`].
#[derive(Clone, Copy)]
struct RailStyle {
    dpi: u32,
    high_contrast: bool,
    normal: RailColors,
    selected: RailColors,
    /// The focus ring's ink.
    focus_ink: u32,
    icons: HFONT,
}

thread_local! {
    static RAIL_STYLE: std::cell::Cell<Option<RailStyle>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    /// How many times the rail style was rebuilt (a registry read and a font
    /// each) on this thread: painting must not move it.
    static RAIL_STYLE_BUILDS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The cached rail style for `dpi`, built on first use after a change.
unsafe fn rail_style(dpi: u32) -> RailStyle {
    use crate::ui::theme::{accent_fill, read_accent_shades, Theme, Tokens, ICON_FONT_DP};
    if let Some(style) = RAIL_STYLE.get() {
        if style.dpi == dpi {
            return style;
        }
        invalidate_rail_style();
    }
    #[cfg(test)]
    RAIL_STYLE_BUILDS.set(RAIL_STYLE_BUILDS.get() + 1);
    let shared = edit_style();
    let (theme, sys, face) = (shared.look.theme, shared.look.sys, shared.face);
    let high_contrast = theme == Theme::HighContrast;
    let tokens = Tokens::for_theme(theme, &sys);
    let accent = if high_contrast {
        face
    } else {
        accent_fill(theme, read_accent_shades(), face)
    };
    let style = RailStyle {
        dpi,
        high_contrast,
        normal: rail_colors(false, high_contrast, face, &tokens, &sys, face),
        selected: rail_colors(true, high_contrast, face, &tokens, &sys, accent),
        focus_ink: tokens.text,
        icons: icon_font(ICON_FONT_DP, dpi),
    };
    RAIL_STYLE.set(Some(style));
    style
}

/// Drops the cached rail style and frees its font; the next paint rebuilds
/// it. Called on a theme, accent, system colour or dpi change, and when
/// Settings closes.
unsafe fn invalidate_rail_style() {
    if let Some(style) = RAIL_STYLE.take() {
        let _ = DeleteObject(HGDIOBJ(style.icons.0));
    }
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

/// The area a combo item's text is drawn in: the item's rect with
/// `COMBO_TEXT_PAD_DP` kept clear on each side. Never inverted, however narrow
/// the item.
fn combo_text_rect(
    rc: windows::Win32::Foundation::RECT,
    dpi: u32,
) -> windows::Win32::Foundation::RECT {
    let pad = to_px(crate::ui::theme::COMBO_TEXT_PAD_DP, dpi);
    let left = rc.left + pad;
    windows::Win32::Foundation::RECT {
        left,
        top: rc.top,
        right: (rc.right - pad).max(left),
        bottom: rc.bottom,
    }
}

const COMBO_SUBCLASS_ID: usize = 0x854;

/// The Card corner combo, which the gallery puts the mouse over.
#[cfg(debug_assertions)]
pub(crate) const CARD_CORNER_COMBO: i32 = ID_CARD_CORNER;

/// `WM_MOUSELEAVE`, sent once after `TrackMouseEvent(TME_LEAVE)`.
const WM_MOUSELEAVE: u32 = 0x02A3;
const WM_ENABLE: u32 = 0x000A;
const WM_SHOWWINDOW: u32 = 0x0018;
const WM_CAPTURECHANGED: u32 = 0x0215;

thread_local! {
    /// #1109: the combo the mouse is over (0: none). One mouse, one hover.
    static HOVERED_COMBO: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };
    /// How many times a hover change repainted a combo, for the tests.
    #[cfg(test)]
    static COMBO_HOVER_REPAINTS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

fn combo_is_hovered(hwnd: HWND) -> bool {
    HOVERED_COMBO.get() == hwnd.0 as isize
}

#[cfg(test)]
fn combo_hover_repaints() -> u32 {
    COMBO_HOVER_REPAINTS.get()
}

/// Moves the hover to `hwnd` (`on`) or takes it off `hwnd`. Repaints the combos
/// that changed, and returns whether anything did: a mouse moving inside the
/// field changes nothing and costs nothing.
fn set_combo_hover(hwnd: HWND, on: bool) -> bool {
    let me = hwnd.0 as isize;
    let before = HOVERED_COMBO.get();
    let after = if on {
        me
    } else if before == me {
        0
    } else {
        before
    };
    if after == before {
        return false;
    }
    HOVERED_COMBO.set(after);
    #[cfg(test)]
    COMBO_HOVER_REPAINTS.set(COMBO_HOVER_REPAINTS.get() + 1);
    for changed in [before, after] {
        if changed != 0 {
            unsafe {
                let _ = RedrawWindow(
                    Some(HWND(changed as *mut c_void)),
                    None,
                    None,
                    windows::Win32::Graphics::Gdi::RDW_FRAME | RDW_INVALIDATE,
                );
            }
        }
    }
    true
}

/// Asks Windows for one `WM_MOUSELEAVE` when the mouse leaves `hwnd`.
fn track_mouse_leave(hwnd: HWND) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
    };
    let mut track = TRACKMOUSEEVENT {
        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
        dwFlags: TME_LEAVE,
        hwndTrack: hwnd,
        dwHoverTime: 0,
    };
    unsafe {
        let _ = TrackMouseEvent(&mut track);
    }
}

/// Gives the gallery a hovered combo: the state a mouse over it leaves.
#[cfg(debug_assertions)]
fn hover_combo_for_gallery(combo: HWND) {
    set_combo_hover(combo, true);
}

/// An owner-drawn combo loses the system's light themed frame, so the frame is
/// put back after each paint in the text box's look (#934): the same border,
/// corners and accent line, which also keeps a row of controls consistent.
unsafe extern "system" fn combo_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    match msg {
        WM_PAINT => {
            let result = DefSubclassProc(hwnd, msg, wparam, lparam);
            paint_edit_frame(hwnd);
            result
        }
        // #1109: the mouse over the field hovers it; Windows says when it leaves.
        // Only a change repaints, and nothing runs while the mouse is still.
        windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE => {
            if set_combo_hover(hwnd, true) {
                track_mouse_leave(hwnd);
            }
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_MOUSELEAVE => {
            set_combo_hover(hwnd, false);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        // #1143: Windows does not always send `WM_MOUSELEAVE` to a combo that is
        // disabled, hidden or has lost the mouse capture to its list, so those
        // end the hover themselves. (The list opening is `CBN_DROPDOWN`, which
        // the window handles.)
        WM_ENABLE | WM_SHOWWINDOW if wparam.0 == 0 => {
            set_combo_hover(hwnd, false);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_CAPTURECHANGED => {
            set_combo_hover(hwnd, false);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        WM_NCDESTROY => {
            set_combo_hover(hwnd, false);
            let _ = RemoveWindowSubclass(hwnd, Some(combo_subclass), COMBO_SUBCLASS_ID);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

const COMBO_LIST_SUBCLASS_ID: usize = 0x1164;

/// Gives the drop list of `combo` the Settings look: its frame and the fill
/// behind its rows (#1169). The list window exists from the combo's creation, so
/// it is subclassed once here and goes away with the combo.
unsafe fn subclass_combo_list(combo: HWND) {
    use windows::Win32::UI::Controls::COMBOBOXINFO;
    use windows::Win32::UI::WindowsAndMessaging::CB_GETCOMBOBOXINFO;
    let mut info = COMBOBOXINFO {
        cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
        ..Default::default()
    };
    SendMessageW(
        combo,
        CB_GETCOMBOBOXINFO,
        Some(WPARAM(0)),
        Some(LPARAM(&mut info as *mut COMBOBOXINFO as isize)),
    );
    if !info.hwndList.is_invalid() {
        let _ = windows::Win32::UI::Shell::SetWindowSubclass(
            info.hwndList,
            Some(combo_list_subclass),
            COMBO_LIST_SUBCLASS_ID,
            0,
        );
    }
}

/// The drop list's own frame and background: the border colour of the closed
/// combo around it, and the list fill where no row reaches (#1169). Only cached
/// brushes. A list long enough for a scroll bar lets the system draw that first.
/// The list is a top-level popup, so `dpi_for` reads its own monitor's scale on
/// every paint (nothing is cached per DPI); the cached colours are dropped on a
/// theme change and the list repaints whole each time it opens.
unsafe extern "system" fn combo_list_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{FillRect, GetWindowDC, ReleaseDC, HDC};
    use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClientRect, GetWindowLongPtrW, GWL_STYLE, SM_CXBORDER, WM_ERASEBKGND, WM_NCDESTROY,
        WM_NCPAINT,
    };
    match msg {
        WM_ERASEBKGND => {
            let mut rc = RECT::default();
            if GetClientRect(hwnd, &mut rc).is_ok() {
                FillRect(HDC(wparam.0 as *mut c_void), &rc, edit_style().brush);
                LRESULT(1)
            } else {
                DefSubclassProc(hwnd, msg, wparam, lparam)
            }
        }
        WM_NCPAINT => {
            let scrolls = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32 & WS_VSCROLL.0;
            let result = if scrolls != 0 {
                DefSubclassProc(hwnd, msg, wparam, lparam)
            } else {
                LRESULT(0)
            };
            let mut wr = RECT::default();
            let hdc = GetWindowDC(Some(hwnd));
            if !hdc.is_invalid() {
                if GetWindowRect(hwnd, &mut wr).is_ok() {
                    // The ring the system reserves for the frame, in the border
                    // colour of the closed combo.
                    let (w, h) = (wr.right - wr.left, wr.bottom - wr.top);
                    let edge = GetSystemMetricsForDpi(SM_CXBORDER, dpi_for(hwnd)).max(1);
                    let brush = edit_style().hairline_brush;
                    let side = |left, top, right, bottom| RECT {
                        left,
                        top,
                        right,
                        bottom,
                    };
                    FillRect(hdc, &side(0, 0, w, edge), brush);
                    FillRect(hdc, &side(0, h - edge, w, h), brush);
                    FillRect(hdc, &side(0, 0, edge, h), brush);
                    FillRect(hdc, &side(w - edge, 0, w, h), brush);
                }
                ReleaseDC(Some(hwnd), hdc);
            }
            result
        }
        WM_NCDESTROY => {
            let _ = RemoveWindowSubclass(hwnd, Some(combo_list_subclass), COMBO_LIST_SUBCLASS_ID);
            DefSubclassProc(hwnd, msg, wparam, lparam)
        }
        _ => DefSubclassProc(hwnd, msg, wparam, lparam),
    }
}

/// The height of a dropdown's list items: a row less the frame.
fn combo_item_h_px(dpi: u32) -> i32 {
    to_px(ROW_H, dpi) - to_px(COMBO_FRAME_DP, dpi)
}

/// The frame a combo adds around its selection field, in dp, as a first guess;
/// [`set_combo_heights`] measures the real one.
const COMBO_FRAME_DP: i32 = 4;

fn is_combo(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
    let mut buf = [0u16; 16];
    let n = unsafe { GetClassNameW(hwnd, &mut buf) }.max(0) as usize;
    String::from_utf16_lossy(&buf[..n.min(buf.len())]).eq_ignore_ascii_case(WC_COMBOBOX)
}

/// Sets the item height of an owner-drawn combo: the list items (index 0) and
/// the closed selection field (-1). The closed combo is its field plus a frame
/// of the system's choosing, so the field is sized to leave the combo `ROW_H`
/// tall, like the controls beside it.
unsafe fn set_combo_heights(combo: HWND, dpi: u32) {
    use windows::Win32::UI::WindowsAndMessaging::CB_SETITEMHEIGHT;
    let set = |index: usize, h: i32| {
        SendMessageW(
            combo,
            CB_SETITEMHEIGHT,
            Some(WPARAM(index)),
            Some(LPARAM(h.max(1) as isize)),
        );
    };
    let want = to_px(ROW_H, dpi);
    set(0, combo_item_h_px(dpi));
    // Measure the frame with a field of the wanted height, then take it off.
    set(usize::MAX, want);
    let mut rc = windows::Win32::Foundation::RECT::default();
    let frame = if GetWindowRect(combo, &mut rc).is_ok() {
        (rc.bottom - rc.top - want).clamp(0, want / 2)
    } else {
        to_px(COMBO_FRAME_DP, dpi)
    };
    set(usize::MAX, want - frame);
}

/// `WM_MEASUREITEM` for a combo. Returns false for anything else.
unsafe fn measure_combo_item(lparam: LPARAM, dpi: u32) -> bool {
    let mis = &mut *(lparam.0 as *mut windows::Win32::UI::Controls::MEASUREITEMSTRUCT);
    if mis.CtlType.0 != ODT_COMBOBOX {
        return false;
    }
    mis.itemHeight = combo_item_h_px(dpi).max(1) as u32;
    true
}

/// The hover pill of a list row (#1169): inset like a Windows 11 menu's, with
/// 4 dp corners, filled with the cached hover brush. High contrast fills the
/// whole row square, as every native list does. Nothing is created or freed.
unsafe fn draw_item_pill(
    hdc: windows::Win32::Graphics::Gdi::HDC,
    rc: windows::Win32::Foundation::RECT,
    dpi: u32,
    style: &EditStyle,
) {
    use crate::ui::theme::{COMBO_ITEM_INSET_Y_DP, CONTROL_RADIUS_DP, MENU_EDGE_PAD_DP};
    use windows::Win32::Graphics::Gdi::{
        FillRect, GetStockObject, RoundRect, SelectObject, NULL_PEN,
    };
    if style.look.theme == crate::ui::theme::Theme::HighContrast {
        FillRect(hdc, &rc, style.item_hover_brush);
        return;
    }
    let (inset_x, inset_y) = (
        to_px(MENU_EDGE_PAD_DP, dpi),
        to_px(COMBO_ITEM_INSET_Y_DP, dpi),
    );
    let round = 2 * to_px(CONTROL_RADIUS_DP, dpi);
    let old_brush = SelectObject(hdc, HGDIOBJ(style.item_hover_brush.0));
    let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
    // A null pen leaves the right and bottom edges off, so grow them by one.
    let _ = RoundRect(
        hdc,
        rc.left + inset_x,
        rc.top + inset_y,
        rc.right - inset_x + 1,
        rc.bottom - inset_y + 1,
        round,
        round,
    );
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_brush);
}

/// `WM_DRAWITEM` for a combo: the item (or the closed field), its text kept
/// `COMBO_TEXT_PAD_DP` off the border. Returns false for anything else. Colours
/// come from the cached [`edit_style`] (system colours in high contrast) or the
/// system's own brushes, so nothing is created per paint.
unsafe fn draw_combo_item(lparam: LPARAM) -> bool {
    use windows::Win32::Foundation::COLORREF;
    use windows::Win32::Graphics::Gdi::{
        DrawTextW, FillRect, GetSysColor, GetSysColorBrush, SelectObject, SetBkMode, SetTextColor,
        COLOR_3DFACE, COLOR_GRAYTEXT, COLOR_WINDOW, COLOR_WINDOWTEXT, DT_END_ELLIPSIS, DT_LEFT,
        DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, TRANSPARENT,
    };
    use windows::Win32::UI::WindowsAndMessaging::{CB_GETLBTEXT, CB_GETLBTEXTLEN, WM_GETFONT};

    let di = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
    if di.CtlType.0 != ODT_COMBOBOX {
        return false;
    }
    let hdc = di.hDC;
    let rc = di.rcItem;
    let state = di.itemState.0;
    let disabled = state & ODS_DISABLED != 0;
    // Only a list item highlights. The closed field gets ODS_SELECTED while the
    // combo has the focus, and keeps the window colours; the focus line the
    // frame draws is its focus mark.
    let in_field = state & ODS_COMBOBOXEDIT != 0;
    let selected = state & ODS_SELECTED != 0 && !disabled && !in_field;
    // A list row lies on the text box fill and its hover is a rounded pill, as
    // in Windows 11's own lists (#1169); the closed field and a disabled combo
    // keep the system colours.
    let ink = if !in_field && !disabled {
        let style = edit_style();
        FillRect(hdc, &rc, style.brush);
        if selected {
            draw_item_pill(hdc, rc, dpi_for(di.hwndItem), &style);
            style.colors.item_text_hot
        } else {
            style.colors.text
        }
    } else {
        let (face, ink) = if disabled {
            (COLOR_3DFACE, COLOR_GRAYTEXT)
        } else {
            (COLOR_WINDOW, COLOR_WINDOWTEXT)
        };
        FillRect(hdc, &rc, GetSysColorBrush(face));
        GetSysColor(ink)
    };

    // An empty combo asks to be drawn with item id -1: just the background.
    let Ok(index) = usize::try_from(di.itemID as i32) else {
        return true;
    };
    let len = SendMessageW(di.hwndItem, CB_GETLBTEXTLEN, Some(WPARAM(index)), None).0;
    let Ok(len) = usize::try_from(len) else {
        return true;
    };
    let mut text = vec![0u16; len + 1];
    let got = SendMessageW(
        di.hwndItem,
        CB_GETLBTEXT,
        Some(WPARAM(index)),
        Some(LPARAM(text.as_mut_ptr() as isize)),
    )
    .0;
    let Ok(got) = usize::try_from(got) else {
        return true;
    };
    text.truncate(got.min(len));

    let dpi = dpi_for(di.hwndItem);
    let mut area = combo_text_rect(rc, dpi);
    let font = SendMessageW(di.hwndItem, WM_GETFONT, None, None).0;
    let old = (font != 0).then(|| SelectObject(hdc, HGDIOBJ(font as *mut c_void)));
    SetBkMode(hdc, TRANSPARENT);
    SetTextColor(hdc, COLORREF(ink));
    DrawTextW(
        hdc,
        &mut text,
        &mut area,
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
    );
    if let Some(old) = old {
        SelectObject(hdc, old);
    }
    true
}

/// `WM_MEASUREITEM`: every rail item is the same height.
unsafe fn measure_rail_item(lparam: LPARAM, dpi: u32) {
    let mis = &mut *(lparam.0 as *mut windows::Win32::UI::Controls::MEASUREITEMSTRUCT);
    if mis.CtlID == ID_RAIL as u32 {
        mis.itemHeight = to_px(RAIL_ITEM_H_DP, dpi).max(1) as u32;
    }
}

/// Whether a `WM_DRAWITEM` is for the rail (the only owner-drawn control the
/// main window itself hosts).
unsafe fn is_rail_draw_item(lparam: LPARAM) -> bool {
    let di = &*(lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT);
    di.CtlID == ID_RAIL as u32
}

/// `WM_DRAWITEM` for the rail: one item.
unsafe fn draw_rail_item(lparam: LPARAM) {
    use crate::ui::theme::{
        CONTROL_RADIUS_DP, FOCUS_LINE_DP, RAIL_ICON_X_DP, RAIL_INSET_DP, RAIL_PILL_H_DP,
        RAIL_PILL_W_DP, RAIL_TEXT_X_DP,
    };
    use windows::Win32::Foundation::{COLORREF, RECT};
    use windows::Win32::Graphics::Gdi::{
        CreateSolidBrush, FillRect, FrameRect, GetStockObject, RoundRect, SelectObject, SetBkMode,
        SetTextColor, DT_END_ELLIPSIS, DT_LEFT, DT_NOPREFIX, DT_SINGLELINE, DT_VCENTER, NULL_PEN,
        TRANSPARENT,
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

    // #1055: the colours and the icon font are cached, so a paint reads
    // neither the registry nor builds a font.
    let style = rail_style(dpi);
    let high_contrast = style.high_contrast;
    let colors = if selected {
        style.selected
    } else {
        style.normal
    };

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
    let old_font = SelectObject(hdc, HGDIOBJ(style.icons.0));
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

    // A focus ring only when Windows says to draw one (keyboard use).
    if di.itemState.0 & ODS_FOCUS != 0 && di.itemState.0 & ODS_NOFOCUSRECT == 0 {
        let ring = to_px(FOCUS_LINE_DP, dpi).max(1);
        let b = CreateSolidBrush(COLORREF(style.focus_ink));
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
    /// #393: the key-mode combo's text, the press-time box's text and the
    /// click-away checkbox.
    key_mode: String,
    double_press_text: String,
    click_away_dismiss: bool,
}

/// The Active provider combo's selection as `RawForm::provider_choice`. A
/// control that was never created, or one with nothing selected, is
/// `usize::MAX` (no choice), which keeps the stored order. A null window would
/// read 0, which is "ChatGPT first" (#782).
fn provider_choice_from(cursel: Option<isize>) -> usize {
    match cursel {
        Some(i) if i >= 0 => i as usize,
        _ => usize::MAX,
    }
}

/// Reads every control into a [`RawForm`], without yet folding it into a
/// [`Config`]. Split out from [`read_form`] so Save can validate the raw
/// values (#344) before committing to `build_config`'s fallback behavior.
fn read_raw_form(inner: &SettingsInner) -> RawForm {
    let hwnd = inner.hwnd;
    RawForm {
        provider_choice: provider_choice_from(
            get_dlg_item(hwnd, ID_ACTIVE_PROVIDER)
                .map(|h| unsafe { SendMessageW(h, CB_GETCURSEL, None, None).0 }),
        ),
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
        key_mode: combo_selected_text(hwnd, ID_KEY_MODE),
        double_press_text: get_dlg_item(hwnd, ID_DOUBLE_PRESS)
            .map(get_text)
            .unwrap_or_default(),
        click_away_dismiss: if get_dlg_item(hwnd, ID_CLICK_AWAY).is_some() {
            checkbox_checked(hwnd, ID_CLICK_AWAY)
        } else {
            inner.original.options.click_away_dismiss
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
    // The staged copy holds the chords learned here. The key mode and press time
    // are rows of their own (#393): the form's values, never the staged copy's.
    let (key_mode, double_press_ms) = (cfg.hotkeys.key_mode, cfg.hotkeys.double_press_ms);
    cfg.hotkeys = inner.hotkeys.clone();
    cfg.hotkeys.key_mode = key_mode;
    cfg.hotkeys.double_press_ms = double_press_ms;
    cfg
}

fn set_hotkey_status(hwnd: HWND, text: &str) {
    say(hwnd, ID_HOTKEY_STATUS, text);
}

/// Points the two shortcut labels at `hotkeys`.
fn set_hotkey_labels(hwnd: HWND, hotkeys: &Hotkeys) {
    if let Some(l) = get_dlg_item(hwnd, ID_PRIMARY_LABEL) {
        set_text(l, &primary_label(&hotkeys.primary));
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
            set_hotkey_status(inner.hwnd, &format!("Set to {}.", chord_to_string(&chord)));
            // #875: applied at once, like every other field.
            commit_field(
                inner,
                match which {
                    HK_PRIMARY => Field::PrimaryKey,
                    HK_SECONDARY => Field::SecondaryKey,
                    _ => Field::PauseKey,
                },
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

/// #393: the three key modes in the Settings combo, in display order. Short
/// enough for the control column; the row's note says the rest.
const KEY_MODE_LABELS: [&str; 3] = [
    "Picks an option or closes",
    "Always picks an option",
    "Always closes the card",
];

/// The label shown for a [`KeyMode`](crate::keypress::KeyMode). Inverse of
/// [`label_to_key_mode`].
fn key_mode_label(mode: crate::keypress::KeyMode) -> &'static str {
    use crate::keypress::KeyMode;
    match mode {
        KeyMode::Dynamic => KEY_MODE_LABELS[0],
        KeyMode::Pick => KEY_MODE_LABELS[1],
        KeyMode::Close => KEY_MODE_LABELS[2],
    }
}

/// Parses a combo selection back into a key mode. `None` for a blank or
/// unrecognized selection, so callers keep the original config value.
fn label_to_key_mode(label: &str) -> Option<crate::keypress::KeyMode> {
    use crate::keypress::KeyMode;
    [KeyMode::Dynamic, KeyMode::Pick, KeyMode::Close]
        .into_iter()
        .find(|&m| key_mode_label(m) == label)
}

/// The press-time row's title, unit and note, and the click-away row's title.
const DOUBLE_PRESS_LABEL: &str = "Dou&ble press time";
const DOUBLE_PRESS_UNIT: &str = "milliseconds";
const DOUBLE_PRESS_NOTE: &str = "Two presses this close pick option 2. 0 is off.";
const CLICK_AWAY_LABEL: &str = "Click away c&loses an answer or options card";

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
    crate::config::env_var_usable(var)
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
    say(hwnd, ID_VALIDATION_MESSAGE, &env_key_hint(&env_hint_vars()));
}

/// The names of the set key variables, for [`env_key_hint`].
fn env_hint_vars() -> Vec<&'static str> {
    env_locked_key_fields()
        .into_iter()
        .map(|(_, v)| v)
        .collect()
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

    // #393: a hand-written key mode the combo cannot show ("PICK ", a typo)
    // stays as written until the user picks a mode; a blank box keeps the number.
    if let Some(mode) = label_to_key_mode(&raw.key_mode) {
        if mode != original.hotkeys.key_mode_value() {
            cfg.hotkeys.key_mode = mode.name().to_string();
        }
    }
    cfg.hotkeys.double_press_ms =
        parse_u32_or(&raw.double_press_text, original.hotkeys.double_press_ms);
    cfg.options.click_away_dismiss = raw.click_away_dismiss;

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
    /// Declared after `raw`: restores the thread's DPI awareness once the
    /// window is destroyed by `Drop`.
    _unaware: Option<UnawareScope>,
}

/// Makes the thread DPI-unaware until dropped (the gallery's real 100%).
#[cfg(debug_assertions)]
struct UnawareScope(windows::Win32::UI::HiDpi::DPI_AWARENESS_CONTEXT);

#[cfg(debug_assertions)]
impl UnawareScope {
    fn enter() -> UnawareScope {
        use windows::Win32::UI::HiDpi::{
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_UNAWARE,
        };
        UnawareScope(unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_UNAWARE) })
    }
}

#[cfg(debug_assertions)]
impl Drop for UnawareScope {
    fn drop(&mut self) {
        unsafe { windows::Win32::UI::HiDpi::SetThreadDpiAwarenessContext(self.0) };
    }
}

#[cfg(debug_assertions)]
impl GalleryWindow {
    /// Opens the window and shows it without taking focus. `dpi` lays it out
    /// at that DPI whatever the monitor says (the gallery's `--dpi`); `None`
    /// uses the window's own.
    #[cfg(test)]
    pub fn open(instance: HINSTANCE, dpi: Option<u32>) -> Option<GalleryWindow> {
        GalleryWindow::open_themed(instance, dpi, None)
    }

    /// Opens the window and shows it without taking focus, under a forced theme
    /// (#582); `None` follows the system. `dpi` is as for the plain open. The
    /// forced theme lasts until the window is dropped. High contrast is drawn
    /// with Windows' black colours, since the gallery cannot switch the system
    /// theme; dark is drawn although real windows keep it off.
    pub fn open_themed(
        instance: HINSTANCE,
        dpi: Option<u32>,
        theme: Option<crate::ui::theme::Theme>,
    ) -> Option<GalleryWindow> {
        static ONCE: std::sync::Once = std::sync::Once::new();
        static OK: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        ONCE.call_once(|| {
            let _ = OK.set(unsafe { register_class_named(instance, GALLERY_CLASS_NAME) });
        });
        if !OK.get().copied().unwrap_or(false) {
            return None;
        }
        ensure_common_controls();
        // 100% is the one scale a window can really be drawn at on any
        // screen: made DPI-unaware, Windows gives it 96 DPI and its native
        // checkbox boxes and scrollbar come out at 96 DPI too (#1016, #1052).
        // The thread stays unaware until the window is dropped, so the
        // gallery's capture of it measures in the same units.
        let unaware = (dpi == Some(96)).then(UnawareScope::enter);
        let dpi = if unaware.is_some() { None } else { dpi };
        FORCED_DPI.with(|f| f.set(dpi.map(|d| d.max(1))));
        force_theme(theme);

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
            force_theme(None);
            unsafe { drop(Box::from_raw(raw)) };
            return None;
        };
        // From here the window owns nothing the Drop below cannot free.
        let gallery = GalleryWindow {
            raw,
            _unaware: unaware,
        };
        let inner = unsafe { &mut *raw };
        inner.hwnd = hwnd;
        // A simulated scale cannot change the native scrollbar, which keeps
        // the window's real DPI: remember that DPI so the width reserved for
        // the scrollbar is the width it really takes (#1052).
        NATIVE_DPI.with(|n| n.set(dpi.map(|_| unsafe { GetDpiForWindow(hwnd) }.max(1))));
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

    /// Puts `text` on the status line, with Undo when `undo` (what a saved change
    /// or a failed one shows).
    pub(crate) fn show_status(&mut self, text: &str, undo: bool) {
        let inner = unsafe { &mut *self.raw };
        set_status(inner, text, undo);
        unsafe { sync_visibility(inner) };
    }

    /// Picks the active provider on the Models page (what the combo does).
    pub(crate) fn pick_provider(&mut self, provider: ActiveProvider) {
        let inner = unsafe { &mut *self.raw };
        if let Some(combo) = get_dlg_item(inner.hwnd, ID_ACTIVE_PROVIDER) {
            combo_select_index(combo, provider.index() as i32);
            unsafe { sync_models_flow(inner) };
        }
    }

    /// Puts the mouse over the combo `id` (what hovering it leaves), for the gallery.
    pub(crate) fn hover_combo(&mut self, id: i32) {
        let inner = unsafe { &mut *self.raw };
        if let Some(combo) = get_dlg_item(inner.hwnd, id) {
            hover_combo_for_gallery(combo);
        }
    }

    /// Drops the list of the combo `id` open (what a click on it does), for the gallery.
    pub(crate) fn open_combo_list(&mut self, id: i32) {
        let inner = unsafe { &mut *self.raw };
        if let Some(combo) = get_dlg_item(inner.hwnd, id) {
            unsafe {
                SendMessageW(
                    combo,
                    windows::Win32::UI::WindowsAndMessaging::CB_SHOWDROPDOWN,
                    Some(WPARAM(1)),
                    Some(LPARAM(0)),
                );
            }
        }
    }

    /// Ticks the Start with Windows checkbox and gives it the keyboard focus
    /// with the focus cue showing (what Tab onto it leaves), for the gallery.
    pub(crate) fn focus_checked_autostart(&mut self) {
        const WM_UPDATEUISTATE: u32 = 0x0128;
        const BM_SETCHECK: u32 = 0x00F1;
        let inner = unsafe { &mut *self.raw };
        if let Some(check) = get_dlg_item(inner.hwnd, ID_AUTOSTART) {
            unsafe {
                SendMessageW(check, BM_SETCHECK, Some(WPARAM(1)), None);
                let _ = windows::Win32::UI::Input::KeyboardAndMouse::SetFocus(Some(check));
                // UIS_CLEAR (2) of UISF_HIDEFOCUS (1): show the cue.
                SendMessageW(check, WM_UPDATEUISTATE, Some(WPARAM(2 | (1 << 16))), None);
            }
        }
    }

    /// The window of the open drop list of the combo `id`, if it is showing.
    pub(crate) fn combo_list_hwnd(&self, id: i32) -> Option<HWND> {
        use windows::Win32::UI::Controls::COMBOBOXINFO;
        use windows::Win32::UI::WindowsAndMessaging::{IsWindowVisible, CB_GETCOMBOBOXINFO};
        let inner = unsafe { &*self.raw };
        let combo = get_dlg_item(inner.hwnd, id)?;
        let mut info = COMBOBOXINFO {
            cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                combo,
                CB_GETCOMBOBOXINFO,
                Some(WPARAM(0)),
                Some(LPARAM(&mut info as *mut COMBOBOXINFO as isize)),
            );
            (!info.hwndList.is_invalid() && IsWindowVisible(info.hwndList).as_bool())
                .then_some(info.hwndList)
        }
    }

    /// Shows every provider (what "Add another provider" does).
    pub(crate) fn reveal_providers(&mut self) {
        let inner = unsafe { &mut *self.raw };
        unsafe { reveal_providers(inner) };
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
        NATIVE_DPI.with(|n| n.set(None));
        unsafe {
            let _ = DestroyWindow((*self.raw).hwnd);
            drop(Box::from_raw(self.raw));
        }
        force_theme(None);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::Theme;
    use windows::Win32::UI::WindowsAndMessaging::GW_HWNDPREV;

    #[test]
    fn an_older_accepted_list_never_overwrites_a_newer_saved_one() {
        use crate::provider::model_list::Vendor;
        let sync = CloudSync::default();
        save_accepted_list(&sync, 2, 5, &["newer".to_string()]);
        // Both were accepted in their time; the older one finishes writing last.
        save_accepted_list(&sync, 2, 4, &["older".to_string()]);
        assert_eq!(
            crate::provider::model_cache::load(Vendor::Gemini),
            Some(vec!["newer".to_string()])
        );
        save_accepted_list(&sync, 2, 6, &["newest".to_string()]);
        assert_eq!(
            crate::provider::model_cache::load(Vendor::Gemini),
            Some(vec!["newest".to_string()])
        );
    }

    #[test]
    fn a_superseded_worker_does_not_overwrite_the_saved_list() {
        use crate::provider::model_list::Vendor;
        let slots: CloudModels = Default::default();
        let sync = CloudSync::default();
        // Worker 1 started, then worker 2 replaced it.
        sync.generation[1].store(2, Ordering::SeqCst);
        assert!(store_fetched_list(
            &slots,
            &sync,
            1,
            2,
            "k",
            vec!["from-worker-2".into()]
        ));
        // Worker 1 returns last: dropped, and the saved copy stays worker 2's.
        assert!(!store_fetched_list(
            &slots,
            &sync,
            1,
            1,
            "k",
            vec!["from-worker-1".into()]
        ));
        assert_eq!(
            crate::provider::model_cache::load(Vendor::Anthropic),
            Some(vec!["from-worker-2".to_string()])
        );
    }

    #[test]
    fn a_saved_list_never_replaces_a_fetched_one_or_a_newer_fetch() {
        let slots: CloudModels = Default::default();
        let sync = CloudSync::default();
        sync.generation[0].store(2, Ordering::SeqCst);
        // An older worker's saved list is dropped.
        assert!(!store_cached_list(&slots, &sync, 0, 1, vec!["old".into()]));
        assert!(store_cached_list(&slots, &sync, 0, 2, vec!["saved".into()]));
        // It does not mark the key as fetched, so a failed refresh retries.
        assert_eq!(sync.fetched_keys.lock().unwrap()[0], "");
        // A fetched list replaces it; a later saved list does not replace that.
        assert!(store_fetched_list(
            &slots,
            &sync,
            0,
            2,
            "k",
            vec!["live".into()]
        ));
        assert!(!store_cached_list(
            &slots,
            &sync,
            0,
            2,
            vec!["saved".into()]
        ));
        assert_eq!(slots.lock().unwrap()[0], Some(vec!["live".to_string()]));
    }

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
            for id in [ID_UNDO, ID_VALIDATION_MESSAGE] {
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
            assert_eq!(untagged, 2, "only Undo and the status line");
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
            assert!(
                is_shown(hwnd, ID_VALIDATION_MESSAGE),
                "the footer is on every page"
            );

            // Ctrl+3: Privacy.
            unsafe { handle_page_request(inner, PAGE_PICK, 2) };
            assert_eq!(inner.page, Page::Privacy);
            assert!(is_shown(hwnd, ID_REDACT));
            assert!(is_shown(hwnd, ID_MAX_EDGE));
            assert!(!is_shown(hwnd, ID_AUTOSTART));
            assert!(is_shown(hwnd, ID_VALIDATION_MESSAGE));
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
            // The card is under the Ollama block, which shows on request.
            unsafe { reveal_providers(inner) };
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
                    let need = page_max_dp(inner, page) + FOOTER_H_DP + NC_HEIGHT_GUESS_DP;
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

    // -- rows, names and mnemonics (#877) -----------------------------------

    /// The Alt letter a text declares (`&x`, lower case), if any. `&&` is a
    /// literal ampersand.
    fn mnemonic_of(text: &str) -> Option<char> {
        let mut chars = text.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '&' {
                match chars.next() {
                    Some('&') | None => {}
                    Some(m) => return Some(m.to_ascii_lowercase()),
                }
            }
        }
        None
    }

    /// The words a screen reader says for a text: without the mnemonic mark
    /// and the colon a title used to end in.
    fn spoken(text: &str) -> String {
        text.replace("&&", "\u{1}")
            .replace('&', "")
            .replace('\u{1}', "&")
            .trim_end_matches(':')
            .to_string()
    }

    unsafe fn class_of(hwnd: HWND) -> String {
        use windows::Win32::UI::WindowsAndMessaging::GetClassNameW;
        let mut name = [0u16; 32];
        let n = GetClassNameW(hwnd, &mut name) as usize;
        String::from_utf16_lossy(&name[..n])
    }

    #[test]
    fn the_mnemonic_helpers_read_the_ampersand() {
        assert_eq!(mnemonic_of("&Mode"), Some('m'));
        assert_eq!(mnemonic_of("Card m&onitor"), Some('o'));
        assert_eq!(mnemonic_of("Fish && &Chips"), Some('c'));
        assert_eq!(mnemonic_of("No mark"), None);
        assert_eq!(mnemonic_of("Trailing &"), None);
        assert_eq!(spoken("OpenAI &model"), "OpenAI model");
        assert_eq!(spoken("Fish && Chips:"), "Fish & Chips");
    }

    /// No two labels on a page share an Alt letter (a letter may repeat across
    /// pages: a hidden page is unreachable), and every page with settings has
    /// some. A repeated letter makes Alt+letter cycle instead of going to the
    /// row asked for.
    #[test]
    fn mnemonics_are_unique_within_each_page() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            for page in Page::ALL {
                let mut seen: Vec<(char, String)> = Vec::new();
                for c in inner.children.iter().filter(|c| c.page == Some(page)) {
                    let text = get_text(c.hwnd);
                    let Some(m) = mnemonic_of(&text) else {
                        continue;
                    };
                    if let Some((_, other)) = seen.iter().find(|(k, _)| *k == m) {
                        panic!("{page:?}: Alt+{m} is on both {other:?} and {text:?}");
                    }
                    seen.push((m, text));
                }
                if page != Page::Actions {
                    assert!(!seen.is_empty(), "{page:?} has no mnemonics");
                }
            }
        });
    }

    /// The Gemini thinking row is greyed for a 2.5 model. Its Alt letter then
    /// goes nowhere: the focus stays where it was, it never lands on whatever
    /// control sits next to the greyed row.
    #[test]
    fn the_alt_letter_of_a_greyed_row_does_not_move_focus_to_a_neighbour() {
        use windows::Win32::UI::Input::KeyboardAndMouse::{IsWindowEnabled, SetFocus};
        use windows::Win32::UI::WindowsAndMessaging::{IsDialogMessageW, MSG, WM_SYSCHAR};
        let mut cfg = Config::default();
        cfg.providers.order = vec!["gemini".to_string()];
        cfg.providers.gemini.model = "gemini-2.5-pro".to_string();
        with_window(cfg, |hwnd, raw| {
            unsafe { show_page(&mut *raw, Page::Models) };
            sync_gemini_thinking_enabled(hwnd);
            let label = get_dlg_item(hwnd, ID_GEMINI_EFFORT_LABEL).expect("label");
            let m = mnemonic_of(&get_text(label)).expect("the label has a mnemonic");
            let combo = get_dlg_item(hwnd, ID_GEMINI_MODEL).expect("combo");
            assert!(!unsafe { IsWindowEnabled(label) }.as_bool(), "row greyed");
            unsafe {
                let _ = SetFocus(Some(combo));
                let before = GetFocus();
                assert_eq!(before, combo, "the test needs a real focus to compare");
                let msg = MSG {
                    hwnd: combo,
                    message: WM_SYSCHAR,
                    wParam: WPARAM(m as usize),
                    lParam: LPARAM(0),
                    ..Default::default()
                };
                let _ = IsDialogMessageW(hwnd, &msg);
                assert_eq!(GetFocus(), before, "Alt+{m} moved the focus");
            }
        });
    }

    /// A box (a combo, a text box or a slider) is named by the static title
    /// right before it in z-order, and a button by its text or its assigned
    /// name. On a page no two share a name, so a screen reader never says the
    /// same thing for two controls (#781). The Models page holds the hidden
    /// providers too: all three are checked.
    #[test]
    fn controls_are_named_by_the_title_before_them_and_no_name_repeats_on_a_page() {
        use windows::Win32::UI::WindowsAndMessaging::GW_HWNDPREV;
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            for page in Page::ALL {
                let mut names: Vec<String> = Vec::new();
                let mut boxes = 0;
                for c in inner.children.iter().filter(|c| c.page == Some(page)) {
                    let id = unsafe { GetDlgCtrlID(c.hwnd) };
                    let class = unsafe { class_of(c.hwnd) };
                    let name = match class.as_str() {
                        "ComboBox" | "Edit" | "msctls_trackbar32" => {
                            boxes += 1;
                            let before = unsafe { GetWindow(c.hwnd, GW_HWNDPREV) }
                                .unwrap_or_else(|_| panic!("id {id} has nothing before it"));
                            assert_eq!(
                                unsafe { class_of(before) },
                                "Static",
                                "id {id} on {page:?} is not named by a title"
                            );
                            let before_id = unsafe { GetDlgCtrlID(before) };
                            assert!(
                                before_id != ID_ROW_DESC && before_id != ID_ROW_SEPARATOR,
                                "id {id} on {page:?} would be named by a description or a line"
                            );
                            let title = spoken(&get_text(before));
                            assert!(!title.is_empty(), "id {id} on {page:?} has an empty title");
                            title
                        }
                        "Button" => ACCESSIBLE_NAMES
                            .iter()
                            .find(|(i, _)| *i == id)
                            .map(|(_, n)| n.to_string())
                            .unwrap_or_else(|| spoken(&get_text(c.hwnd))),
                        _ => continue,
                    };
                    assert!(
                        !names.contains(&name),
                        "{page:?}: two controls are named {name:?}"
                    );
                    names.push(name);
                }
                if page != Page::Actions {
                    assert!(boxes > 0, "{page:?} was scanned and had no boxes");
                }
            }
        });
    }

    /// The observable of #877: Narrator reads "OpenAI model" and then the combo.
    /// The title is the static before the combo, so its text is the name.
    #[test]
    fn the_openai_model_combo_is_named_openai_model() {
        use windows::Win32::UI::WindowsAndMessaging::GW_HWNDPREV;
        with_window(Config::default(), |hwnd, _raw| {
            for (id, want) in [
                (ID_OPENAI_KEY, "OpenAI API key"),
                (ID_OPENAI_MODEL, "OpenAI model"),
                (ID_OPENAI_EFFORT, "OpenAI thinking time"),
                (ID_ANTHROPIC_MODEL, "Anthropic model"),
                (ID_GEMINI_MODEL, "Gemini model"),
                (ID_OLLAMA_MODEL, "Ollama model"),
            ] {
                let control = get_dlg_item(hwnd, id).expect("control");
                let before = unsafe { GetWindow(control, GW_HWNDPREV) }.expect("a title");
                assert_eq!(spoken(&get_text(before)), want);
            }
        });
    }

    /// What the accessibility system reports for a control (MSAA, which UI
    /// Automation reads through its proxy), or `None` when it has no name.
    fn msaa_name(hwnd: HWND) -> Option<String> {
        use windows::core::Interface;
        use windows::Win32::System::Variant::VARIANT;
        use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
        const OBJID_CLIENT: u32 = 0xFFFF_FFFC;
        unsafe {
            let mut out: *mut c_void = std::ptr::null_mut();
            AccessibleObjectFromWindow(hwnd, OBJID_CLIENT, &IAccessible::IID, &mut out).ok()?;
            let acc = IAccessible::from_raw(out);
            let name = acc.get_accName(&VARIANT::from(0i32)).ok()?;
            Some(name.to_string())
        }
    }

    /// What UI Automation (which Narrator uses) reports as the control's name.
    fn uia_name(hwnd: HWND) -> Option<String> {
        use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
        use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_APARTMENTTHREADED,
            );
            let uia: IUIAutomation =
                CoCreateInstance(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()?;
            let element = uia.ElementFromHandle(hwnd).ok()?;
            Some(element.CurrentName().ok()?.to_string())
        }
    }

    /// A window's names are taken back as it goes, so a recycled handle does
    /// not carry "Show OpenAI key" into some other control.
    #[test]
    fn clearing_the_names_gives_the_button_its_own_text_back() {
        with_window(Config::default(), |hwnd, raw| {
            unsafe { show_page(&mut *raw, Page::Models) };
            let button = get_dlg_item(hwnd, ID_OPENAI_SHOW_KEY).expect("button");
            assert_eq!(msaa_name(button).as_deref(), Some("Show OpenAI key"));
            clear_accessible_names(hwnd);
            assert_eq!(msaa_name(button).as_deref(), Some("Show"));
        });
    }

    /// #877: Narrator reads UI Automation, not MSAA. The long names must come
    /// through that path too, or "Show" is all a screen reader says.
    #[test]
    fn ui_automation_reports_the_vendor_names() {
        with_window(Config::default(), |hwnd, raw| {
            unsafe {
                show_page(&mut *raw, Page::Models);
                reveal_providers(&mut *raw);
            }
            for (id, want) in [
                (ID_OPENAI_SHOW_KEY, "Show OpenAI key"),
                (ID_OPENAI_MODEL, "OpenAI model"),
            ] {
                let control = get_dlg_item(hwnd, id).expect("control");
                let name = uia_name(control)
                    .unwrap_or_else(|| panic!("UI Automation gave no name for {id}"));
                assert_eq!(name, want, "control id {id}");
            }
        });
    }

    /// The readback: what Narrator would be told, asked of the real controls.
    /// The window is hidden, so this is the system's answer for a control and
    /// not a speech test; run by hand with Narrator for that.
    #[test]
    fn the_accessibility_system_reports_the_vendor_names() {
        with_window(Config::default(), |hwnd, raw| {
            // Show the Models page with every provider so the controls exist.
            unsafe {
                show_page(&mut *raw, Page::Models);
                reveal_providers(&mut *raw);
            }
            let mut answered = 0;
            for (id, want) in [
                (ID_OPENAI_SHOW_KEY, "Show OpenAI key"),
                (ID_ANTHROPIC_SHOW_KEY, "Show Anthropic key"),
                (ID_GEMINI_SHOW_KEY, "Show Gemini key"),
                (ID_CHANGE_PRIMARY, "Change Copilot key"),
                (ID_CHANGE_SECONDARY, "Change shortcut for any keyboard"),
                (ID_CHANGE_PAUSE, "Change pause shortcut"),
                (ID_OPENAI_MODEL, "OpenAI model"),
                (ID_ANTHROPIC_MODEL, "Anthropic model"),
            ] {
                let control = get_dlg_item(hwnd, id).expect("control");
                let name = msaa_name(control)
                    .unwrap_or_else(|| panic!("the accessibility system gave no name for {id}"));
                answered += 1;
                assert_eq!(name, want, "control id {id}");
            }
            assert_eq!(answered, 8);
        });
    }

    /// Every title and description fits the room its row gives it, so nothing
    /// is cut off at the row's edge. Measured with the real fonts at 100, 150
    /// and 250 %, where the wrapped lines grow with the type but the rows grow
    /// with the same scale. The two-line descriptions are named, so a text that
    /// stops being measured cannot make the test pass.
    #[test]
    fn titles_and_descriptions_fit_their_rows() {
        use windows::Win32::Graphics::Gdi::{
            DrawTextW, GetDC, ReleaseDC, SelectObject, DT_CALCRECT, DT_LEFT, DT_WORDBREAK,
        };
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let dc = unsafe { GetDC(Some(hwnd)) };
            for dpi in [96u32, 144, 240] {
                let font = build_font(dpi);
                let hint_font = unsafe { build_hint_font(font, dpi) };
                assert!(!font.0.is_null() && !hint_font.0.is_null());
                let mut checked = 0;
                let mut long_ones = 0;
                for c in &inner.children {
                    if unsafe { class_of(c.hwnd) } != "Static" {
                        continue;
                    }
                    let id = unsafe { GetDlgCtrlID(c.hwnd) };
                    let text = get_text(c.hwnd);
                    if text.is_empty() || id == ID_OLLAMA_STATUS || id == ID_PULL_STATUS {
                        continue;
                    }
                    let used = if is_hint_id(id) { hint_font } else { font };
                    let old = unsafe { SelectObject(dc, HGDIOBJ(used.0)) };
                    let mut rect = windows::Win32::Foundation::RECT {
                        left: 0,
                        top: 0,
                        right: to_px(c.w, dpi),
                        bottom: 0,
                    };
                    let mut wide: Vec<u16> = text.encode_utf16().collect();
                    unsafe {
                        DrawTextW(
                            dc,
                            &mut wide,
                            &mut rect,
                            DT_CALCRECT | DT_LEFT | DT_WORDBREAK,
                        );
                        SelectObject(dc, old);
                    }
                    assert!(
                        rect.bottom <= to_px(c.h, dpi),
                        "{dpi} dpi: {text:?} needs {} px of {} in {} px width",
                        rect.bottom,
                        to_px(c.h, dpi),
                        to_px(c.w, dpi)
                    );
                    if text == MODE_DESC || text == REDACT_HINT {
                        long_ones += 1;
                    }
                    checked += 1;
                }
                unsafe {
                    let _ = DeleteObject(HGDIOBJ(hint_font.0));
                    let _ = DeleteObject(HGDIOBJ(font.0));
                }
                assert!(
                    checked > 20,
                    "{dpi} dpi: only {checked} texts were measured"
                );
                assert_eq!(long_ones, 2, "{dpi} dpi: Mode and Redact descriptions");
            }
            unsafe { ReleaseDC(Some(hwnd), dc) };
        });
    }
    /// A hairline sits above every row but a page's first, and none is left
    /// loose: each one is a 1 dp tall control made on a page, so it hides with
    /// its page and, on Models, with its block.
    #[test]
    fn rows_are_separated_by_hairlines_and_the_first_row_has_none() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            for page in [Page::General, Page::Models, Page::Privacy] {
                let lines: Vec<&ChildLayout> = inner
                    .children
                    .iter()
                    .filter(|c| c.page == Some(page))
                    .filter(|c| unsafe { GetDlgCtrlID(c.hwnd) } == ID_ROW_SEPARATOR)
                    .collect();
                assert!(lines.len() >= 3, "{page:?} has {} hairlines", lines.len());
                let top = inner
                    .children
                    .iter()
                    .filter(|c| c.page == Some(page))
                    .map(|c| c.y)
                    .min()
                    .unwrap();
                for line in &lines {
                    assert_eq!(line.anchor, Anchor::Hairline);
                    assert_eq!(line.h, 1);
                    assert!(line.y > top, "{page:?}: a hairline above the first row");
                }
            }
        });
    }

    /// The hairline answers its colour request with its own brush (the border
    /// token), through the body's real window procedure: a different brush
    /// from the face, and the same one on the next ask (cached, not made per
    /// paint).
    #[test]
    fn the_hairline_is_painted_with_a_cached_border_brush() {
        use windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC;
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            let line = inner
                .children
                .iter()
                .find(|c| unsafe { GetDlgCtrlID(c.hwnd) } == ID_ROW_SEPARATOR)
                .expect("a hairline")
                .hwnd;
            let dc = unsafe { windows::Win32::Graphics::Gdi::GetDC(Some(inner.body)) };
            let ask = || unsafe {
                SendMessageW(
                    inner.body,
                    WM_CTLCOLORSTATIC,
                    Some(WPARAM(dc.0 as usize)),
                    Some(LPARAM(line.0 as isize)),
                )
                .0
            };
            let (a, b) = (ask(), ask());
            unsafe { windows::Win32::Graphics::Gdi::ReleaseDC(Some(inner.body), dc) };
            assert_ne!(a, 0, "the hairline got a brush");
            assert_eq!(a, b, "the brush is cached, not made per ask");
            assert_eq!(a, unsafe { edit_style() }.hairline_brush.0 as isize);
        });
    }

    thread_local! {
        /// Live region events the test hook has seen, as the window handles.
        static LIVE_EVENTS: std::cell::RefCell<Vec<isize>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    unsafe extern "system" fn live_event_proc(
        _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
        _event: u32,
        hwnd: HWND,
        _id_object: i32,
        _id_child: i32,
        _thread: u32,
        _time: u32,
    ) {
        LIVE_EVENTS.with(|e| e.borrow_mut().push(hwnd.0 as isize));
    }

    /// The observable of the live region: a message on the status line raises
    /// `EVENT_OBJECT_LIVEREGIONCHANGED` on that control, which is what a
    /// screen reader listens for. A real out-of-context event hook, pumped on
    /// this thread, hears it; with the call missing it hears nothing.
    #[test]
    fn a_status_message_raises_a_live_region_event_on_the_status_line() {
        use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, EVENT_OBJECT_LIVEREGIONCHANGED, PM_REMOVE,
        };
        with_window(Config::default(), |hwnd, raw| {
            LIVE_EVENTS.with(|e| e.borrow_mut().clear());
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_OBJECT_LIVEREGIONCHANGED,
                    EVENT_OBJECT_LIVEREGIONCHANGED,
                    None,
                    Some(live_event_proc),
                    std::process::id(),
                    windows::Win32::System::Threading::GetCurrentThreadId(),
                    0, // WINEVENT_OUTOFCONTEXT
                )
            };
            assert!(!hook.0.is_null(), "the event hook was installed");
            set_status(unsafe { &mut *raw }, "Saved: Card corner", true);
            let mut m = MSG::default();
            let until = std::time::Instant::now() + Duration::from_secs(2);
            while LIVE_EVENTS.with(|e| e.borrow().is_empty()) && std::time::Instant::now() < until {
                while unsafe { PeekMessageW(&mut m, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe { DispatchMessageW(&m) };
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            unsafe {
                let _ = UnhookWinEvent(hook);
            }
            let line = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).expect("status line");
            let heard = LIVE_EVENTS.with(|e| e.borrow().clone());
            assert!(heard.contains(&(line.0 as isize)), "heard {heard:?}");
            assert_eq!(get_text(line), "Saved: Card corner");
        });
    }

    /// Narrator announces a live region change only for a control whose UIA
    /// `LiveSetting` is set. Read back through real UI Automation (from a
    /// second thread, as a screen reader is another process; this thread
    /// pumps the window), both status lines say Polite. Without the annotation
    /// the property reads 0 (Off) and this fails.
    #[test]
    fn the_status_lines_are_polite_uia_live_regions() {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        use windows::Win32::UI::Accessibility::UIA_LiveSettingPropertyId;
        use windows::Win32::UI::WindowsAndMessaging::{DispatchMessageW, PeekMessageW, PM_REMOVE};
        with_window(Config::default(), |hwnd, _raw| {
            let lines: Vec<isize> = LIVE_STATUS_IDS
                .iter()
                .map(|id| get_dlg_item(hwnd, *id).expect("status line").0 as isize)
                .collect();
            let reader = std::thread::spawn(move || unsafe {
                let init = CoInitializeEx(None, COINIT_MULTITHREADED);
                let automation = crate::inputs::uia_automation::create_automation();
                let out: Vec<i32> = match automation {
                    Ok(a) => lines
                        .iter()
                        .map(|h| {
                            a.ElementFromHandle(HWND(*h as *mut c_void))
                                .and_then(|e| e.GetCurrentPropertyValue(UIA_LiveSettingPropertyId))
                                .ok()
                                .and_then(|v| i32::try_from(&v).ok())
                                .unwrap_or(-1)
                        })
                        .collect(),
                    Err(_) => vec![-1; lines.len()],
                };
                if init.is_ok() {
                    CoUninitialize();
                }
                out
            });
            let mut m = MSG::default();
            while !reader.is_finished() {
                while unsafe { PeekMessageW(&mut m, None, 0, 0, PM_REMOVE) }.as_bool() {
                    unsafe { DispatchMessageW(&m) };
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            let got = reader.join().expect("reader thread");
            assert_eq!(
                got,
                vec![1, 1],
                "LiveSetting of the two status lines (1 = Polite)"
            );
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

    // -- Models page shows the active provider (#874) -----------------------

    /// Whether the control carries `WS_VISIBLE` (the window itself is not
    /// shown in these tests, so `IsWindowVisible` would say no to everything).
    fn shows(hwnd: HWND, id: i32) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::GWL_STYLE;
        let h = get_dlg_item(hwnd, id).unwrap_or_else(|| panic!("id {id} missing"));
        unsafe { GetWindowLongPtrW(h, GWL_STYLE) as u32 & WS_VISIBLE.0 != 0 }
    }

    const OPENAI_IDS: [i32; 4] = [
        ID_OPENAI_KEY,
        ID_OPENAI_SHOW_KEY,
        ID_OPENAI_MODEL,
        ID_OPENAI_EFFORT,
    ];
    const ANTHROPIC_IDS: [i32; 4] = [
        ID_ANTHROPIC_KEY,
        ID_ANTHROPIC_SHOW_KEY,
        ID_ANTHROPIC_MODEL,
        ID_ANTHROPIC_EFFORT,
    ];
    const GEMINI_IDS: [i32; 5] = [
        ID_GEMINI_KEY,
        ID_GEMINI_SHOW_KEY,
        ID_GEMINI_MODEL,
        ID_GEMINI_EFFORT,
        ID_GEMINI_EFFORT_LABEL,
    ];

    fn pick_provider(inner: &mut SettingsInner, index: i32) {
        let combo = get_dlg_item(inner.hwnd, ID_ACTIVE_PROVIDER).expect("provider combo");
        combo_select_index(combo, index);
        handle_command(inner, ID_ACTIVE_PROVIDER, CBN_SELCHANGE);
    }

    #[test]
    fn the_models_page_starts_with_only_the_active_provider() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            assert!(OPENAI_IDS.iter().all(|id| shows(hwnd, *id)));
            assert!(ANTHROPIC_IDS.iter().all(|id| !shows(hwnd, *id)));
            assert!(GEMINI_IDS.iter().all(|id| !shows(hwnd, *id)));
            assert!(!shows(hwnd, ID_OLLAMA_MODEL));
            assert!(shows(hwnd, ID_ACTIVE_PROVIDER));
            assert!(shows(hwnd, ID_ADD_PROVIDER));
            assert!(shows(hwnd, ID_OPEN_CONFIG));
        });
    }

    #[test]
    fn switching_provider_swaps_the_visible_group_and_keeps_typed_values() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            let openai_key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(openai_key, "sk-typed-by-hand");
            pick_provider(inner, ActiveProvider::Anthropic.index() as i32);
            assert!(ANTHROPIC_IDS.iter().all(|id| shows(hwnd, *id)));
            // OpenAI has typed text now, so it stays shown like a vendor with a key.
            assert!(OPENAI_IDS.iter().all(|id| shows(hwnd, *id)));
            assert!(GEMINI_IDS.iter().all(|id| !shows(hwnd, *id)));
            pick_provider(inner, ActiveProvider::Gemini.index() as i32);
            assert!(GEMINI_IDS.iter().all(|id| shows(hwnd, *id)));
            assert!(ANTHROPIC_IDS.iter().all(|id| !shows(hwnd, *id)));
            assert_eq!(get_text(openai_key), "sk-typed-by-hand");
            pick_provider(inner, ActiveProvider::Ollama.index() as i32);
            assert!(shows(hwnd, ID_OLLAMA_MODEL));
        });
    }

    #[test]
    fn a_stored_key_keeps_its_vendor_shown() {
        let mut cfg = Config::default();
        cfg.providers.openai.api_key = "sk-stored".to_string();
        cfg.providers.order = vec!["anthropic".to_string(), "openai".to_string()];
        with_window(cfg, |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            assert!(ANTHROPIC_IDS.iter().all(|id| shows(hwnd, *id)));
            assert!(OPENAI_IDS.iter().all(|id| shows(hwnd, *id)));
            assert!(GEMINI_IDS.iter().all(|id| !shows(hwnd, *id)));
        });
    }

    #[test]
    fn add_another_provider_shows_the_rest_and_goes_away() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            handle_command(inner, ID_ADD_PROVIDER, BN_CLICKED);
            for id in OPENAI_IDS
                .iter()
                .chain(ANTHROPIC_IDS.iter())
                .chain(GEMINI_IDS.iter())
            {
                assert!(shows(hwnd, *id), "id {id}");
            }
            assert!(shows(hwnd, ID_OLLAMA_MODEL));
            assert!(!shows(hwnd, ID_ADD_PROVIDER));
            // Switching provider afterwards does not hide what was asked for.
            pick_provider(inner, ActiveProvider::Anthropic.index() as i32);
            assert!(shows(hwnd, ID_OPENAI_KEY));
        });
    }

    /// The button that had focus is gone after "Add another provider", so
    /// focus goes to the first row that appeared, never to the hidden button.
    #[test]
    fn focus_after_add_another_provider_goes_to_the_first_new_row() {
        let one = [true, false, false, false];
        assert_eq!(reveal_focus_id(one, [true; 4]), ID_ANTHROPIC_KEY);
        assert_eq!(
            reveal_focus_id([false, true, false, false], [true; 4]),
            ID_OPENAI_KEY
        );
        assert_eq!(
            reveal_focus_id([true, true, true, false], [true; 4]),
            ID_OLLAMA_MODEL
        );
        // Nothing new appeared: the always-present provider combo.
        assert_eq!(reveal_focus_id([true; 4], [true; 4]), ID_ACTIVE_PROVIDER);
        // And the real window really uses it: the pick is a visible control.
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            handle_command(inner, ID_ADD_PROVIDER, BN_CLICKED);
            assert!(shows(hwnd, reveal_focus_id(one, inner.models_shown)));
        });
    }

    #[test]
    fn the_ollama_card_hides_with_the_ollama_block() {
        let card = [
            ID_OLLAMA_MODEL,
            ID_OLLAMA_STATUS,
            ID_PULL_NAME,
            ID_PULL_BUTTON,
        ];
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            assert!(
                card.iter().all(|id| !shows(hwnd, *id)),
                "default order has no ollama"
            );
            let short = inner.models_shrink_dp;
            pick_provider(inner, ActiveProvider::Ollama.index() as i32);
            assert!(card.iter().all(|id| shows(hwnd, *id)));
            assert!(
                inner.models_shrink_dp < short,
                "the card brings its height back"
            );
        });
        let mut cfg = Config::default();
        cfg.providers.order = vec!["openai".to_string(), "ollama".to_string()];
        with_window(cfg, |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            assert!(
                card.iter().all(|id| shows(hwnd, *id)),
                "ollama is in the order"
            );
        });
    }

    /// A hidden vendor's hand-edited model, and a stored key on a vendor that
    /// stays shown, both come back unchanged from the Save path (`read_form`).
    #[test]
    fn hidden_vendors_survive_the_save_path_unchanged() {
        let mut cfg = Config::default();
        cfg.providers.order = vec!["anthropic".to_string()];
        cfg.providers.openai.model = "gpt-hand-edited".to_string();
        cfg.providers.openai.models = vec!["gpt-hand-edited".to_string(), "gpt-other".to_string()];
        cfg.providers.gemini.api_key = "AIza-stored-gemini-key".to_string();
        cfg.providers.gemini.model = "gemini-hand-edited".to_string();
        with_window(cfg.clone(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            assert!(
                !shows(hwnd, ID_OPENAI_MODEL),
                "OpenAI has no key and is not active"
            );
            assert!(
                shows(hwnd, ID_GEMINI_KEY),
                "a stored key keeps its vendor shown"
            );
            let saved = read_form(inner);
            assert_eq!(saved.providers.openai.model, "gpt-hand-edited");
            assert_eq!(saved.providers.openai.models, cfg.providers.openai.models);
            assert_eq!(saved.providers.gemini.api_key, cfg.providers.gemini.api_key);
            assert_eq!(saved.providers.gemini.model, "gemini-hand-edited");
            assert_eq!(saved.providers.order, cfg.providers.order);
        });
    }
    #[test]
    fn a_hidden_block_leaves_no_gap_and_the_page_gets_shorter() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let design_y = |inner: &SettingsInner, id: i32| {
                let h = get_dlg_item(hwnd, id).unwrap();
                inner.children.iter().find(|c| c.hwnd == h).unwrap().y
            };
            unsafe { show_page(inner, Page::Models) };
            let first_block_y = design_y(inner, ID_OPENAI_KEY);
            let hint_y_one_vendor = design_y(inner, ID_OPEN_CONFIG);
            let shrink_openai = inner.models_shrink_dp;
            assert!(
                shrink_openai > 0,
                "two vendors and the Ollama row are hidden"
            );
            pick_provider(inner, ActiveProvider::Anthropic.index() as i32);
            // Claude takes the place OpenAI had, right under the combo.
            assert_eq!(design_y(inner, ID_ANTHROPIC_KEY), first_block_y);
            // Same number of rows hidden, so the bottom did not move.
            assert_eq!(design_y(inner, ID_OPEN_CONFIG), hint_y_one_vendor);
            handle_command(inner, ID_ADD_PROVIDER, BN_CLICKED);
            assert!(inner.models_shrink_dp < shrink_openai);
            assert!(design_y(inner, ID_OPEN_CONFIG) > hint_y_one_vendor);
        });
    }
    /// With the Ollama card hidden, the Models page ends where every other page
    /// does: a row's padding and the body's under its lowest visible control.
    #[test]
    fn a_hidden_ollama_card_leaves_the_page_ending_where_other_pages_end() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { show_page(inner, Page::Models) };
            let lowest = inner
                .children
                .iter()
                .filter(|c| c.page == Some(Page::Models))
                .filter(|c| unsafe { IsWindowVisible(c.hwnd) }.as_bool())
                // A combo's design height is its drop-down list: the box is a row.
                .map(|c| {
                    c.y + if unsafe { class_of(c.hwnd) } == "ComboBox" {
                        ROW_H
                    } else {
                        c.h
                    }
                })
                .max()
                .expect("a visible control");
            assert!(!shows(hwnd, ID_PULL_STATUS), "the card is hidden");
            let content = inner.page_content_dp[Page::Models.index()] - inner.models_shrink_dp;
            // The last row's own padding, then the body's: as every other page.
            assert_eq!(content, lowest + ROW_PAD + BODY_PAD_BOTTOM);
        });
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
            // The card is under the Ollama block, hidden on a default config
            // (#874); the probe still reports into it, and this checks the
            // button once it is on show.
            unsafe { reveal_providers(inner) };
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
            // The `&` marks a mnemonic (#877); the words are what is checked.
            let words: Vec<String> = texts.iter().map(|t| t.replace('&', "")).collect();
            assert!(words.iter().any(|t| t.starts_with("Hide the card after")));
            assert!(words
                .iter()
                .any(|t| t.starts_with("Shortcut for any keyboard")));
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

    /// #1152 wired-to-nothing: the three buttons are real controls on the
    /// Actions page, named for a screen reader, and Enter presses each.
    #[test]
    fn the_actions_page_has_its_three_buttons() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            for (id, text) in [
                (ID_OPEN_ACTIONS, "Open actions file"),
                (ID_IMPORT_ACTION, "Import action"),
                (ID_EXPORT_ACTION, "Export action"),
            ] {
                let button = get_dlg_item(hwnd, id).expect("button exists");
                assert_eq!(get_text(button), text);
                assert_eq!(enter_command(id), id);
                assert!(
                    ACCESSIBLE_NAMES.iter().any(|(i, _)| *i == id),
                    "{text} has no accessible name"
                );
                assert!(!is_shown(hwnd, id), "{text} shows on another page");
            }
            unsafe {
                SendMessageW(inner.rail, LB_SETCURSEL, Some(WPARAM(3)), None);
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_RAIL as usize | ((LBN_SELCHANGE as usize) << 16))),
                    Some(LPARAM(inner.rail.0 as isize)),
                );
            }
            for id in [ID_OPEN_ACTIONS, ID_IMPORT_ACTION, ID_EXPORT_ACTION] {
                assert!(is_shown(hwnd, id));
            }
        });
        assert!(!ACTIONS_NOTE.contains("tray"), "{ACTIONS_NOTE}");
    }

    /// #1152: the export list becomes a menu, or one plain line when there is
    /// nothing to pick. Never a silent no-op (rule 7).
    #[test]
    fn the_export_list_becomes_a_menu_or_a_plain_line() {
        let two = vec![
            ("a".to_string(), "A & B".to_string()),
            ("b".to_string(), "C".to_string()),
        ];
        assert_eq!(export_outcome(Ok(two.clone())), ExportOutcome::Menu(two));
        assert_eq!(
            export_outcome(Ok(Vec::new())),
            ExportOutcome::Say(NO_ACTIONS_TO_EXPORT.to_string())
        );
        match export_outcome(Err("boom".to_string())) {
            ExportOutcome::Say(line) => {
                assert!(line.contains("boom"), "{line}");
                assert!(!line.contains('\u{2014}'), "{line}");
            }
            other => panic!("{other:?}"),
        }
        for line in [NO_ACTIONS_TO_EXPORT, NO_ACTIONS_FILE, OPEN_ACTIONS_FAILED] {
            assert!(!line.contains('\u{2014}'), "{line}");
        }
    }

    /// #1152: a menu label with an ampersand shows it, not an underline.
    #[test]
    fn an_export_menu_label_shows_its_ampersands() {
        assert_eq!(menu_label("A & B"), "A && B");
        assert_eq!(menu_label("Plain"), "Plain");
    }

    /// #1152 wired-to-nothing: each button's click reaches the app's own
    /// handler or a worker, the window answers the worker and the menu by
    /// message, and the click itself does no file IO.
    #[test]
    fn the_actions_buttons_are_wired_to_the_apps_handlers() {
        let src = include_str!("settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        for arm in [
            "ID_IMPORT_ACTION if notify == BN_CLICKED => {",
            "ID_OPEN_ACTIONS if notify == BN_CLICKED => start_actions_job(inner, ActionsJob::Open),",
            "WM_SETTINGS_ACTIONS =>",
            "msg == WM_EXPORT_MENU",
            "crate::app::SettingsCommand::ImportAction",
            "crate::app::post_export_pick(",
        ] {
            assert!(code.contains(arm), "no `{arm}`");
        }
        let click = code
            .split("ID_EXPORT_ACTION if notify == BN_CLICKED")
            .nth(1)
            .expect("no export arm");
        let click = click.split("ID_PULL_BUTTON").next().unwrap();
        assert!(click.contains("ActionsJob::Export"), "{click}");
        assert!(!click.contains("load_actions"), "IO in the click: {click}");
    }

    /// #1152: a worker whose answer could not be posted frees the page for the
    /// next click instead of leaving it busy for good. (When the window is
    /// gone nothing reads the flag.)
    #[test]
    fn a_failed_post_clears_the_actions_busy_flag() {
        let src = include_str!("settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let job = code
            .split("fn start_actions_job(")
            .nth(1)
            .and_then(|s| s.split("\nfn take_actions(").next())
            .expect("start_actions_job moved");
        assert!(job.contains("if !posted {"), "{job}");
        assert!(job.contains("busy.store(false"), "{job}");
        assert!(job.contains(".is_ok()"), "{job}");
    }

    /// #1152: with no owner (as here) a click asks nobody and does not panic.
    #[test]
    fn clicking_import_in_a_window_with_no_owner_does_nothing() {
        with_window(Config::default(), |hwnd, raw| {
            handle_command(unsafe { &mut *raw }, ID_IMPORT_ACTION, BN_CLICKED);
            assert!(get_dlg_item(hwnd, ID_IMPORT_ACTION).is_some());
        });
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
            let light =
                crate::ui::theme::Tokens::for_theme(Theme::Light, &crate::ui::theme::HC_BLACK);
            let ink = hint_text_color(Theme::Light, &light, rgb(0, 0, 0));
            let ratio = contrast_ratio(relative_luminance(ink), relative_luminance(face));
            assert!(ratio >= 4.5, "hint contrast {ratio:.2}:1 is under 4.5:1");
        }
        // High contrast uses the system's own text colour, never a grey.
        let hc = rgb(255, 255, 255);
        let hc_tokens =
            crate::ui::theme::Tokens::for_theme(Theme::HighContrast, &crate::ui::theme::HC_BLACK);
        assert_eq!(hint_text_color(Theme::HighContrast, &hc_tokens, hc), hc);
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

    /// #877: the group boxes are gone: rows and hairlines instead. Two checks,
    /// because a source scan alone passes while a box is still made by number
    /// and a window scan alone passes while the constant is still declared:
    /// no `BS_GROUPBOX` anywhere in this file outside tests, and no button in
    /// the built window has that style.
    #[test]
    fn settings_has_no_group_box() {
        let src = include_str!("settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        assert!(
            code.contains("fn build_ui_recorded"),
            "scanned the real code"
        );
        let needle = ["BS_", "GROUPBOX"].concat();
        assert!(!code.contains(&needle), "{needle} is still in settings.rs");
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &*raw };
            assert!(!inner.children.is_empty());
            let boxes = inner
                .children
                .iter()
                .filter(|c| unsafe { is_button_with_style(c.hwnd, 0x0007) })
                .count();
            assert_eq!(boxes, 0, "a group box is still built");
        });
    }

    /// Whether `hwnd` is a button whose style (the low four bits) is `style`.
    unsafe fn is_button_with_style(hwnd: HWND, style: i32) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetClassNameW, GetWindowLongPtrW, GWL_STYLE,
        };
        let mut name = [0u16; 16];
        let n = GetClassNameW(hwnd, &mut name) as usize;
        String::from_utf16_lossy(&name[..n]) == "Button"
            && (GetWindowLongPtrW(hwnd, GWL_STYLE) as i32 & 0xF) == style
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
        assert_eq!(footer_divider_color(Theme::Light, &HC_BLACK), LIGHT.border);
        assert_eq!(
            footer_divider_color(Theme::Dark, &HC_BLACK),
            crate::ui::theme::DARK.border
        );
        for sys in [HC_BLACK, HC_WHITE] {
            assert_eq!(
                footer_divider_color(Theme::HighContrast, &sys),
                sys.window_text
            );
        }
    }

    /// #582: a helper line is the dim text token in light and dark, and the
    /// system's button text in high contrast.
    #[test]
    fn hint_text_colour_follows_the_theme() {
        use crate::ui::theme::{Tokens, DARK, HC_BLACK, LIGHT};
        let btn = 0x0012_3456;
        let t = |theme| Tokens::for_theme(theme, &HC_BLACK);
        assert_eq!(
            hint_text_color(Theme::Light, &t(Theme::Light), btn),
            LIGHT.text_dim
        );
        assert_eq!(
            hint_text_color(Theme::Dark, &t(Theme::Dark), btn),
            DARK.text_dim
        );
        assert_eq!(
            hint_text_color(Theme::HighContrast, &t(Theme::HighContrast), btn),
            btn
        );
    }

    /// #934: a text box takes its fill, border and text from the theme tokens
    /// (the system colours in high contrast), and the focus line is the accent
    /// in a normal theme and the system highlight in high contrast.
    #[test]
    fn edit_colours_come_from_the_tokens() {
        use crate::ui::theme::{rgb, HC_BLACK, HC_WHITE, LIGHT};
        let accent = rgb(0, 103, 192);
        let face = rgb(240, 240, 240);
        let c = edit_colors(Theme::Light, &HC_BLACK, accent, face);
        assert_eq!(c.fill, LIGHT.control_fill);
        assert_eq!(c.border, LIGHT.border);
        assert_eq!(c.text, LIGHT.text);
        assert_eq!(c.focus_line, accent);
        assert_eq!(c.face, face);
        // #582: dark is the dark tokens, with the control edge (not the
        // decorative hairline) as the border.
        let d = edit_colors(
            Theme::Dark,
            &HC_BLACK,
            accent,
            crate::ui::theme::DARK.surface,
        );
        assert_eq!(d.fill, crate::ui::theme::DARK.control_fill);
        assert_eq!(d.border, crate::ui::theme::DARK.control_border);
        assert_eq!(d.text, crate::ui::theme::DARK.text);
        assert_eq!(d.face, crate::ui::theme::DARK.surface);
        for sys in [HC_BLACK, HC_WHITE] {
            let c = edit_colors(Theme::HighContrast, &sys, accent, face);
            assert_eq!(c.fill, sys.window);
            assert_eq!(c.border, sys.window_text);
            assert_eq!(c.text, sys.window_text);
            assert_eq!(c.focus_line, sys.highlight);
        }
    }

    /// #1109: the hovered look is a firmer border in a normal theme and the system
    /// highlight border in high contrast.
    #[test]
    fn hover_colours_come_from_the_tokens() {
        use crate::ui::theme::{rgb, HC_BLACK, HC_WHITE};
        let accent = rgb(0, 103, 192);
        let face = rgb(240, 240, 240);
        let c = edit_colors(Theme::Light, &HC_BLACK, accent, face);
        assert_ne!(
            c.hover_border, c.border,
            "a hovered box has a firmer border"
        );
        for sys in [HC_BLACK, HC_WHITE] {
            let c = edit_colors(Theme::HighContrast, &sys, accent, face);
            assert_eq!(c.hover_border, sys.highlight);
        }
        let hovered = frame_colors(&c, true);
        assert_eq!(hovered.border, c.hover_border);
        assert_eq!(frame_colors(&c, false).border, c.border);
    }

    /// Forces a theme for the test's thread and puts it back on drop, even if
    /// the test panics.
    struct Forced;
    impl Forced {
        fn new(theme: Theme) -> Forced {
            force_theme(Some(theme));
            Forced
        }
    }
    impl Drop for Forced {
        fn drop(&mut self) {
            force_theme(None);
        }
    }

    /// The colour of the pixel the window paints at (5, 5) when asked to erase
    /// its background into a memory bitmap.
    fn erased_face_pixel(hwnd: HWND) -> u32 {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetPixel, SelectObject,
        };
        use windows::Win32::UI::WindowsAndMessaging::WM_ERASEBKGND;
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 64, 64);
            let old = SelectObject(dc, bitmap.into());
            let answer = SendMessageW(hwnd, WM_ERASEBKGND, Some(WPARAM(dc.0 as usize)), None);
            assert_eq!(answer.0, 1, "the erase is answered");
            let pixel = GetPixel(dc, 5, 5).0;
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            pixel
        }
    }

    /// What the window answers a label's colour request with: the brush's
    /// colour and the text colour left on the DC.
    fn label_answer(hwnd: HWND) -> (u32, u32) {
        let status = get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).expect("status line");
        label_answer_for(hwnd, status)
    }

    /// The same, asked of `window` about `label` (the body answers for its own
    /// labels).
    fn label_answer_for(window: HWND, label: HWND) -> (u32, u32) {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, DeleteDC, GetObjectW, GetTextColor, LOGBRUSH,
        };
        use windows::Win32::UI::WindowsAndMessaging::WM_CTLCOLORSTATIC;
        unsafe {
            let dc = CreateCompatibleDC(None);
            let brush = SendMessageW(
                window,
                WM_CTLCOLORSTATIC,
                Some(WPARAM(dc.0 as usize)),
                Some(LPARAM(label.0 as isize)),
            );
            let mut logbrush = LOGBRUSH::default();
            GetObjectW(
                HGDIOBJ(brush.0 as *mut c_void),
                std::mem::size_of::<LOGBRUSH>() as i32,
                Some(&mut logbrush as *mut LOGBRUSH as *mut c_void),
            );
            let text = GetTextColor(dc).0;
            let _ = DeleteDC(dc);
            (logbrush.lbColor.0, text)
        }
    }

    fn get_dlg_item_body(hwnd: HWND) -> HWND {
        unsafe { GetDlgItem(Some(hwnd), ID_BODY) }.expect("body window")
    }

    /// A plain label under the body, with no id the body singles out.
    fn body_test_label(body: HWND) -> HWND {
        let class = wide_z("STATIC");
        unsafe {
            CreateWindowExW(
                Default::default(),
                PCWSTR(class.as_ptr()),
                PCWSTR::null(),
                WS_CHILD,
                0,
                0,
                10,
                10,
                Some(body),
                None,
                None,
                None,
            )
        }
        .expect("a body label")
    }

    /// Whether the title bar asks for the dark frame.
    fn title_bar_is_dark(hwnd: HWND) -> bool {
        use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
        let mut value = 0i32;
        unsafe {
            DwmGetWindowAttribute(
                hwnd,
                DWMWA_USE_IMMERSIVE_DARK_MODE,
                &mut value as *mut i32 as *mut c_void,
                std::mem::size_of::<i32>() as u32,
            )
            .expect("read the title bar attribute");
        }
        value != 0
    }

    /// #582, wired-to-nothing check: with nothing wired, a dark window would
    /// still erase to the system face and answer labels with the default
    /// colours. It erases to the surface token, answers labels with the text
    /// token on it, and asks DWM for the dark title bar. Light keeps the
    /// system's answer, and a theme change while open switches between them.
    #[test]
    fn a_dark_window_paints_its_face_and_labels_from_the_tokens() {
        use crate::ui::theme::{DARK, HC_BLACK};
        use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_3DFACE};
        use windows::Win32::UI::WindowsAndMessaging::WM_THEMECHANGED;
        let system_face = unsafe { GetSysColor(COLOR_3DFACE) };
        {
            let _forced = Forced::new(Theme::Dark);
            with_window(Config::default(), |hwnd, _raw| {
                assert_eq!(erased_face_pixel(hwnd), DARK.surface);
                assert_eq!(label_answer(hwnd), (DARK.surface, DARK.text));
                assert!(title_bar_is_dark(hwnd));
                // The body answers for its own face and labels too.
                let body = get_dlg_item_body(hwnd);
                let label = body_test_label(body);
                assert_eq!(erased_face_pixel(body), DARK.surface);
                assert_eq!(label_answer_for(body, label), (DARK.surface, DARK.text));
                // Live re-theme: back to light, the same window.
                force_theme(Some(Theme::Light));
                unsafe { SendMessageW(hwnd, WM_THEMECHANGED, None, None) };
                assert_eq!(erased_face_pixel(hwnd), system_face);
                assert!(!title_bar_is_dark(hwnd));
                assert_ne!(label_answer_for(body, label).0, DARK.surface);
            });
        }
        {
            let _forced = Forced::new(Theme::Light);
            with_window(Config::default(), |hwnd, _raw| {
                assert_eq!(erased_face_pixel(hwnd), system_face);
                assert_eq!(label_answer(hwnd).0, system_face);
                assert!(!title_bar_is_dark(hwnd));
            });
        }
        {
            // The gallery's high contrast is Windows' black theme.
            let _forced = Forced::new(Theme::HighContrast);
            with_window(Config::default(), |hwnd, _raw| {
                assert_eq!(erased_face_pixel(hwnd), HC_BLACK.window);
                assert_eq!(label_answer(hwnd), (HC_BLACK.window, HC_BLACK.window_text));
            });
        }
    }

    /// The Start with Windows checkbox, on the page Settings opens on.
    fn autostart_checkbox(hwnd: HWND) -> HWND {
        get_dlg_item(hwnd, ID_AUTOSTART).expect("the Start with Windows checkbox")
    }

    /// The pixel at (`x`, `y`) of `hwnd`, as `PrintWindow` captures it.
    fn printed_pixel(hwnd: HWND, x: i32, y: i32) -> u32 {
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, GetPixel, SelectObject,
        };
        use windows::Win32::Storage::Xps::{PrintWindow, PRINT_WINDOW_FLAGS};
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bitmap = CreateCompatibleBitmap(screen, 400, 80);
            let old = SelectObject(dc, bitmap.into());
            // PW_RENDERFULLCONTENT
            assert!(PrintWindow(hwnd, dc, PRINT_WINDOW_FLAGS(2)).as_bool());
            let pixel = GetPixel(dc, x, y).0;
            SelectObject(dc, old);
            let _ = DeleteObject(bitmap.into());
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            pixel
        }
    }

    /// #582 slice 2, wired-to-nothing check: with nothing wired, a dark
    /// window's checkbox would be the stock one, a light box with a black
    /// label. Captured with `PrintWindow`: the unchecked box's left edge is
    /// the control border token, and a checked box's corner is the accent.
    /// Light keeps the stock box, whose edge is not the token.
    #[test]
    fn a_dark_checkbox_is_drawn_from_the_tokens_and_light_stays_stock() {
        use crate::ui::theme::DARK;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNA};
        const BM_SETCHECK: u32 = 0x00F1;
        let probe = |theme: Theme| {
            let _forced = Forced::new(theme);
            let mut seen = (0u32, 0u32, 0u32, 0u32);
            with_window(Config::default(), |hwnd, _raw| {
                let check = autostart_checkbox(hwnd);
                unsafe {
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                    let _ = ShowWindow(check, SW_SHOWNA);
                    SendMessageW(check, BM_SETCHECK, Some(WPARAM(0)), None);
                }
                let mut rc = windows::Win32::Foundation::RECT::default();
                unsafe {
                    let _ = GetClientRect(check, &mut rc);
                }
                let g = crate::ui::settings_buttons::check_geometry(
                    rc.right - rc.left,
                    rc.bottom - rc.top,
                    96,
                    0,
                );
                let mid = (g.box_rc.top + g.box_rc.bottom) / 2;
                let edge = printed_pixel(check, g.box_rc.left, mid);
                unsafe { SendMessageW(check, BM_SETCHECK, Some(WPARAM(1)), None) };
                let corner = printed_pixel(check, g.box_rc.left + 8, g.box_rc.top + 2);
                // Inside the 400 px capture, on the top row, clear of the label.
                let face = printed_pixel(check, rc.right.min(390) - 2, 1);
                seen = (
                    edge,
                    corner,
                    face,
                    unsafe { edit_style() }.colors.focus_line,
                );
            });
            seen
        };
        let (edge, corner, face, accent) = probe(Theme::Dark);
        // The edge is anti-aliased, so it is held to the 3:1 bar the token
        // exists for, not to the token's exact value. The stock box edge
        // (MEASURED 2026-10-17: 0xF0 on the face in dark, before this slice)
        // would fail it.
        let ratio = |a: u32, b: u32| {
            use crate::ui::theme::{contrast_ratio, relative_luminance};
            contrast_ratio(relative_luminance(a), relative_luminance(b))
        };
        assert!(
            ratio(edge, DARK.surface) >= 3.0,
            "the unchecked box edge is {edge:#08x}"
        );
        assert_eq!(face, DARK.surface, "the face behind the box");
        assert_eq!(corner, accent, "a checked box is the accent");
        let (light_edge, light_corner, light_face, _) = probe(Theme::Light);
        assert_ne!(
            light_edge, edge,
            "light is the stock box, not the dark paint"
        );
        // And it is what stock drawing gives: the system face behind the box
        // and a checked box that is not the dark accent paint.
        let system_face = unsafe {
            windows::Win32::Graphics::Gdi::GetSysColor(windows::Win32::Graphics::Gdi::COLOR_3DFACE)
        };
        assert_eq!(light_face, system_face, "the stock face behind the box");
        assert_ne!(light_face, DARK.surface);
        assert_ne!(light_corner, accent, "a stock checked box is not our fill");
    }

    /// #582 slice 2: a focused checkbox draws its 2 px frame in the text token
    /// only while the keyboard cue is showing, as the stock one does. With the
    /// cue hidden (the state after a mouse click) the same pixel is the face.
    #[test]
    fn a_dark_checkbox_shows_its_focus_frame_only_when_the_cue_is_visible() {
        use crate::ui::theme::DARK;
        use windows::Win32::UI::Input::KeyboardAndMouse::SetFocus;
        use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_SHOWNA};
        const WM_UPDATEUISTATE: u32 = 0x0128;
        let _forced = Forced::new(Theme::Dark);
        with_window(Config::default(), |hwnd, _raw| {
            let check = autostart_checkbox(hwnd);
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNA);
                let _ = ShowWindow(check, SW_SHOWNA);
                let _ = SetFocus(Some(check));
            }
            let mut rc = windows::Win32::Foundation::RECT::default();
            unsafe {
                let _ = GetClientRect(check, &mut rc);
            }
            let mid = (rc.bottom - rc.top) / 2;
            // UIS_SET (1) / UIS_CLEAR (2) in the low word, UISF_HIDEFOCUS (1)
            // in the high word.
            let ui = |action: usize| unsafe {
                SendMessageW(
                    check,
                    WM_UPDATEUISTATE,
                    Some(WPARAM(action | (1 << 16))),
                    None,
                )
            };
            ui(1);
            assert_eq!(printed_pixel(check, 0, mid), DARK.surface, "cue hidden");
            ui(2);
            let frame = printed_pixel(check, 0, mid);
            use crate::ui::theme::{contrast_ratio, relative_luminance};
            let ratio = contrast_ratio(relative_luminance(frame), relative_luminance(DARK.surface));
            assert!(ratio >= 3.0, "cue shown: the frame pixel is {frame:#08x}");
        });
    }

    /// #582 slice 2: the manifest took effect in the process that runs
    /// Settings, because custom draw of buttons only exists in common
    /// controls v6.
    #[test]
    fn a_settings_button_is_a_common_controls_v6_button() {
        with_window(Config::default(), |hwnd, _raw| {
            assert!(
                crate::ui::settings_theme::button_is_common_controls_v6(autostart_checkbox(hwnd)),
                "Settings runs classic controls: NM_CUSTOMDRAW never arrives for buttons"
            );
        });
    }

    /// #1109: the mouse over a combo hovers it, once: moving inside it again
    /// repaints nothing, and leaving clears it. No timer is involved.
    #[test]
    fn a_combo_is_hovered_while_the_mouse_is_over_it() {
        use windows::Win32::UI::WindowsAndMessaging::WM_MOUSEMOVE;
        const WM_MOUSELEAVE: u32 = 0x02A3;
        with_window(Config::default(), |hwnd, _raw| {
            let combo = get_dlg_item(hwnd, ID_CARD_CORNER).unwrap();
            let other = get_dlg_item(hwnd, ID_MONITOR).unwrap();
            let send = |to: HWND, msg: u32| unsafe {
                SendMessageW(to, msg, Some(WPARAM(0)), Some(LPARAM(0x0005_0005)));
            };
            let base = combo_hover_repaints();
            assert!(!combo_is_hovered(combo));
            send(combo, WM_MOUSEMOVE);
            assert!(combo_is_hovered(combo));
            assert!(!combo_is_hovered(other));
            assert_eq!(combo_hover_repaints(), base + 1);
            send(combo, WM_MOUSEMOVE);
            assert_eq!(combo_hover_repaints(), base + 1, "no change, no repaint");
            send(combo, WM_MOUSELEAVE);
            assert!(!combo_is_hovered(combo));
            assert_eq!(combo_hover_repaints(), base + 2);
            send(combo, WM_MOUSELEAVE);
            assert_eq!(combo_hover_repaints(), base + 2);
        });
    }

    /// #1143: a combo that is disabled, hidden or opened does not keep the
    /// hovered border, whether or not Windows still sends `WM_MOUSELEAVE`.
    #[test]
    fn a_combo_loses_the_hover_when_disabled_hidden_or_opened() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CBN_DROPDOWN, WM_CAPTURECHANGED, WM_ENABLE, WM_MOUSEMOVE, WM_SHOWWINDOW,
        };
        with_window(Config::default(), |hwnd, _raw| {
            let combo = get_dlg_item(hwnd, ID_CARD_CORNER).unwrap();
            let hover = || unsafe {
                SendMessageW(
                    combo,
                    WM_MOUSEMOVE,
                    Some(WPARAM(0)),
                    Some(LPARAM(0x0005_0005)),
                );
            };
            let send = |msg: u32, w: usize| unsafe {
                SendMessageW(combo, msg, Some(WPARAM(w)), Some(LPARAM(0)));
            };
            // Staying enabled or shown, or a capture change, are told apart.
            hover();
            assert!(combo_is_hovered(combo));
            send(WM_ENABLE, 1);
            send(WM_SHOWWINDOW, 1);
            assert!(combo_is_hovered(combo), "enabled and shown keep the hover");
            send(WM_ENABLE, 0);
            assert!(!combo_is_hovered(combo), "disabled");
            hover();
            send(WM_SHOWWINDOW, 0);
            assert!(!combo_is_hovered(combo), "hidden");
            hover();
            send(WM_CAPTURECHANGED, 0);
            assert!(!combo_is_hovered(combo), "capture moved away");
            hover();
            assert!(combo_is_hovered(combo));
            let id = ID_CARD_CORNER as usize;
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(((CBN_DROPDOWN as usize) << 16) | id)),
                    Some(LPARAM(combo.0 as isize)),
                );
            }
            assert!(!combo_is_hovered(combo), "its list opened");
        });
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
            hover_border: 0x0000_0080,
            item_hover: 0x00EE_EEEE,
            item_text_hot: 0,
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
            hover_border: 0x0000_0080,
            item_hover: 0x00EE_EEEE,
            item_text_hot: 0,
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
        let c = edit_colors(
            crate::ui::theme::Theme::HighContrast,
            &HC_BLACK,
            rgb(0, 103, 192),
            face,
        );
        assert_eq!(c.disabled_fill, HC_BLACK.window);
        assert_eq!(c.disabled_border, HC_BLACK.gray_text);
        assert_eq!(c.disabled_text, HC_BLACK.gray_text);
        let c = edit_colors(
            crate::ui::theme::Theme::Light,
            &HC_BLACK,
            rgb(0, 103, 192),
            face,
        );
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

    /// #1015: a helper line is under the body height at every scale, never
    /// below a legible floor, and never taller than the body.
    #[test]
    fn the_hint_font_is_smaller_than_the_body_font_but_legible() {
        for dpi in [96u32, 120, 144, 192, 240] {
            let body = body_font_height(dpi);
            let hint = hint_font_height(dpi);
            assert!(hint.abs() < body.abs(), "dpi {dpi}: {hint} vs {body}");
            // Windows 11's floor is 12 px at 100 %, scaled.
            assert!(
                hint.abs() as f32 >= (12.0 * dpi as f32 / 96.0).round() - 0.5,
                "dpi {dpi}: {hint} is below the 12 px floor"
            );
        }
        // The ramp at 100 %: 14 px labels, 12 px helper lines.
        assert_eq!(body_font_height(96), -14);
        assert_eq!(hint_font_height(96), -12);
    }

    /// #1015: in the real window every helper line is set in a smaller font
    /// than a label, and freed fonts do not leak into a re-layout.
    #[test]
    fn helper_lines_are_set_smaller_than_the_labels() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            unsafe { apply_dpi(inner) };
            let label = font_height_of(get_dlg_item(hwnd, ID_REDACT).expect("label"));
            for id in [ID_MODEL_LIST_HINT, ID_CAPTURE_DETAIL_HINT, ID_REDACT_HINT] {
                let hint = font_height_of(get_dlg_item(hwnd, id).expect("hint"));
                assert!(hint < label, "hint {id}: {hint} px vs label {label} px");
            }
            // Laid out again: still smaller, and the old hint font is gone.
            unsafe { apply_dpi(inner) };
            let hint = font_height_of(get_dlg_item(hwnd, ID_REDACT_HINT).expect("hint"));
            assert!(hint < label);
        });
    }

    /// #1014: a page that fits needs no bar; a taller one does. An unlaid-out
    /// box (no page) keeps the bar.
    #[test]
    fn a_scroll_bar_is_needed_only_when_the_text_is_taller_than_the_box() {
        assert!(!scrollbar_needed(10, 600, 20), "30 lines of room, 10 used");
        assert!(!scrollbar_needed(30, 600, 20), "exactly full");
        assert!(scrollbar_needed(31, 600, 20), "one line too many");
        assert!(scrollbar_needed(1, 0, 20), "no height yet: keep the bar");
        assert!(scrollbar_needed(1, 600, 0), "no line height: keep the bar");
    }

    /// #1014: the prompt box has a bar while its text is longer than the box
    /// and none when it fits, so no empty strip is left down its side.
    #[test]
    fn the_prompt_box_shows_a_bar_only_when_its_text_overflows() {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowTextW, GWL_STYLE, SW_SHOW, WS_VSCROLL,
        };
        with_window(Config::default(), |hwnd, _raw| {
            let prompt = get_dlg_item(hwnd, ID_PROMPT_EDIT).expect("prompt edit");
            let has_bar =
                || unsafe { GetWindowLongPtrW(prompt, GWL_STYLE) } as u32 & WS_VSCROLL.0 != 0;
            // The box is created with WS_VSCROLL, so "no bar" is the first
            // assertion that can only pass if the sync ran.
            unsafe {
                let _ = ShowWindow(prompt, SW_SHOW);
                // A real box is at least three rows tall; the unlaid-out test
                // window leaves it one, which the 14 px labels no longer fit.
                let _ = SetWindowPos(prompt, None, 0, 0, 300, 120, SWP_NOMOVE | SWP_NOZORDER);
                let _ = SetWindowTextW(prompt, windows::core::w!("short"));
            }
            assert!(!has_bar(), "one line needs none");
            unsafe {
                let long: Vec<u16> = "a line\r\n".repeat(400).encode_utf16().chain([0]).collect();
                let _ = SetWindowTextW(prompt, windows::core::PCWSTR(long.as_ptr()));
            }
            assert!(has_bar(), "400 lines need a bar");
            // Select all + Delete: a key, not a text message, empties the box.
            unsafe {
                SendMessageW(
                    prompt,
                    0x00B1, /* EM_SETSEL */
                    Some(WPARAM(0)),
                    Some(LPARAM(-1)),
                );
                SendMessageW(
                    prompt,
                    windows::Win32::UI::WindowsAndMessaging::WM_KEYDOWN,
                    Some(WPARAM(0x2E /* VK_DELETE */)),
                    Some(LPARAM(0)),
                );
            }
            assert!(
                !has_bar(),
                "the strip goes away after select all and Delete"
            );
            // A font change re-wraps the text.
            unsafe {
                let long: Vec<u16> = "a line\r\n".repeat(400).encode_utf16().chain([0]).collect();
                let _ = SetWindowTextW(prompt, windows::core::PCWSTR(long.as_ptr()));
                let _ = windows::Win32::UI::Controls::ShowScrollBar(prompt, SB_VERT, false);
                SendMessageW(
                    prompt,
                    windows::Win32::UI::WindowsAndMessaging::WM_SETFONT,
                    Some(WPARAM(0)),
                    Some(LPARAM(1)),
                );
            }
            assert!(has_bar(), "a re-wrap that overflows brings the bar back");
        });
    }

    /// #1055: painting a rail item uses the cached style: it does not rebuild
    /// it (a registry read and a font each). A theme, accent or dpi change
    /// drops the cache and the next paint builds it once. Closing frees the
    /// icon font.
    #[test]
    fn painting_a_rail_item_does_not_read_the_registry_or_build_a_font() {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
        use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, ODT_LISTBOX};
        use windows::Win32::UI::WindowsAndMessaging::{WM_SETTINGCHANGE, WM_THEMECHANGED};
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &*raw };
            let rail = inner.rail;
            let paint = |selected: bool| unsafe {
                let hdc = GetDC(Some(rail));
                let di = DRAWITEMSTRUCT {
                    CtlType: ODT_LISTBOX,
                    CtlID: ID_RAIL as u32,
                    itemID: 1,
                    itemState: windows::Win32::UI::Controls::ODS_FLAGS(if selected {
                        ODS_SELECTED
                    } else {
                        0
                    }),
                    hwndItem: rail,
                    hDC: hdc,
                    rcItem: RECT {
                        left: 0,
                        top: 0,
                        right: 200,
                        bottom: 40,
                    },
                    itemData: 0,
                    itemAction: Default::default(),
                };
                SendMessageW(
                    hwnd,
                    WM_DRAWITEM,
                    Some(WPARAM(ID_RAIL as usize)),
                    Some(LPARAM(&di as *const _ as isize)),
                );
                ReleaseDC(Some(rail), hdc);
            };
            paint(true);
            let builds = RAIL_STYLE_BUILDS.get();
            for _ in 0..6 {
                paint(true);
                paint(false);
            }
            assert_eq!(
                RAIL_STYLE_BUILDS.get(),
                builds,
                "painting rebuilt the style"
            );
            unsafe { SendMessageW(hwnd, WM_THEMECHANGED, Some(WPARAM(0)), Some(LPARAM(0))) };
            paint(true);
            assert_eq!(
                RAIL_STYLE_BUILDS.get(),
                builds + 1,
                "a change rebuilds once"
            );
            unsafe { SendMessageW(hwnd, WM_SETTINGCHANGE, Some(WPARAM(0)), Some(LPARAM(0))) };
            paint(false);
            paint(true);
            assert_eq!(RAIL_STYLE_BUILDS.get(), builds + 2);
            unsafe { invalidate_rail_style() };
            assert!(RAIL_STYLE.get().is_none(), "the icon font is released");
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

            // A change in another box does not stop the pull.
            commit_field(unsafe { &mut *raw }, Field::CardSeconds);
            assert!(!cancel.load(Ordering::SeqCst));
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

    // -- clamp_text_scale --------------------------------------------------

    /// #828 (d): the "Checking Ollama" cue cycles one, two, three dots and
    /// the third frame is the static text the window is built with.
    #[test]
    fn checking_frames_cycle_dots_and_end_on_the_static_text() {
        assert_eq!(checking_text(0), "Checking Ollama.");
        assert_eq!(checking_text(1), "Checking Ollama..");
        assert_eq!(checking_text(2), OLLAMA_CHECKING_TEXT);
        assert_eq!(checking_text(3), checking_text(0));
        assert_eq!(checking_text(usize::MAX), checking_text(usize::MAX % 3));
        assert!(is_checking_text(&checking_text(1)));
        assert!(!is_checking_text("Fixing: stopping Ollama"));
        assert!(!is_checking_text("Ollama is installed but not running"));
    }

    /// #828 (d) wired-to-nothing check: a timer message moves the real
    /// window's Ollama status to the next frame, and the probe's result
    /// stops the animation for good (nothing ticks once the check is done).
    #[test]
    fn the_checking_cue_animates_then_stops_on_the_result() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        ensure_test_class(instance);
        ensure_common_controls();
        let raw = Box::into_raw(Box::new(SettingsInner::new(Config::default())));
        let class_name = wide_z(TEST_CLASS_NAME);
        let title = wide_z("Wingman settings (checking cue test)");
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
        {
            let inner = unsafe { &mut *raw };
            inner.hwnd = hwnd;
            inner.font = font;
            inner.adopt(built);
            start_checking_animation(inner, true);
            assert!(inner.checking_frame.is_some(), "the cue is running");
        }
        let status = get_dlg_item(hwnd, ID_OLLAMA_STATUS).expect("status label");
        unsafe {
            SendMessageW(
                hwnd,
                WM_TIMER,
                Some(WPARAM(CHECK_TIMER_ID)),
                Some(LPARAM(0)),
            );
        }
        assert_eq!(get_text(status), checking_text(0));
        unsafe {
            SendMessageW(
                hwnd,
                WM_TIMER,
                Some(WPARAM(CHECK_TIMER_ID)),
                Some(LPARAM(0)),
            );
        }
        assert_eq!(get_text(status), checking_text(1));

        // The result arrives: the text is the result, and a late tick
        // changes nothing.
        let inner = unsafe { &mut *raw };
        if let Ok(mut g) = inner.ollama_status.lock() {
            *g = Some(OllamaSection {
                text: "Ollama is installed but not running.".to_string(),
                link: None,
                fix: false,
            });
        }
        unsafe {
            SendMessageW(hwnd, WM_OLLAMA_STATUS, None, None);
            SendMessageW(
                hwnd,
                WM_TIMER,
                Some(WPARAM(CHECK_TIMER_ID)),
                Some(LPARAM(0)),
            );
        }
        assert!(inner.checking_frame.is_none(), "the cue stopped");
        assert_eq!(get_text(status), "Ollama is installed but not running.");
    }

    /// #828: the slider's value reads as a percent, like the rest of Windows.
    #[test]
    fn text_scale_label_is_a_percent() {
        assert_eq!(text_scale_label(1.0), "100%");
        assert_eq!(text_scale_label(1.25), "125%");
        assert_eq!(text_scale_label(0.5), "50%");
        assert_eq!(text_scale_label(9.0), "200%");
        assert_eq!(text_scale_label(f32::NAN), "100%");
    }

    /// #828: a Windows title is "Settings" in title case.
    #[test]
    fn window_title_is_title_case() {
        assert_eq!(SETTINGS_TITLE, "Wingman Settings");
    }

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
    fn the_model_list_hint_is_built_below_the_provider_rows() {
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
        assert!(top(ollama) < top(hint), "below the Ollama row (#874)");

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
        let m = commit::failed_line(Field::Autostart.label(), AUTOSTART_FAILED);
        assert!(
            !AUTOSTART_FAILED.is_empty() && !m.contains('\u{2014}'),
            "{m}"
        );
        assert!(m.starts_with("Not saved: Start with Windows."), "{m}");
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
            key_mode: key_mode_label(original.hotkeys.key_mode_value()).to_string(),
            double_press_text: original.hotkeys.double_press_ms.to_string(),
            click_away_dismiss: original.options.click_away_dismiss,
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
    fn a_missing_provider_combo_reads_as_no_selection() {
        assert_eq!(provider_choice_from(None), usize::MAX);
        assert_eq!(provider_choice_from(Some(-1)), usize::MAX);
        assert_eq!(provider_choice_from(Some(0)), 0);
        assert_eq!(provider_choice_from(Some(2)), 2);
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

    // -- combo text padding (#854) ----------------------------------------

    #[test]
    fn combo_text_keeps_the_standard_inner_padding_at_each_scale() {
        use windows::Win32::Foundation::RECT;
        let rc = RECT {
            left: 0,
            top: 0,
            right: 400,
            bottom: 20,
        };
        for (dpi, want) in [(96u32, 8), (144, 12), (192, 16), (240, 20)] {
            let t = combo_text_rect(rc, dpi);
            assert_eq!(t.left - rc.left, want, "left pad at {dpi} dpi");
            assert_eq!(rc.right - t.right, want, "right pad at {dpi} dpi");
        }
    }

    #[test]
    fn combo_text_area_is_never_inverted() {
        use windows::Win32::Foundation::RECT;
        for w in [-5, 0, 3, 10, 17] {
            let t = combo_text_rect(
                RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: 20,
                },
                96,
            );
            assert!(t.right >= t.left, "width {w}");
        }
    }

    /// Draws a combo's selected item through the real `draw_combo_item` into
    /// a white bitmap and returns the first column with ink, and the width.
    fn first_ink_column(dpi: u32, disabled: bool) -> (i32, i32) {
        let (first, w, _, _) = probe_combo(dpi, if disabled { ODS_DISABLED } else { 0 });
        (first, w)
    }

    /// [`first_ink_column`] for any item state; also returns the item's
    /// colour at its middle right (clear of the text, as a `COLORREF`) and
    /// at its top-left pixel.
    fn probe_combo(dpi: u32, state: u32) -> (i32, i32, u32, u32) {
        use windows::Win32::Foundation::RECT;
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, DeleteDC, GdiFlush, SelectObject, BITMAPINFO,
            BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
        use windows::Win32::UI::WindowsAndMessaging::{CB_ADDSTRING, CB_SETCURSEL, WS_POPUP};
        ensure_common_controls();
        let class = wide_z(WC_COMBOBOX);
        let name = wide_z("");
        let combo = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(class.as_ptr()),
                PCWSTR(name.as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                    WS_POPUP.0 | (CBS_DROPDOWNLIST | CBS_HASSTRINGS | CBS_OWNERDRAWFIXED) as u32,
                ),
                0,
                0,
                300,
                200,
                None,
                None,
                None,
                None,
            )
        }
        .expect("combo");
        let item = wide_z("Fast");
        unsafe {
            SendMessageW(
                combo,
                CB_ADDSTRING,
                None,
                Some(LPARAM(item.as_ptr() as isize)),
            );
            SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(0)), None);
        }
        let (w, h) = (300i32, 24i32);
        let mut first = -1;
        let (back, corner);
        unsafe {
            let dc = CreateCompatibleDC(None);
            let bmi = BITMAPINFO {
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
            let mut pixels: *mut c_void = std::ptr::null_mut();
            let bmp = CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut pixels, None, 0)
                .expect("DIB");
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let dis = DRAWITEMSTRUCT {
                CtlType: windows::Win32::UI::Controls::DRAWITEMSTRUCT_CTL_TYPE(ODT_COMBOBOX),
                itemID: 0,
                itemState: windows::Win32::UI::Controls::ODS_FLAGS(state),
                hwndItem: combo,
                hDC: dc,
                rcItem: RECT {
                    left: 0,
                    top: 0,
                    right: w,
                    bottom: h,
                },
                ..Default::default()
            };
            FORCED_DPI.with(|f| f.set(Some(dpi)));
            let drawn = draw_combo_item(LPARAM(&dis as *const DRAWITEMSTRUCT as isize));
            FORCED_DPI.with(|f| f.set(None));
            assert!(drawn, "a combo item is drawn by us");
            let _ = GdiFlush();
            let px = std::slice::from_raw_parts(pixels as *const u8, (w * h * 4) as usize);
            let bg = [px[0], px[1], px[2]];
            let at = |x: i32, y: i32| {
                let i = ((y * w + x) * 4) as usize;
                u32::from(px[i + 2]) | u32::from(px[i + 1]) << 8 | u32::from(px[i]) << 16
            };
            corner = at(0, 0);
            back = at(w - 24, h / 2);
            'cols: for x in 0..w {
                for y in 0..h {
                    let i = ((y * w + x) * 4) as usize;
                    if [px[i], px[i + 1], px[i + 2]] != bg {
                        first = x;
                        break 'cols;
                    }
                }
            }
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            let _ = DestroyWindow(combo);
        }
        (first, w, back, corner)
    }

    #[test]
    fn a_focused_closed_combo_field_keeps_the_window_colours_but_a_list_item_highlights() {
        use windows::Win32::Graphics::Gdi::{GetSysColor, COLOR_HIGHLIGHT, COLOR_WINDOW};
        let window = unsafe { GetSysColor(COLOR_WINDOW) } & 0x00FF_FFFF;
        let highlight = unsafe { GetSysColor(COLOR_HIGHLIGHT) } & 0x00FF_FFFF;
        assert_ne!(window, highlight, "the check needs two different colours");
        // The closed field, focused: the combo hands us ODS_SELECTED with it.
        let (_, _, field, _) = probe_combo(96, ODS_COMBOBOXEDIT | ODS_SELECTED | ODS_FOCUS);
        assert_eq!(field, window, "a focused field stays on the window colour");
        // A list item under the pointer or cursor still highlights, in the
        // theme's own hover fill and not the system's bright blue (#1169).
        let (_, _, item, _) = probe_combo(96, ODS_SELECTED);
        let hover = unsafe { edit_style() }.colors.item_hover;
        assert_eq!(item, hover, "a selected list item takes the hover fill");
        assert_ne!(item, highlight, "and not the system highlight");
    }

    #[test]
    fn a_list_item_is_a_rounded_pill_on_the_list_fill() {
        use crate::ui::theme::{CONTROL_RADIUS_DP, MENU_EDGE_PAD_DP};
        if crate::ui::card_buttons::system_high_contrast().is_some() {
            return; // high contrast fills the whole row, square
        }
        let colors = unsafe { edit_style() }.colors;
        for dpi in [96u32, 144, 240] {
            let (_, _, idle, corner) = probe_combo(dpi, 0);
            assert_eq!(idle, colors.fill, "an idle item is the list fill at {dpi}");
            let (_, _, hot, hot_corner) = probe_combo(dpi, ODS_SELECTED);
            assert_eq!(hot, colors.item_hover, "pill fill at {dpi}");
            assert_eq!(corner, colors.fill);
            assert_eq!(
                hot_corner, colors.fill,
                "the pill is inset from the row at {dpi}"
            );
            assert!(to_px(MENU_EDGE_PAD_DP, dpi) > 0 && to_px(CONTROL_RADIUS_DP, dpi) > 0);
        }
    }

    /// High contrast: the hovered row is the system Highlight with
    /// HighlightText on it, whatever the tokens say.
    #[test]
    fn high_contrast_list_rows_use_the_system_highlight() {
        use crate::ui::theme::{rgb, HC_BLACK, HC_WHITE};
        for sys in [HC_BLACK, HC_WHITE] {
            let c = edit_colors(
                crate::ui::theme::Theme::HighContrast,
                &sys,
                rgb(0, 103, 192),
                sys.window,
            );
            assert_eq!(c.item_hover, sys.highlight);
            assert_eq!(c.item_text_hot, sys.highlight_text);
        }
    }

    /// Every combo's drop list is the one the frame and fill are given to.
    /// Debug builds only on purpose: it needs `GalleryWindow`, which release
    /// builds do not contain.
    #[cfg(debug_assertions)]
    #[test]
    fn a_settings_combo_list_gets_the_list_subclass() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::Controls::COMBOBOXINFO;
        use windows::Win32::UI::Shell::GetWindowSubclass;
        use windows::Win32::UI::WindowsAndMessaging::CB_GETCOMBOBOXINFO;
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let w = GalleryWindow::open(HINSTANCE(h.0), Some(96)).expect("open");
        let combo = get_dlg_item(w.hwnd(), ID_ACTIVE_PROVIDER).expect("combo");
        let mut info = COMBOBOXINFO {
            cbSize: std::mem::size_of::<COMBOBOXINFO>() as u32,
            ..Default::default()
        };
        unsafe {
            SendMessageW(
                combo,
                CB_GETCOMBOBOXINFO,
                Some(WPARAM(0)),
                Some(LPARAM(&mut info as *mut COMBOBOXINFO as isize)),
            );
            assert!(!info.hwndList.is_invalid(), "the combo has a list window");
            let mut data = 0usize;
            let found = GetWindowSubclass(
                info.hwndList,
                Some(combo_list_subclass),
                COMBO_LIST_SUBCLASS_ID,
                Some(&mut data),
            );
            assert!(found.as_bool(), "the list is subclassed");
        }
        drop(w);
    }

    /// The combo is a real Settings combo, and the body window really draws it.
    #[cfg(debug_assertions)]
    #[test]
    fn a_settings_combo_is_owner_drawn_and_the_body_draws_it() {
        use windows::Win32::Graphics::Gdi::{CreateCompatibleDC, DeleteDC};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::Controls::{DRAWITEMSTRUCT, DRAWITEMSTRUCT_CTL_TYPE};
        use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongPtrW, GWL_STYLE};
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let w = GalleryWindow::open(HINSTANCE(h.0), Some(96)).expect("open");
        let combo = get_dlg_item(w.hwnd(), ID_ACTIVE_PROVIDER).expect("active provider combo");
        let style = unsafe { GetWindowLongPtrW(combo, GWL_STYLE) } as u32;
        assert_ne!(style & CBS_OWNERDRAWFIXED as u32, 0, "owner-drawn");
        let body = unsafe { GetParent(combo) }.expect("the body");
        unsafe {
            let dc = CreateCompatibleDC(None);
            let dis = DRAWITEMSTRUCT {
                CtlType: DRAWITEMSTRUCT_CTL_TYPE(ODT_COMBOBOX),
                CtlID: ID_ACTIVE_PROVIDER as u32,
                itemID: 0,
                hwndItem: combo,
                hDC: dc,
                rcItem: windows::Win32::Foundation::RECT {
                    left: 0,
                    top: 0,
                    right: 100,
                    bottom: 20,
                },
                ..Default::default()
            };
            let handled = SendMessageW(
                body,
                WM_DRAWITEM,
                Some(WPARAM(ID_ACTIVE_PROVIDER as usize)),
                Some(LPARAM(&dis as *const DRAWITEMSTRUCT as isize)),
            );
            let _ = DeleteDC(dc);
            assert_eq!(handled.0, 1, "the body answers a combo's WM_DRAWITEM");
            // The main window draws rail items only: a list box with another
            // id is left to the default handling.
            let other = DRAWITEMSTRUCT {
                CtlType: DRAWITEMSTRUCT_CTL_TYPE(2),
                CtlID: ID_ACTIVE_PROVIDER as u32,
                ..Default::default()
            };
            let r = SendMessageW(
                w.hwnd(),
                WM_DRAWITEM,
                Some(WPARAM(ID_ACTIVE_PROVIDER as usize)),
                Some(LPARAM(&other as *const DRAWITEMSTRUCT as isize)),
            );
            assert_ne!(r.0, 1, "not a rail item, so not handled as one");
        }
        drop(w);
    }

    #[test]
    fn the_drawn_combo_text_starts_in_from_the_border_at_each_scale() {
        for dpi in [96u32, 144, 192] {
            let (first, _) = first_ink_column(dpi, false);
            assert!(first >= 0, "text was drawn at {dpi} dpi");
            let pad = to_px(crate::ui::theme::COMBO_TEXT_PAD_DP, dpi);
            assert!(
                first >= pad,
                "text starts at {first}px, the padding is {pad}px at {dpi} dpi"
            );
            assert!(first <= pad + to_px(4, dpi), "and not far past it");
        }
    }

    #[test]
    fn a_disabled_combo_draws_its_text_in_from_the_border_too() {
        let (first, _) = first_ink_column(96, true);
        assert!(first >= 8, "starts at {first}px");
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
        assert!(c.reset_btn_y >= c.prompt_top + PROMPT_HEAD_H);
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
    fn enter_presses_the_focused_button_and_applies_the_box_elsewhere() {
        assert_eq!(enter_command(ID_PROMPT_TOGGLE), ID_PROMPT_TOGGLE);
        assert_eq!(enter_command(ID_RESET_PROMPT), ID_RESET_PROMPT);
        assert_eq!(enter_command(ID_UNDO), ID_UNDO);
        assert_eq!(enter_command(ID_OLLAMA_LINK), ID_OLLAMA_LINK);
        // Every other push button too: Enter on a button presses it.
        for button in [
            ID_ADD_PROVIDER,
            ID_OPENAI_SHOW_KEY,
            ID_ANTHROPIC_SHOW_KEY,
            ID_GEMINI_SHOW_KEY,
        ] {
            assert_eq!(enter_command(button), button);
        }
        assert_eq!(enter_command(ID_CARD_SECONDS), ID_APPLY_FOCUSED);
        assert_eq!(enter_command(ID_OPENAI_KEY), ID_APPLY_FOCUSED);
        assert_eq!(enter_command(0), ID_APPLY_FOCUSED);
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

        assert!(get_dlg_item(hwnd, ID_UNDO).is_some());
        assert!(get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).is_some());

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
            // The real clipboard is the point of this check; put the user's
            // own contents back afterwards (#1114).
            crate::inputs::selection::with_real_clipboard_restored(|| {
                if !put_on_clipboard(hwnd, &key) {
                    return;
                }
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
            });
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
            let save = get_dlg_item(w.hwnd(), ID_UNDO).expect("Undo button");
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

    /// The width the body's own scrollbar really takes: window minus client.
    #[cfg(debug_assertions)]
    fn body_scrollbar_px(w: &GalleryWindow) -> i32 {
        let body = get_dlg_item(w.hwnd(), ID_BODY).expect("body");
        let mut wr = windows::Win32::Foundation::RECT::default();
        let mut cr = windows::Win32::Foundation::RECT::default();
        unsafe {
            GetWindowRect(body, &mut wr).expect("GetWindowRect");
            GetClientRect(body, &mut cr).expect("GetClientRect");
        }
        (wr.right - wr.left) - (cr.right - cr.left)
    }

    /// #1016, #1052: a Settings window "at 100%" is a real 96 DPI window
    /// (so its checkbox boxes and scrollbar are real size), and at another
    /// simulated scale the width reserved for the scrollbar is the one the
    /// scrollbar really takes. On a screen above 100% (MEASURED 2026-10-17,
    /// 250%) both fail without the fix: the scrollbar is 42 px, not 17.
    #[cfg(debug_assertions)]
    #[test]
    fn the_gallery_settings_scrollbar_matches_the_reserved_width() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::HiDpi::{
            SetThreadDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let prev =
            unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
        let at_100 = GalleryWindow::open(instance, Some(96)).expect("open at 96");
        assert_eq!(unsafe { GetDpiForWindow(at_100.hwnd()) }, 96, "real 96 DPI");
        assert_eq!(body_scrollbar_px(&at_100), scrollbar_width_px(96));
        drop(at_100);
        let at_150 = GalleryWindow::open(instance, Some(144)).expect("open at 144");
        assert_eq!(body_scrollbar_px(&at_150), scrollbar_width_px(144));
        drop(at_150);
        unsafe { SetThreadDpiAwarenessContext(prev) };
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

    /// #877: a hairline is one physical pixel thick at every scale, where a
    /// 1 dp rect would be two pixels at 150 %.
    #[test]
    fn a_hairline_is_one_pixel_at_every_scale() {
        let l = prompt_footer_layout(700, 918);
        let c = child(16, 40, 588, 1, Anchor::Hairline);
        for dpi in [96u32, 144, 192, 240] {
            let (x, y, w, h) = child_rect_px(&c, &l, &l, dpi);
            assert_eq!(h, 1, "{dpi} dpi");
            assert_eq!((x, y, w), (to_px(16, dpi), to_px(40, dpi), to_px(588, dpi)));
        }
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
    fn child_rect_px_stretches_the_prompt_edit_and_moves_the_reset_row() {
        let old = prompt_footer_layout(700, 918);
        let new = prompt_footer_layout(700, 1000);
        let edit = child(32, 720, 556, old.prompt_edit_h, Anchor::PromptEdit);
        let reset = child(32, old.reset_btn_y, 200, 28, Anchor::ResetBtn);
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

        let save = get_dlg_item(hwnd, ID_UNDO).expect("Undo button");
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
            // The Models page is the longest with every provider showing, so
            // it is the one to scroll.
            unsafe { show_page(inner, Page::Models) };
            unsafe { reveal_providers(inner) };
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
        let save = get_dlg_item(hwnd, ID_UNDO).expect("Undo");
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
        let save = get_dlg_item(hwnd, ID_UNDO).expect("undo");
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
        assert!(footer >= 2, "Undo and the status line");
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

    /// #625: Undo's right edge lines up with the rows' (one margin
    /// before the scrollbar column), and the client is exactly as wide as the
    /// design says. The window used to be too narrow by its frame width and
    /// the footer was pushed over the scrollbar column, so Cancel sat a few
    /// pixels from the window edge.
    #[test]
    fn the_footer_ends_in_line_with_the_groups_and_the_client_is_design_wide() {
        let win = open_layout_window();
        let (client_w, _) = client_size(win.0);
        assert_eq!(client_w, window_width_px(96), "client width");
        let cancel = get_dlg_item(win.0, ID_UNDO).expect("Undo");
        let right = right_in_parent_px(win.0, cancel);
        assert_eq!(
            client_w - right,
            to_px(MARGIN, 96) + scrollbar_width_px(96),
            "Cancel margin"
        );
        // The rows end one margin before the scrollbar column.
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
        // The edit sits in the label's row, to its right, not under it. The
        // row is two lines tall (the title and a note), so the box is centred
        // in it and only overlaps the title's own height.
        let (label_top, edit_top) = (
            top_in_parent_px(body, label),
            top_in_parent_px(body, seconds),
        );
        let label_h = {
            let mut r = windows::Win32::Foundation::RECT::default();
            unsafe { GetClientRect(label, &mut r) }.expect("label rect");
            r.bottom - r.top
        };
        assert!(
            edit_top >= label_top && edit_top < label_top + label_h,
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

    /// Removes every queued message of `id` posted to `to`.
    fn drain_posted(to: HWND, id: u32) {
        while take_posted(to, id).is_some() {}
    }

    /// Takes the newest message of `id` posted to `to`, and drops the older
    /// ones. A real activation change of the hidden window can post an earlier
    /// "left" (count 0) of its own before the close's, so a test that reads the
    /// close's count reads the last one.
    fn take_last_posted(to: HWND, id: u32) -> Option<MSG> {
        let mut last = None;
        while let Some(m) = take_posted(to, id) {
            last = Some(m);
        }
        last
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

    // -- instant apply (#875) ------------------------------------------------

    /// What a commit handed the sink: the field's name and the keys it patches.
    type Sent = std::rc::Rc<std::cell::RefCell<Vec<(String, Vec<String>)>>>;

    /// Gives the window a sink that records, and takes the commit as done.
    fn record_sink(raw: *mut SettingsInner) -> Sent {
        let seen: Sent = Default::default();
        let record = seen.clone();
        unsafe { &mut *raw }.save_sink = Some(Box::new(move |label, changes, _before, _with| {
            record.borrow_mut().push((
                label.to_string(),
                changes.iter().map(|c| c.path().join(".")).collect(),
            ));
            Ok(())
        }));
        seen
    }

    /// Picks `text` in the list `id`, the way the list tells its window.
    fn pick(hwnd: HWND, id: i32, text: &str) {
        let combo = get_dlg_item(hwnd, id).expect("combo");
        combo_select(combo, text);
        unsafe {
            SendMessageW(
                hwnd,
                WM_COMMAND,
                Some(WPARAM(id as usize | ((CBN_SELCHANGE as usize) << 16))),
                Some(LPARAM(combo.0 as isize)),
            );
        }
    }

    /// The box `id` loses the focus, the way the box tells its window.
    fn leave(hwnd: HWND, id: i32) {
        let edit = get_dlg_item(hwnd, id).expect("box");
        unsafe {
            SendMessageW(
                hwnd,
                WM_COMMAND,
                Some(WPARAM(id as usize | ((EN_KILLFOCUS as usize) << 16))),
                Some(LPARAM(edit.0 as isize)),
            );
        }
    }

    /// The app's answer for the write of `label`.
    fn answer(hwnd: HWND, label: &str, result: Result<(), String>) -> bool {
        let answer = WriteAnswer {
            label,
            result: &result,
        };
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETTINGS_WRITE_RESULT,
                None,
                Some(LPARAM(&answer as *const WriteAnswer as isize)),
            )
            .0 != 0
        }
    }

    fn status(hwnd: HWND) -> String {
        get_text(get_dlg_item(hwnd, ID_VALIDATION_MESSAGE).expect("status line"))
    }

    /// The observable of slice 5: picking a card corner sends that one field,
    /// under its name, and the window's picture of what is applied moves on. If
    /// the commit arm were not wired the sink would hear nothing.
    #[test]
    fn a_card_corner_pick_sends_only_that_field_to_the_sink() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            // A box with text typed and not applied is not this pick's business.
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "42");
            pick(hwnd, ID_CARD_CORNER, "Top left");
            assert_eq!(
                *seen.borrow(),
                vec![(
                    "Card corner".to_string(),
                    vec!["ui.card_corner".to_string()]
                )]
            );
            let inner = unsafe { &*raw };
            assert_eq!(inner.original.ui.card_corner, CardCorner::TopLeft);
            assert_eq!(inner.tracker.writes_in_flight(), 1);
            assert!(inner.tracker.undo.is_some());
            assert!(!inner.undo_shown, "Undo waits for the write to be saved");
        });
    }

    /// Every list, checkbox and box applies on its own trigger, and a keystroke
    /// never does.
    #[test]
    fn each_control_applies_on_its_own_trigger_and_never_per_keystroke() {
        use windows::Win32::UI::WindowsAndMessaging::CBN_SELCHANGE;
        const EN_CHANGE: u32 = 0x0300;
        assert_eq!(
            commit_trigger(ID_CARD_CORNER, CBN_SELCHANGE),
            Some(Field::CardCorner)
        );
        assert_eq!(
            commit_trigger(ID_ACTIVE_PROVIDER, CBN_SELCHANGE),
            Some(Field::ActiveProvider)
        );
        assert_eq!(commit_trigger(ID_REDACT, BN_CLICKED), Some(Field::Redact));
        assert_eq!(
            commit_trigger(ID_AUTOSTART, BN_CLICKED),
            Some(Field::Autostart)
        );
        assert_eq!(
            commit_trigger(ID_CARD_SECONDS, EN_KILLFOCUS),
            Some(Field::CardSeconds)
        );
        assert_eq!(
            commit_trigger(ID_OPENAI_KEY, EN_KILLFOCUS),
            Some(Field::OpenAiKey)
        );
        assert_eq!(
            commit_trigger(ID_PROMPT_EDIT, EN_KILLFOCUS),
            Some(Field::Prompt)
        );
        // Typing is not a trigger.
        assert_eq!(commit_trigger(ID_CARD_SECONDS, EN_CHANGE), None);
        assert_eq!(commit_trigger(ID_PROMPT_EDIT, EN_CHANGE), None);
        // The slider applies on release (WM_HSCROLL), the shortcuts when a chord
        // is learned, Mode through the tray's own handler.
        assert_eq!(commit_trigger(ID_TEXT_SCALE_TRACK, 0), None);
        assert_eq!(commit_trigger(ID_CHANGE_PRIMARY, BN_CLICKED), None);
        assert_eq!(commit_trigger(ID_MODE, CBN_SELCHANGE), None);
        // A list does not apply on a click, a checkbox does not on a focus change.
        assert_eq!(commit_trigger(ID_CARD_CORNER, BN_CLICKED), None);
        assert_eq!(commit_trigger(ID_REDACT, EN_KILLFOCUS), None);
    }

    /// Every field has a control in the window, and the control maps back to it
    /// (the shortcuts have a Change button that starts a capture instead).
    #[test]
    fn every_field_has_a_control_that_maps_back_to_it() {
        with_window(Config::default(), |hwnd, _raw| {
            for field in Field::ALL {
                let id = control_of_field(field);
                assert!(
                    get_dlg_item(hwnd, id).is_some(),
                    "{field:?}: no control {id}"
                );
                if trigger_of(field) == Trigger::Capture {
                    assert_eq!(field_of_control(id), None, "{field:?}");
                } else {
                    assert_eq!(field_of_control(id), Some(field), "{field:?}");
                }
            }
        });
    }

    /// A number box applies when it is left, and what is applied is what was
    /// typed.
    #[test]
    fn leaving_a_number_box_applies_it() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), " 42 ");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(
                *seen.borrow(),
                vec![(
                    "Hide card after".to_string(),
                    vec!["ui.card_seconds".to_string()]
                )]
            );
            assert_eq!(unsafe { &*raw }.original.ui.card_seconds, 42);
            // The box shows the number as it parsed.
            assert_eq!(get_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap()), "42");
            // Leaving it again with nothing new sends nothing.
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(seen.borrow().len(), 1);
        });
    }

    /// A number that does not parse is never written: the row says so, the text
    /// stays for fixing.
    #[test]
    fn a_bad_number_is_not_sent_and_the_row_says_error() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            let field = get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap();
            set_text(field, "2ooo");
            leave(hwnd, ID_CARD_SECONDS);
            assert!(seen.borrow().is_empty(), "a bad number was sent");
            assert_eq!(get_text(field), "2ooo", "the text stays for fixing");
            let line = status(hwnd);
            assert!(line.starts_with("Error: Hide card after."), "{line}");
            assert!(!line.contains('\u{2014}'), "{line}");
            assert_eq!(unsafe { &*raw }.tracker.writes_in_flight(), 0);
        });
    }

    /// #393, the observable: picking a key mode in the window sends exactly
    /// `hotkeys.key_mode` to the writer, under its row's name, and the window's
    /// picture of what is applied holds the mode the key reads. Wired to
    /// nothing, the sink would hear nothing.
    #[test]
    fn picking_a_key_mode_sends_only_hotkeys_key_mode() {
        use crate::keypress::KeyMode;
        for (label, mode) in [
            ("Always closes the card", KeyMode::Close),
            ("Always picks an option", KeyMode::Pick),
        ] {
            with_window(Config::default(), |hwnd, raw| {
                let seen = record_sink(raw);
                set_text(get_dlg_item(hwnd, ID_DOUBLE_PRESS).unwrap(), "450");
                pick(hwnd, ID_KEY_MODE, label);
                assert_eq!(
                    *seen.borrow(),
                    vec![(
                        "Wingman key on a card".to_string(),
                        vec!["hotkeys.key_mode".to_string()]
                    )]
                );
                let inner = unsafe { &*raw };
                assert_eq!(inner.original.hotkeys.key_mode_value(), mode);
                assert_eq!(inner.original.hotkeys.double_press_ms, 300);
            });
        }
    }

    /// The press time applies when the box is left, an out of range number is a
    /// row error that sends nothing, and a blank box keeps the applied value.
    #[test]
    fn the_press_time_box_applies_on_leave_and_refuses_out_of_range() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            let field = get_dlg_item(hwnd, ID_DOUBLE_PRESS).unwrap();
            set_text(field, " 450 ");
            leave(hwnd, ID_DOUBLE_PRESS);
            assert_eq!(
                *seen.borrow(),
                vec![(
                    "Double press time".to_string(),
                    vec!["hotkeys.double_press_ms".to_string()]
                )]
            );
            assert_eq!(unsafe { &*raw }.original.hotkeys.double_press_ms, 450);
            set_text(field, "50");
            leave(hwnd, ID_DOUBLE_PRESS);
            assert_eq!(seen.borrow().len(), 1, "an out of range time was sent");
            assert_eq!(get_text(field), "50", "the text stays for fixing");
            let line = status(hwnd);
            assert!(line.starts_with("Error: Double press time."), "{line}");
            assert!(!line.contains('\u{2014}'), "{line}");
            set_text(field, "");
            leave(hwnd, ID_DOUBLE_PRESS);
            assert_eq!(seen.borrow().len(), 1, "a blank box wrote something");
            assert_eq!(unsafe { &*raw }.original.hotkeys.double_press_ms, 450);
        });
    }

    /// A press time written by hand outside the honoured range is not touched by
    /// changing any other row, and the box shows it as written.
    #[test]
    fn a_hand_edited_press_time_and_key_mode_survive_another_rows_change() {
        let mut cfg = Config::default();
        cfg.hotkeys.double_press_ms = 5000;
        cfg.hotkeys.key_mode = "PICK ".to_string();
        with_window(cfg, |hwnd, raw| {
            let seen = record_sink(raw);
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_DOUBLE_PRESS).unwrap()),
                "5000"
            );
            pick(hwnd, ID_CARD_CORNER, "Top left");
            assert_eq!(seen.borrow().len(), 1);
            assert_eq!(seen.borrow()[0].1, vec!["ui.card_corner".to_string()]);
            let inner = unsafe { &*raw };
            assert_eq!(inner.original.hotkeys.double_press_ms, 5000);
            assert_eq!(inner.original.hotkeys.key_mode, "PICK ");
        });
    }

    /// The click-away checkbox applies on a click and sends only its own key.
    #[test]
    fn the_click_away_checkbox_sends_options_click_away_dismiss() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            let box_ = get_dlg_item(hwnd, ID_CLICK_AWAY).unwrap();
            assert!(checkbox_checked(hwnd, ID_CLICK_AWAY), "on by default");
            set_checkbox(box_, false);
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(
                        ID_CLICK_AWAY as usize | ((BN_CLICKED as usize) << 16),
                    )),
                    Some(LPARAM(box_.0 as isize)),
                );
            }
            assert_eq!(
                *seen.borrow(),
                vec![(
                    "Click away closes the card".to_string(),
                    vec!["options.click_away_dismiss".to_string()]
                )]
            );
            assert!(!unsafe { &*raw }.original.options.click_away_dismiss);
        });
        assert_eq!(
            commit_trigger(ID_CLICK_AWAY, BN_CLICKED),
            Some(Field::ClickAway)
        );
    }

    #[test]
    fn build_config_keeps_a_hand_written_key_mode_until_one_is_picked() {
        use crate::keypress::KeyMode;
        let mut original = Config::default();
        original.hotkeys.key_mode = "PICK ".to_string();
        let raw = raw_from(&original);
        assert_eq!(build_config(&original, &raw).hotkeys.key_mode, "PICK ");
        let mut raw = raw_from(&original);
        raw.key_mode = key_mode_label(KeyMode::Close).to_string();
        let picked = build_config(&original, &raw);
        assert_eq!(picked.hotkeys.key_mode_value(), KeyMode::Close);
        let mut raw = raw_from(&original);
        raw.key_mode = String::new();
        assert_eq!(build_config(&original, &raw).hotkeys.key_mode, "PICK ");
        for mode in [KeyMode::Dynamic, KeyMode::Pick, KeyMode::Close] {
            assert_eq!(label_to_key_mode(key_mode_label(mode)), Some(mode));
        }
        assert_eq!(KEY_MODE_LABELS.len(), 3);
    }

    #[test]
    fn build_config_applies_the_press_time_and_click_away() {
        let original = Config::default();
        let mut raw = raw_from(&original);
        raw.double_press_text = "420".to_string();
        raw.click_away_dismiss = false;
        let cfg = build_config(&original, &raw);
        assert_eq!(cfg.hotkeys.double_press_ms, 420);
        assert!(!cfg.options.click_away_dismiss);
        raw.double_press_text = "abc".to_string();
        assert_eq!(build_config(&original, &raw).hotkeys.double_press_ms, 300);
    }

    /// A sink that cannot take the change puts the control back and says why
    /// (rule 7: never a dialog, never a silent no-op).
    #[test]
    fn a_sink_that_refuses_puts_the_control_back_and_says_why() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        with_window(Config::default(), |hwnd, raw| {
            unsafe { &mut *raw }.save_sink = Some(Box::new(|_, _, _, _| {
                Err("The writer is not running.".into())
            }));
            let default_seconds = Config::default().ui.card_seconds.to_string();
            let field = get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap();
            set_text(field, "42");
            leave(hwnd, ID_CARD_SECONDS);
            assert!(unsafe { IsWindow(Some(hwnd)) }.as_bool());
            assert_eq!(get_text(field), default_seconds, "back to what is applied");
            let line = status(hwnd);
            assert!(line.starts_with("Not saved: Hide card after."), "{line}");
            assert!(line.contains("The writer is not running."), "{line}");
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 1);
        });
    }

    /// The writer's answer: saved shows "Saved: Card corner" with Undo; a failure
    /// puts the value back and says why.
    #[test]
    fn a_write_answer_shows_saved_with_undo_and_a_failure_puts_the_value_back() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            pick(hwnd, ID_CARD_CORNER, "Top left");
            assert!(answer(hwnd, "Card corner", Ok(())));
            assert_eq!(status(hwnd), "Saved: Card corner");
            assert!(is_shown(hwnd, ID_UNDO), "Undo comes with the saved line");
            assert_eq!(unsafe { &*raw }.tracker.writes_in_flight(), 0);

            // The next change hides Undo until its own answer arrives.
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "42");
            leave(hwnd, ID_CARD_SECONDS);
            assert!(!is_shown(hwnd, ID_UNDO));
            assert!(answer(
                hwnd,
                "Hide card after",
                Err("The disk is full.".into())
            ));
            let line = status(hwnd);
            assert!(line.starts_with("Not saved: Hide card after."), "{line}");
            assert!(line.contains("The disk is full."), "{line}");
            let seconds = Config::default().ui.card_seconds.to_string();
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap()),
                seconds
            );
            let inner = unsafe { &*raw };
            assert_eq!(
                inner.original.ui.card_seconds,
                Config::default().ui.card_seconds
            );
            assert_eq!(inner.tracker.failed_count(), 1);
            assert!(!is_shown(hwnd, ID_UNDO), "a failed row has nothing to undo");
            // The corner that did save stays.
            assert_eq!(inner.original.ui.card_corner, CardCorner::TopLeft);
            // An answer nobody asked for is left for the app to say.
            assert!(!answer(hwnd, "Card corner", Ok(())));
            assert!(!answer(hwnd, "Not a field", Ok(())));
        });
    }

    /// Undo sends the old value through the same sink and shows it at once.
    #[test]
    fn undo_sends_the_old_value_and_shows_it() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            pick(hwnd, ID_CARD_CORNER, "Top left");
            assert!(answer(hwnd, "Card corner", Ok(())));
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_COMMAND,
                    Some(WPARAM(ID_UNDO as usize | ((BN_CLICKED as usize) << 16))),
                    Some(LPARAM(0)),
                );
            }
            assert_eq!(seen.borrow().len(), 2, "the old value was sent");
            assert_eq!(seen.borrow()[1].1, vec!["ui.card_corner".to_string()]);
            let corner = get_dlg_item(hwnd, ID_CARD_CORNER).unwrap();
            assert_eq!(get_text(corner), card_corner_label(CardCorner::BottomRight));
            assert!(!is_shown(hwnd, ID_UNDO));
            assert_eq!(
                unsafe { &*raw }.original.ui.card_corner,
                CardCorner::BottomRight
            );
            assert!(answer(hwnd, "Card corner", Ok(())));
            assert_eq!(status(hwnd), "Undone: Card corner");
            assert!(!is_shown(hwnd, ID_UNDO), "no undoing an undo");
        });
    }

    /// Esc with text typed and not applied puts the applied text back and keeps
    /// the window; with nothing pending it closes.
    #[test]
    fn esc_puts_back_pending_text_and_closes_only_when_nothing_is_pending() {
        with_open_window(|hwnd, _owner| {
            let inner =
                unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner) };
            let field = get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap();
            let applied = Config::default().ui.card_seconds.to_string();
            set_text(field, "99");
            escape_with(inner, field);
            assert_eq!(get_text(field), applied);
            assert!(take_posted(hwnd, WM_CLOSE).is_none(), "it kept the window");
            // Nothing pending now: Esc closes.
            escape_with(inner, field);
            assert!(
                take_posted(hwnd, WM_CLOSE).is_some(),
                "it closed the window"
            );
            // A list is never "pending".
            let combo = get_dlg_item(hwnd, ID_CARD_CORNER).unwrap();
            escape_with(inner, combo);
            assert!(take_posted(hwnd, WM_CLOSE).is_some());
        });
    }

    /// A window closed with a row still failed tells the owner how many, so one
    /// card says so; one closed with none says zero.
    #[test]
    fn closing_with_a_failed_row_tells_the_owner_how_many() {
        with_open_window(|hwnd, owner| {
            let inner =
                unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner) };
            inner.tracker.fail(Field::CardCorner, "disk full");
            inner.tracker.fail(Field::Redact, "disk full");
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            close_now();
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT)
                .expect("closing tells the owner");
            assert_eq!(left.wParam.0, 2);
        });
        with_open_window(|_hwnd, owner| {
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            close_now();
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT).unwrap();
            assert_eq!(left.wParam.0, 0);
        });
    }

    /// The owner window receives the field's change from the real sink: one
    /// field, its name, and the config with it. The window stays open.
    #[test]
    fn a_pick_posts_one_field_to_the_owner_and_the_window_stays() {
        use windows::Win32::UI::WindowsAndMessaging::IsWindow;
        with_open_window(|hwnd, owner| {
            pick(hwnd, ID_CARD_CORNER, "Top right");
            let posted = take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE)
                .expect("the change must be posted to the owner window");
            let save = unsafe { *Box::from_raw(posted.lParam.0 as *mut crate::app::SettingsSave) };
            let paths: Vec<String> = save.changes.iter().map(|c| c.path().join(".")).collect();
            assert_eq!(save.label, "Card corner");
            assert_eq!(paths, vec!["ui.card_corner"]);
            assert_eq!(save.edited.ui.card_corner, CardCorner::TopRight);
            assert_eq!(save.original.ui.card_corner, CardCorner::BottomRight);
            assert!(
                unsafe { IsWindow(Some(hwnd)) }.as_bool(),
                "no Save closes it"
            );
        });
    }

    /// The slice's done condition without the app in the middle: what the window
    /// posts for a card corner, written the way the writer thread writes it onto a
    /// file that has a comment, an unknown key and a hand edit, changes that one
    /// key and nothing else. A test-only path (rule 9).
    #[test]
    fn a_card_corner_pick_changes_one_key_of_the_file_and_leaves_the_rest() {
        let path = std::env::temp_dir().join(format!(
            "wingman-settings-instant-{}.toml",
            std::process::id()
        ));
        std::fs::write(
            &path,
            "# my notes\nmystery = 1\n\n[ui]\ncard_seconds = 11 # hand edit\ncard_corner = \"bottom_right\"\n",
        )
        .unwrap();
        with_open_window(|hwnd, owner| {
            pick(hwnd, ID_CARD_CORNER, "Top right");
            let posted = take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE).expect("posted");
            let save = unsafe { *Box::from_raw(posted.lParam.0 as *mut crate::app::SettingsSave) };
            crate::config_writer::write_changes(&path, &save.changes).expect("written");
        });
        let after = std::fs::read_to_string(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(after.contains("card_corner = \"top_right\""), "{after}");
        assert!(
            after.contains("# my notes") && after.contains("mystery = 1"),
            "{after}"
        );
        assert!(after.contains("card_seconds = 11 # hand edit"), "{after}");
    }

    /// A number that does not parse is a failed row, not only a status line: the
    /// next commit rewrites the status line, and closing must still say that the
    /// value was dropped. Through the real arms: leave the box, commit another
    /// field, close.
    #[test]
    fn an_invalid_number_survives_the_next_commit_and_is_counted_on_close() {
        with_open_window(|hwnd, owner| {
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "2ooo");
            leave(hwnd, ID_CARD_SECONDS);
            assert!(status(hwnd).starts_with("Error"), "{}", status(hwnd));
            pick(hwnd, ID_CARD_CORNER, "Top left");
            assert!(answer(hwnd, "Card corner", Ok(())));
            assert!(!status(hwnd).starts_with("Error"), "the status moved on");
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            close_now();
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT)
                .expect("closing tells the owner");
            assert_eq!(left.wParam.0, 1, "the dropped number must be counted");
        });
    }

    /// Fixing the number (or putting the applied one back) clears the row.
    #[test]
    fn a_good_commit_of_the_field_clears_its_failed_row() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            let seconds = get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap();
            set_text(seconds, "2ooo");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 1);
            set_text(seconds, "9");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
            // The applied value typed back is also a fix.
            set_text(seconds, "2ooo");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 1);
            set_text(seconds, "9");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
        });
    }

    /// Two writes of one field are waiting and the older one fails: the window
    /// keeps the newer value instead of going back past it.
    #[test]
    fn an_older_failed_write_leaves_the_newer_value_showing() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            pick(hwnd, ID_CARD_CORNER, "Top left");
            pick(hwnd, ID_CARD_CORNER, "Top right");
            assert!(answer(hwnd, "Card corner", Err("disk full".to_string())));
            let inner = unsafe { &*raw };
            assert_eq!(inner.original.ui.card_corner, CardCorner::TopRight);
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_CARD_CORNER).unwrap()),
                card_corner_label(CardCorner::TopRight)
            );
            assert!(answer(hwnd, "Card corner", Ok(())));
            assert_eq!(
                unsafe { &*raw }.tracker.failed_count(),
                0,
                "the newer write saved, so the row is fine"
            );
            assert_eq!(
                unsafe { &*raw }.original.ui.card_corner,
                CardCorner::TopRight
            );
        });
    }

    /// A key box applies on Enter or when the focus moves to another control of
    /// Settings, never because the window was deactivated (alt-tab): a half-typed
    /// key would be stored and the old one lost.
    #[test]
    fn a_key_box_is_not_applied_when_the_window_is_deactivated() {
        use windows::Win32::UI::WindowsAndMessaging::WM_ACTIVATE;
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(key, "sk-half-typed");
            unsafe {
                SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(0)), Some(LPARAM(0)));
            }
            leave(hwnd, ID_OPENAI_KEY);
            assert!(seen.borrow().is_empty(), "alt-tab stored a key");
            assert_eq!(get_text(key), "sk-half-typed", "the text stays for later");
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
            // Back in the window, leaving the box for another control applies it.
            unsafe {
                SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(1)), Some(LPARAM(0)));
            }
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1, "moving inside Settings applies it");
        });
    }

    /// A key of the wrong shape for its vendor is not stored: the row errors
    /// and the text stays for fixing.
    #[test]
    fn a_key_that_does_not_fit_its_vendor_is_not_stored() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            let key = get_dlg_item(hwnd, ID_ANTHROPIC_KEY).unwrap();
            set_text(key, "sk-this-is-an-openai-key");
            leave(hwnd, ID_ANTHROPIC_KEY);
            assert!(seen.borrow().is_empty(), "a wrong key was stored");
            assert!(status(hwnd).starts_with("Error"), "{}", status(hwnd));
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 1);
            assert_eq!(get_text(key), "sk-this-is-an-openai-key");
            set_text(key, "sk-ant-right-one");
            leave(hwnd, ID_ANTHROPIC_KEY);
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
        });
    }

    // -- typed key check (#876) ----------------------------------------------

    use crate::provider::model_list::{KeyCheck, Vendor};
    use std::sync::atomic::AtomicUsize;

    static ONCE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static OFFLINE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static SHAPE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn accepts(_: Vendor, _: &str) -> KeyCheck {
        KeyCheck::Accepted(vec!["gpt-5".to_string()], true)
    }
    fn accepts_and_counts(_: Vendor, _: &str) -> KeyCheck {
        ONCE_CALLS.fetch_add(1, Ordering::SeqCst);
        KeyCheck::Accepted(vec!["claude-x".to_string()], true)
    }
    fn accepts_first_page(_: Vendor, _: &str) -> KeyCheck {
        KeyCheck::Accepted(vec!["page-one".to_string()], false)
    }
    fn rejects(_: Vendor, _: &str) -> KeyCheck {
        KeyCheck::Rejected
    }
    fn unsure(_: Vendor, _: &str) -> KeyCheck {
        KeyCheck::Unchecked
    }
    fn counts_offline(_: Vendor, _: &str) -> KeyCheck {
        OFFLINE_CALLS.fetch_add(1, Ordering::SeqCst);
        KeyCheck::Unchecked
    }
    fn counts_shape(_: Vendor, _: &str) -> KeyCheck {
        SHAPE_CALLS.fetch_add(1, Ordering::SeqCst);
        KeyCheck::Unchecked
    }
    fn auto() -> crate::mode::Mode {
        crate::mode::Mode::Auto
    }
    fn offline() -> crate::mode::Mode {
        crate::mode::Mode::Offline
    }

    fn with_vendor(
        raw: *mut SettingsInner,
        check: fn(Vendor, &str) -> KeyCheck,
        mode: fn() -> crate::mode::Mode,
    ) {
        unsafe { &mut *raw }.key_seams = KeySeams {
            check,
            mode,
            fetch_all: KeySeams::REAL.fetch_all,
            spawn: spawn_key_check_thread,
            late_card: KeySeams::REAL.late_card,
        };
    }

    /// #1140: a check that read only the first page of a longer list leaves the
    /// dropdown with the full list, not that page, and the key as not yet fetched.
    #[test]
    fn a_first_page_check_ends_with_the_full_list() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            with_vendor(raw, accepts_first_page, auto);
            unsafe { &mut *raw }.key_seams.fetch_all =
                |_, _| Ok(vec!["page-one".to_string(), "page-two".to_string()]);
            set_text(get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap(), "sk-a-whole-key");
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            loop {
                let got = unsafe { &*raw }.cloud_models.lock().unwrap()[0].clone();
                if got.as_ref().is_some_and(|l| l.len() == 2) {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "no full list: {got:?}"
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
    }

    /// #1140: a worker thread that cannot start must not leave the row on
    /// "Checking key..." for good: it settles as a check that could not finish.
    #[test]
    fn a_key_check_whose_thread_cannot_start_does_not_hang_the_row() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, accepts, auto);
            unsafe { &mut *raw }.key_seams.spawn =
                |_| Err(std::io::Error::other("no threads left"));
            set_text(get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap(), "sk-a-whole-key");
            leave(hwnd, ID_OPENAI_KEY);
            assert!(!unsafe { &*raw }.key_checks.is_pending(0));
            assert_eq!(seen.borrow().len(), 1, "the key is saved unchecked");
            assert!(answer(hwnd, "OpenAI key", Ok(())));
            assert!(status(hwnd).contains("Could not check"), "{}", status(hwnd));
        });
    }

    /// Delivers the check worker's answer, the way the message loop would. Fails
    /// the test if none comes.
    fn settle_key_check(hwnd: HWND) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            if let Some(m) = take_posted(hwnd, WM_KEY_CHECKED) {
                unsafe { SendMessageW(hwnd, WM_KEY_CHECKED, Some(m.wParam), Some(m.lParam)) };
                return;
            }
            assert!(std::time::Instant::now() < deadline, "no answer came");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    /// The observable of slice 6: a well-shaped key is asked of its vendor before
    /// it is written. The row says "Checking key..." and nothing is stored until
    /// the answer; a good answer stores it and the status line says saved. If the
    /// check were wired to nothing the sink would hear the key at once.
    #[test]
    fn a_good_key_is_stored_only_after_its_vendor_accepts_it() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, accepts, auto);
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(key, "sk-a-whole-key");
            leave(hwnd, ID_OPENAI_KEY);
            assert!(
                seen.borrow().is_empty(),
                "stored before the vendor answered"
            );
            assert_eq!(status(hwnd), "Checking key...");
            assert!(unsafe { &*raw }.key_checks.is_pending(0));
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1, "an accepted key is stored");
            assert!(!unsafe { &*raw }.key_checks.is_pending(0));
            assert!(answer(hwnd, "OpenAI key", Ok(())));
            assert_eq!(status(hwnd), "Saved: OpenAI key");
            assert!(!status(hwnd).contains("sk-a-whole-key"));
            // The key is the applied one now: no second check for the same text.
            assert_eq!(get_text(key), key_field_display("sk-a-whole-key"));
        });
    }

    /// A refused key is not stored: the row errors in plain words and the text
    /// stays typed so it can be corrected. Typing again starts a fresh check.
    #[test]
    fn a_key_the_vendor_refuses_is_not_stored_and_stays_typed() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, rejects, auto);
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(key, "sk-a-wrong-key");
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            assert!(seen.borrow().is_empty(), "a refused key was stored");
            assert_eq!(
                status(hwnd),
                "Error: OpenAI key. OpenAI did not accept that key. Check it and try again."
            );
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 1);
            assert_eq!(get_text(key), "sk-a-wrong-key", "the text stays for fixing");
            assert!(!status(hwnd).contains("sk-a-wrong-key"));
            // A corrected key is a fresh attempt: the old refusal no longer stands.
            with_vendor(raw, accepts, auto);
            set_text(key, "sk-a-right-key");
            leave(hwnd, ID_OPENAI_KEY);
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1);
        });
    }

    /// A check that could not finish does not hold the key back: it is written
    /// and the status line says it was not checked.
    #[test]
    fn a_key_that_could_not_be_checked_is_saved_with_a_note() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, unsure, auto);
            set_text(get_dlg_item(hwnd, ID_GEMINI_KEY).unwrap(), "AIzaWholeKey");
            leave(hwnd, ID_GEMINI_KEY);
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1);
            assert!(answer(hwnd, "Gemini key", Ok(())));
            assert_eq!(status(hwnd), "Saved. Could not check it right now.");
        });
    }

    /// The note goes with its own write: a failed write shows the failure, and the
    /// next key saved says plain "Saved".
    #[test]
    fn the_unchecked_note_is_not_kept_for_a_later_write() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            with_vendor(raw, unsure, auto);
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(key, "sk-first-key");
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            assert!(answer(hwnd, "OpenAI key", Err("Disk full.".to_string())));
            assert!(
                status(hwnd).starts_with("Not saved: OpenAI key."),
                "{}",
                status(hwnd)
            );
            with_vendor(raw, accepts, auto);
            set_text(key, "sk-second-key");
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            assert!(answer(hwnd, "OpenAI key", Ok(())));
            assert_eq!(status(hwnd), "Saved: OpenAI key");
        });
    }

    /// Offline keeps every request on this computer: the key is shape-checked and
    /// saved, no vendor is asked, and the status line says why.
    #[test]
    fn in_offline_mode_a_key_is_saved_without_asking_its_vendor() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, counts_offline, offline);
            let before = OFFLINE_CALLS.load(Ordering::SeqCst);
            set_text(get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap(), "sk-offline-key");
            leave(hwnd, ID_OPENAI_KEY);
            assert_eq!(seen.borrow().len(), 1, "saved at once");
            assert!(!unsafe { &*raw }.key_checks.is_pending(0));
            assert!(answer(hwnd, "OpenAI key", Ok(())));
            assert_eq!(status(hwnd), "Saved. Not checked in Offline mode.");
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(
                OFFLINE_CALLS.load(Ordering::SeqCst),
                before,
                "a request was made"
            );
            assert!(take_posted(hwnd, WM_KEY_CHECKED).is_none());
        });
    }

    /// A key of the wrong shape never leaves the PC: it is refused before any
    /// request, as before.
    #[test]
    fn a_wrong_shaped_key_is_never_sent_to_a_vendor() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, counts_shape, auto);
            let before = SHAPE_CALLS.load(Ordering::SeqCst);
            set_text(
                get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap(),
                "AIza-a-gemini-key",
            );
            leave(hwnd, ID_OPENAI_KEY);
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert!(seen.borrow().is_empty());
            assert!(status(hwnd).starts_with("Error"), "{}", status(hwnd));
            assert_eq!(
                SHAPE_CALLS.load(Ordering::SeqCst),
                before,
                "the key was sent"
            );
        });
    }

    /// An answer for a key the user has typed over since is ignored: nothing is
    /// stored for it, and "Checking key..." does not stay up.
    #[test]
    fn an_answer_for_a_key_typed_over_since_is_ignored() {
        with_window(Config::default(), |hwnd, raw| {
            let seen = record_sink(raw);
            with_vendor(raw, accepts, auto);
            let key = get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap();
            set_text(key, "sk-first-draft");
            leave(hwnd, ID_OPENAI_KEY);
            set_text(key, "sk-second-draft");
            settle_key_check(hwnd);
            assert!(seen.borrow().is_empty(), "the stale answer stored a key");
            assert_ne!(status(hwnd), "Checking key...");
            assert_eq!(unsafe { &*raw }.tracker.failed_count(), 0);
            assert_eq!(get_text(key), "sk-second-draft");
            // The new text commits and checks on its own.
            leave(hwnd, ID_OPENAI_KEY);
            settle_key_check(hwnd);
            assert_eq!(seen.borrow().len(), 1);
        });
    }

    /// Enter after leaving the box does not send the same key out twice.
    #[test]
    fn the_same_key_is_asked_of_its_vendor_once() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            with_vendor(raw, accepts_and_counts, auto);
            let before = ONCE_CALLS.load(Ordering::SeqCst);
            set_text(
                get_dlg_item(hwnd, ID_ANTHROPIC_KEY).unwrap(),
                "sk-ant-one-key",
            );
            leave(hwnd, ID_ANTHROPIC_KEY);
            leave(hwnd, ID_ANTHROPIC_KEY);
            settle_key_check(hwnd);
            std::thread::sleep(std::time::Duration::from_millis(50));
            assert_eq!(ONCE_CALLS.load(Ordering::SeqCst) - before, 1);
            assert!(
                take_posted(hwnd, WM_KEY_CHECKED).is_none(),
                "a second answer"
            );
        });
    }

    /// Esc puts the applied text back, which also ends a row that failed for the
    /// text that was there.
    #[test]
    fn esc_after_a_bad_entry_clears_the_failed_row() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            let inner = unsafe { &mut *raw };
            let seconds = get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap();
            set_text(seconds, "2ooo");
            leave(hwnd, ID_CARD_SECONDS);
            assert_eq!(inner.tracker.failed_count(), 1);
            escape_with(inner, seconds);
            assert_eq!(inner.tracker.failed_count(), 0, "a false unsaved count");
            assert!(!status(hwnd).starts_with("Error"), "{}", status(hwnd));
        });
    }

    /// A key typed, then the window deactivated, then closed: one that does not
    /// fit its vendor is counted as not saved, one still being checked is saved
    /// unchecked (#1130). Never silently dropped.
    #[test]
    fn closing_while_inactive_does_not_drop_a_typed_key() {
        use windows::Win32::UI::WindowsAndMessaging::WM_ACTIVATE;
        let away = |hwnd: HWND, key: &str| unsafe {
            set_text(get_dlg_item(hwnd, ID_OPENAI_KEY).unwrap(), key);
            SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(0)), Some(LPARAM(0)));
            leave(hwnd, ID_OPENAI_KEY);
        };
        with_open_window(|hwnd, owner| {
            away(hwnd, "not-a-key");
            // Drain the activation message so the left count is the close's.
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            // The close button: `WM_CLOSE`, not a bare destroy.
            unsafe { SendMessageW(hwnd, WM_CLOSE, None, None) };
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT)
                .expect("closing tells the owner");
            assert_eq!(left.wParam.0, 1, "a key that does not fit is counted");
        });
        with_open_window(|hwnd, owner| {
            // The check is held until after the close, so the close finds it
            // still out (a check that answers at once is the window's, not the
            // close's, and a loop-less test never delivers its answer).
            let release = gate_check(hwnd, "sk-a-whole-key");
            away(hwnd, "sk-a-whole-key");
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            unsafe { SendMessageW(hwnd, WM_CLOSE, None, None) };
            let _ = release.send(());
            // #1130: the key was still being checked, so closing saves it
            // unchecked (only a refusal blocks a key) instead of losing it.
            assert!(take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE).is_some());
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT)
                .expect("closing tells the owner");
            assert_eq!(left.wParam.0, 0, "a saved key is not counted as lost");
        });
    }

    // -- a key still being checked when the window closes (#1130) ------------

    static GATES: std::sync::Mutex<
        Option<std::collections::HashMap<String, std::sync::mpsc::Receiver<()>>>,
    > = std::sync::Mutex::new(None);
    static CHECKS_DONE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

    /// A vendor stub that holds each key's answer until the test releases it:
    /// a key with "bad" in it is refused, any other is "could not check".
    fn gated_check(_: Vendor, key: &str) -> KeyCheck {
        let gate = GATES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_mut()
            .and_then(|g| g.remove(key));
        if let Some(rx) = gate {
            let _ = rx.recv_timeout(std::time::Duration::from_secs(10));
        }
        CHECKS_DONE.fetch_add(1, Ordering::SeqCst);
        if key.contains("bad") {
            KeyCheck::Rejected
        } else {
            KeyCheck::Unchecked
        }
    }

    /// Puts the gated vendor behind the window (real late-card post, real
    /// owner) and holds `key`'s answer. Send on the result to release it.
    fn gate_check(hwnd: HWND, key: &str) -> std::sync::mpsc::Sender<()> {
        let (tx, rx) = std::sync::mpsc::channel();
        GATES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_or_insert_with(Default::default)
            .insert(key.to_string(), rx);
        let inner = unsafe { &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SettingsInner) };
        inner.key_seams = KeySeams {
            check: gated_check,
            mode: auto,
            fetch_all: KeySeams::REAL.fetch_all,
            spawn: spawn_key_check_thread,
            late_card: crate::app::post_late_key_refusal,
        };
        tx
    }

    fn type_key(hwnd: HWND, id: i32, key: &str) {
        set_text(get_dlg_item(hwnd, id).unwrap(), key);
        leave(hwnd, id);
    }

    /// Waits until `n` more stub checks have returned, then a moment for the
    /// worker to act on its verdict.
    fn wait_checks(before: usize, n: usize) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while CHECKS_DONE.load(Ordering::SeqCst) < before + n {
            assert!(std::time::Instant::now() < deadline, "a check never ended");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }

    fn queued(headline_start: &str) -> usize {
        crate::app::queued_shell_open_titles()
            .iter()
            .filter(|t| t.starts_with(headline_start))
            .count()
    }

    /// The worker glue, end to end: a key still being checked at close is saved
    /// unchecked and the window does not count it as lost; when the vendor then
    /// refuses it, exactly one failure is queued for the app and the app is told
    /// once (`WM_APP_SHELL_OPEN_DONE`). Wired to nothing, either nothing would be
    /// saved or no card would come.
    #[test]
    fn a_key_refused_after_the_window_closed_saved_it_is_one_card() {
        with_open_window(|hwnd, owner| {
            let release = gate_check(hwnd, "sk-bad-whole-key-1");
            drain_posted(owner, crate::app::WM_APP_SHELL_OPEN_DONE);
            while queued("OpenAI did not accept the key") > 0 {
                crate::app::retract_shell_open_failure(&commit::late_refusal_card(0).0);
            }
            let before = CHECKS_DONE.load(Ordering::SeqCst);
            type_key(hwnd, ID_OPENAI_KEY, "sk-bad-whole-key-1");
            drain_posted(owner, crate::app::WM_APP_SETTINGS_SAVE);
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            unsafe { SendMessageW(hwnd, WM_CLOSE, None, None) };
            assert!(take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE).is_some());
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT).unwrap();
            assert_eq!(left.wParam.0, 0);
            assert_eq!(queued("OpenAI did not accept the key"), 0, "too early");
            let _ = release.send(());
            wait_checks(before, 1);
            assert_eq!(queued("OpenAI did not accept the key"), 1);
            assert!(take_posted(owner, crate::app::WM_APP_SHELL_OPEN_DONE).is_some());
            assert!(take_posted(owner, crate::app::WM_APP_SHELL_OPEN_DONE).is_none());
            crate::app::retract_shell_open_failure(&commit::late_refusal_card(0).0);
        });
    }

    /// Key A is out for checking, key B replaces it, the window closes: B is
    /// saved, and A's refusal (A is not what was saved) raises no card.
    #[test]
    fn a_replaced_keys_refusal_is_not_a_late_card() {
        with_open_window(|hwnd, owner| {
            let a = "sk-ant-bad-whole-key-a";
            let b = "sk-ant-ok-whole-key-b";
            let release_a = gate_check(hwnd, a);
            let release_b = gate_check(hwnd, b);
            drain_posted(owner, crate::app::WM_APP_SHELL_OPEN_DONE);
            let before = CHECKS_DONE.load(Ordering::SeqCst);
            type_key(hwnd, ID_ANTHROPIC_KEY, a);
            type_key(hwnd, ID_ANTHROPIC_KEY, b);
            drain_posted(owner, crate::app::WM_APP_SETTINGS_SAVE);
            unsafe { SendMessageW(hwnd, WM_CLOSE, None, None) };
            assert!(take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE).is_some());
            let _ = release_a.send(());
            let _ = release_b.send(());
            wait_checks(before, 2);
            assert_eq!(queued("Anthropic did not accept the key"), 0);
            assert!(take_posted(owner, crate::app::WM_APP_SHELL_OPEN_DONE).is_none());
        });
    }

    /// A plain destroy (not the close button) with a check out: nothing saved
    /// it, so it is counted as unsaved, not dropped silently.
    #[test]
    fn a_plain_destroy_with_a_key_check_out_counts_the_key_as_unsaved() {
        with_open_window(|hwnd, owner| {
            let release = gate_check(hwnd, "sk-ok-whole-key-3");
            type_key(hwnd, ID_OPENAI_KEY, "sk-ok-whole-key-3");
            drain_posted(owner, crate::app::WM_APP_SETTINGS_SAVE);
            drain_posted(owner, crate::app::WM_APP_SETTINGS_LEFT);
            close_now();
            assert!(take_posted(owner, crate::app::WM_APP_SETTINGS_SAVE).is_none());
            let left = take_last_posted(owner, crate::app::WM_APP_SETTINGS_LEFT).unwrap();
            assert_eq!(left.wParam.0, 1, "the unsaved key is counted");
            let _ = release.send(());
        });
    }

    /// A hand edit shows in every row that is not being typed in, and the row
    /// being typed in keeps its text.
    #[test]
    fn a_hand_edit_shows_in_untouched_rows_but_not_in_the_row_being_typed() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let mut fresh = Config::default();
            fresh.ui.card_corner = CardCorner::TopRight;
            fresh.ui.card_seconds = 99;
            fresh.capture.redact = !Config::default().capture.redact;
            set_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap(), "5");
            adopt_file(inner, &fresh);
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_CARD_CORNER).unwrap()),
                card_corner_label(CardCorner::TopRight)
            );
            assert_eq!(
                checkbox_checked(hwnd, ID_REDACT),
                fresh.capture.redact,
                "the checkbox follows the file"
            );
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_CARD_SECONDS).unwrap()),
                "5",
                "typed text is not overwritten"
            );
            assert_eq!(inner.original.ui.card_corner, CardCorner::TopRight);
            assert_eq!(inner.baseline.ui.card_seconds, 99);
            // Nothing was sent: a refresh never writes.
            assert_eq!(inner.tracker.writes_in_flight(), 0);
        });
    }

    /// While a write is in flight the file is about to change, so what was read
    /// is not shown.
    #[test]
    fn a_re_read_during_a_write_waits_for_the_next_activation() {
        with_window(Config::default(), |hwnd, raw| {
            record_sink(raw);
            pick(hwnd, ID_CARD_CORNER, "Top left");
            let inner = unsafe { &mut *raw };
            let mut fresh = Config::default();
            fresh.ui.card_corner = CardCorner::BottomLeft;
            *inner.refresh_slot.lock().unwrap() = Some(Refreshed {
                mtime: None,
                config: Some(fresh),
            });
            take_refresh(inner);
            assert_eq!(
                get_text(get_dlg_item(hwnd, ID_CARD_CORNER).unwrap()),
                card_corner_label(CardCorner::TopLeft)
            );
            assert!(inner.file_mtime.is_none(), "the time is not taken");
        });
    }

    /// Showing a field puts the control at that value, for every field that has
    /// a plain control: what a failed write, Undo and a hand edit rely on.
    #[test]
    fn show_field_puts_every_plain_control_at_the_value() {
        use crate::ui::settings_commit::tests::changed;
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let base = Config::default();
            for field in Field::ALL {
                if field.key_vendor().is_some()
                    || matches!(field, Field::Autostart | Field::ActiveProvider)
                {
                    continue;
                }
                let target = changed(field, &base);
                show_field(inner, field, &target);
                // The form the commit reads: the staged chords, and the key
                // mode and press time from their own rows (#393).
                let form = finalize_config(inner, &read_raw_form(inner));
                assert!(
                    !commit::differs(field, &form, &target),
                    "{field:?} was not shown"
                );
            }
        });
    }

    /// The old Save and Cancel are gone, the commit arms are wired, and nothing
    /// that reads or writes the file runs on the window's thread.
    #[test]
    fn the_window_has_no_save_and_cancel_and_applies_through_its_arms() {
        let src = include_str!("settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        assert!(code.contains("fn commit_field("), "the module moved");
        assert!(!code.contains("ID_SAVE") && !code.contains("ID_CANCEL"));
        assert!(!code.contains("\"Save\"") && !code.contains("\"Cancel\""));
        // The arms that apply: the window procedure ends where `handle_command`
        // starts, so a call elsewhere in the file cannot satisfy the check.
        let start = code.find("unsafe extern \"system\" fn wndproc").unwrap();
        let end = code.find("\nfn handle_command(").unwrap();
        assert!(
            start < end,
            "the module was reordered: slice the function by hand"
        );
        let wndproc = &code[start..end];
        assert!(wndproc.len() > 500, "the slice is not the window procedure");
        for arm in [
            "commit_focused(inner);",
            "commit_field(inner, Field::TextScale)",
            "WM_SETTINGS_WRITE_RESULT =>",
            "WM_SETTINGS_REFRESH =>",
            "start_refresh(inner);",
        ] {
            assert!(wndproc.contains(arm), "wndproc has no `{arm}`");
        }
        assert!(
            wndproc.contains("TB_ENDTRACK") && wndproc.contains("WM_HSCROLL =>"),
            "the slider applies on release"
        );
        let command_end = code[end..].find("\nfn commit_focused(").unwrap() + end;
        let command = &code[end..command_end];
        assert!(command.contains("commit_trigger(id, notify)"));
        assert!(command.contains("ID_UNDO if notify == BN_CLICKED"));
        assert!(command.contains("ID_APPLY_FOCUSED => commit_focused(inner)"));
        // No timer but the one a pending shortcut needs and the "Checking
        // Ollama" cue (#828, stopped by the result), and no polling.
        assert_eq!(code.matches("SetTimer(").count(), 2, "a timer was added");
        assert!(code.contains("fn stop_checking_animation("));
        // The commit path never touches the file.
        let a = code.find("fn commit_field(").unwrap();
        let b = code.find("fn commit_autostart(").unwrap();
        let commit_src = &code[a..b];
        for banned in ["std::fs", "save_to", "Config::save", "write_changes"] {
            assert!(!commit_src.contains(banned), "commit_field uses {banned}");
        }
        // The re-read is on a worker, not in the window procedure.
        let refresh = &code[code.find("fn start_refresh(").unwrap()..];
        assert!(refresh.contains("thread::Builder::new()"));
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
        let before = INNERS_DROPPED.with(|c| c.get());
        let opened = open_impl(
            instance,
            HWND(std::ptr::null_mut()),
            &Config::default(),
            FAIL_CLASS,
            false,
        );
        assert!(!opened, "the window cannot be created");
        assert_eq!(
            INNERS_DROPPED.with(|c| c.get()) - before,
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
        let before = INNERS_DROPPED.with(|c| c.get());
        let opened = open_impl(
            HINSTANCE(h.0),
            HWND(std::ptr::null_mut()),
            &Config::default(),
            "Wingman.Settings.Test.NoSuchClass.91b4",
            false,
        );
        assert!(!opened);
        assert_eq!(INNERS_DROPPED.with(|c| c.get()) - before, 1);
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
            let escape = take_posted(hwnd, WM_COMMAND).expect("Esc posts its command");
            assert_eq!(escape.wParam.0 & 0xFFFF, ID_ESCAPE as usize);

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

            // Delivering the command asks the window to close (nothing is typed
            // in a box), and delivering that closes it, with no loop of ours.
            unsafe { DispatchMessageW(&escape) };
            let close = take_posted(hwnd, WM_CLOSE).expect("Esc asks the window to close");
            unsafe { DispatchMessageW(&close) };
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
        assert!(get_text(primary).starts_with("Copilot &key: "));
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
            format!("Copilot &key: {}", chord_to_string(&staged.primary))
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
        assert_eq!(pause_label(&hk.pause), "Pause s&hortcut: not set");
        let c = chord(b'P' as u32, true, false, true, false);
        hk.pause = Some(c);
        assert_eq!(
            pause_label(&hk.pause),
            format!("Pause s&hortcut: {}", chord_to_string(&c))
        );
    }

    // ---- #1105: no registry or file work on the window's thread ----

    thread_local! {
        static FAKE_ON: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static FAKE_SET_FAILS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
        static FAKE_SETS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }
    fn fake_is_enabled() -> bool {
        FAKE_ON.with(|c| c.get())
    }
    fn fake_set_enabled(on: bool) -> anyhow::Result<()> {
        FAKE_SETS.with(|c| c.set(c.get() + 1));
        if FAKE_SET_FAILS.with(|c| c.get()) {
            anyhow::bail!("denied");
        }
        FAKE_ON.with(|c| c.set(on));
        Ok(())
    }
    fn fake_seams(on: bool, fails: bool) -> AutostartSeams {
        FAKE_ON.with(|c| c.set(on));
        FAKE_SET_FAILS.with(|c| c.set(fails));
        FAKE_SETS.with(|c| c.set(0));
        AutostartSeams {
            is_enabled: fake_is_enabled,
            set_enabled: fake_set_enabled,
        }
    }

    fn ans(was: bool, now: bool, failed: bool, changed: bool) -> AutostartAnswer {
        AutostartAnswer {
            was,
            now,
            failed,
            changed,
        }
    }

    #[test]
    fn the_autostart_worker_reads_only_when_asked_to_read() {
        let a = run_autostart(AutostartJob::Read, fake_seams(true, false));
        assert!(a.now && !a.failed && !a.changed);
        assert_eq!(FAKE_SETS.with(|c| c.get()), 0, "a read wrote");
    }

    #[test]
    fn the_autostart_worker_skips_a_write_that_changes_nothing() {
        let a = run_autostart(AutostartJob::Set(true), fake_seams(true, false));
        assert!(a.now && !a.failed && !a.changed);
        assert_eq!(FAKE_SETS.with(|c| c.get()), 0);
    }

    #[test]
    fn the_autostart_worker_reports_a_change_and_the_new_state() {
        let a = run_autostart(AutostartJob::Set(true), fake_seams(false, false));
        assert!(!a.was && a.now && a.changed && !a.failed);
        let a = run_autostart(AutostartJob::Undo(false), fake_seams(true, false));
        assert!(a.was && !a.now && a.changed && !a.failed);
    }

    #[test]
    fn a_refused_autostart_write_reports_the_real_state_for_the_checkbox() {
        let a = run_autostart(AutostartJob::Set(true), fake_seams(false, true));
        assert!(a.failed && !a.changed);
        assert!(!a.now, "the box must go back to what the registry holds");
    }

    #[test]
    fn the_autostart_answer_decides_the_status_line() {
        let ok = ans(false, true, false, true);
        let bad = ans(false, false, true, false);
        let same = ans(true, true, false, false);
        let set = AutostartJob::Set(true);
        assert_eq!(
            autostart_verdict(set, &ok),
            AutostartVerdict::Saved { was: false }
        );
        assert_eq!(
            autostart_verdict(AutostartJob::Undo(false), &ok),
            AutostartVerdict::Undone
        );
        assert_eq!(autostart_verdict(set, &bad), AutostartVerdict::Failed);
        assert_eq!(
            autostart_verdict(AutostartJob::Undo(true), &bad),
            AutostartVerdict::Failed
        );
        assert_eq!(autostart_verdict(set, &same), AutostartVerdict::Quiet);
        assert_eq!(
            autostart_verdict(AutostartJob::Read, &ok),
            AutostartVerdict::Quiet
        );
        assert_eq!(
            autostart_verdict(AutostartJob::Read, &bad),
            AutostartVerdict::Quiet
        );
    }

    #[test]
    fn only_a_click_is_queued_behind_a_running_autostart_job() {
        assert!(queued_when_busy(AutostartJob::Set(true)));
        assert!(
            !queued_when_busy(AutostartJob::Read),
            "a read would commit the placeholder"
        );
        assert!(!queued_when_busy(AutostartJob::Undo(true)));
    }

    #[test]
    fn a_coalesced_read_does_not_ask_for_a_second_job() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &mut *raw };
            inner.autostart_busy = true;
            start_autostart(inner, AutostartJob::Read);
            assert!(!inner.autostart_again);
            start_autostart(inner, AutostartJob::Set(true));
            assert!(inner.autostart_again);
        });
    }

    #[test]
    fn the_open_time_read_replaces_the_baseline_only_when_it_applies() {
        with_window(Config::default(), |_hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let mut file = Config::default();
            file.ui.card_seconds = 77;
            let mtime = Some(std::time::SystemTime::UNIX_EPOCH);
            *inner.open_slot.lock().unwrap() = Some(Opened {
                baseline: Some(file.clone()),
                mtime,
            });
            inner.wrote = true;
            take_open_read(inner);
            assert_ne!(inner.baseline.ui.card_seconds, 77, "a write came first");
            assert!(inner.file_mtime.is_none());
            inner.wrote = false;
            *inner.open_slot.lock().unwrap() = Some(Opened {
                baseline: Some(file),
                mtime,
            });
            take_open_read(inner);
            assert_eq!(inner.baseline.ui.card_seconds, 77);
            assert_eq!(inner.file_mtime, mtime);
        });
    }

    #[test]
    fn the_open_time_read_is_taken_only_while_it_is_still_true() {
        assert!(open_read_applies(false, 0, false, false));
        assert!(
            !open_read_applies(true, 0, false, false),
            "a write has moved the baseline"
        );
        assert!(!open_read_applies(false, 1, false, false));
        assert!(
            !open_read_applies(false, 0, true, false),
            "a refresh already read the file"
        );
        assert!(!open_read_applies(false, 0, false, true), "closing");
    }

    #[test]
    fn a_finished_autostart_job_updates_the_box_status_and_undo() {
        with_window(Config::default(), |hwnd, raw| {
            let inner = unsafe { &mut *raw };
            let box_ = get_dlg_item(hwnd, ID_AUTOSTART).unwrap();
            set_checkbox(box_, true);
            inner.autostart_busy = true;
            *inner.autostart_slot.lock().unwrap() = Some(AutostartDone {
                job: AutostartJob::Set(true),
                answer: ans(false, false, true, false),
            });
            take_autostart(inner);
            assert!(
                !checkbox_checked(hwnd, ID_AUTOSTART),
                "a failure reverts the box"
            );
            assert!(!inner.autostart_busy);
            assert!(status_is(
                hwnd,
                &commit::failed_line(Field::Autostart.label(), AUTOSTART_FAILED)
            ));
            inner.autostart_busy = true;
            *inner.autostart_slot.lock().unwrap() = Some(AutostartDone {
                job: AutostartJob::Set(true),
                answer: ans(false, true, false, true),
            });
            take_autostart(inner);
            assert!(checkbox_checked(hwnd, ID_AUTOSTART));
            assert!(inner.tracker.undo.is_some(), "a saved change offers Undo");
        });
    }

    /// Only a worker, or a seam a worker calls, touches the registry or the file.
    #[test]
    fn nothing_on_the_ui_thread_touches_the_registry_or_the_file() {
        let src = include_str!("settings.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let lines: Vec<&str> = code.lines().collect();
        let allowed = [
            "fn start_refresh(",
            "fn read_file_config(",
            "fn start_open_read(",
            "fn start_autostart(",
            "const REAL: AutostartSeams",
        ];
        let mut found = 0;
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            for banned in [
                "crate::autostart::",
                "file_baseline(",
                "file_mtime(",
                "read_to_string(",
                "std::fs::",
            ] {
                if !line.contains(banned) {
                    continue;
                }
                found += 1;
                let owner = lines[..=i]
                    .iter()
                    .rev()
                    .find(|l| {
                        let t = l.trim_start();
                        t.starts_with("fn ")
                            || t.starts_with("pub fn ")
                            || t.starts_with("unsafe fn ")
                            || t.starts_with("const REAL")
                    })
                    .copied()
                    .unwrap_or("");
                assert!(
                    allowed.iter().any(|a| owner.contains(a)),
                    "`{banned}` on line {} is in `{}`, which runs on the window's thread",
                    i + 1,
                    owner.trim()
                );
            }
        }
        assert!(
            found >= 4,
            "the scan found only {found} candidates: it checks nothing"
        );
        for arm in ["WM_SETTINGS_AUTOSTART =>", "WM_SETTINGS_OPENED =>"] {
            assert!(code.contains(arm), "no `{arm}` arm");
        }
    }
}
