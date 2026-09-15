//! `Shell_NotifyIconW` tray icon and right-click context menu.
//!
//! See `docs/superpowers/specs/2026-09-14-copilot-ask-design.md`, section
//! "UI: tray".
//!
//! # `NOTIFYICON_VERSION_4` and the tray-message convention
//!
//! [`Tray::new`] switches the icon to `NOTIFYICON_VERSION_4` via
//! `NIM_SETVERSION` immediately after `NIM_ADD`. Under that version the
//! shell sends semantic notifications (`NIN_SELECT` for activation via mouse
//! *or* keyboard, `WM_CONTEXTMENU` for the menu gesture via mouse *or*
//! keyboard) rather than only raw mouse messages, which is the documented,
//! more robust way to handle the callback.
//!
//! For that version, Windows packs the *cursor position* into `wParam`
//! (`GET_X_LPARAM`/`GET_Y_LPARAM`-style, low/high word) and the
//! *notification event* plus icon id into `lParam` (event in the low word,
//! icon id in the high word). [`Tray::on_tray_message`]'s signature takes
//! only the `lParam` of the `WM_APP_TRAY` message (matching this module's
//! fixed public API), so this implementation reads the event from
//! `LOWORD(lparam)` and, when it needs a screen point to anchor the popup
//! menu, calls `GetCursorPos` directly instead of decoding `wParam`. This is
//! simpler than threading `wParam` through as well and is exactly as
//! accurate, since `GetCursorPos` at callback time reflects the same click
//! that generated the notification.
//!
//! This module owns [`WM_APP_TRAY`], the value passed to the shell as
//! `uCallbackMessage`; the integrating agent should route `WM_APP_TRAY` in
//! the main window proc to [`Tray::on_tray_message`].

use anyhow::{anyhow, Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, SetForegroundWindow,
    TrackPopupMenu, HICON, HMENU, IDI_APPLICATION, MF_SEPARATOR, MF_STRING, TPM_RETURNCMD,
    TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP,
};

/// Posted by the shell to this app's window proc on tray icon activity.
pub const WM_APP_TRAY: u32 = WM_APP + 1;

/// Identifies our one tray icon in every `NOTIFYICONDATAW` call.
const TRAY_ICON_ID: u32 = 1;

/// Resource id an embedded icon would be loaded from, if this build actually
/// embedded one. There is no `.rc` build step in this project, so
/// `LoadIconW` with this id always fails at runtime and [`load_icon`] falls
/// through to `IDI_APPLICATION` -- that fallback is the code path that
/// actually runs and must work.
const EMBEDDED_ICON_ID: u16 = 1;

pub mod cmd {
    pub const ASK_NOW: u32 = 1001;
    pub const COPY_LAST: u32 = 1002;
    pub const SET_PRIMARY: u32 = 1003;
    pub const SET_SECONDARY: u32 = 1004;
    pub const EDIT_SETTINGS: u32 = 1005;
    pub const RELOAD: u32 = 1006;
    pub const QUIT: u32 = 1007;
}

pub struct Tray {
    hwnd: HWND,
    /// Display strings for the current primary/secondary bindings (e.g.
    /// `"Win+Shift+F23"`), shown in the "Set ... key" menu labels. Set via
    /// [`Tray::set_key_bindings`]; empty until then, in which case the
    /// parenthetical suffix is simply omitted.
    primary_label: String,
    secondary_label: String,
}

impl Tray {
    /// Adds the tray icon (`NIM_ADD`) and switches it to
    /// `NOTIFYICON_VERSION_4` (`NIM_SETVERSION`) -- see the module docs for
    /// why. `instance` is used to try loading an embedded icon resource
    /// first; `IDI_APPLICATION` is the real fallback (see
    /// [`EMBEDDED_ICON_ID`]).
    pub fn new(hwnd: HWND, instance: HINSTANCE) -> Result<Self> {
        let icon = load_icon(instance);

        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP | NIF_SHOWTIP,
            uCallbackMessage: WM_APP_TRAY,
            hIcon: icon,
            ..Default::default()
        };
        set_sz_tip(&mut nid.szTip, "copilot-ask");

        let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
        if !added.as_bool() {
            return Err(anyhow!("Shell_NotifyIconW(NIM_ADD) failed"));
        }

        // Best-effort: if the shell won't upgrade us to v4 we still work,
        // just with legacy (non-semantic) mouse messages.
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };

        Ok(Self {
            hwnd,
            primary_label: String::new(),
            secondary_label: String::new(),
        })
    }

    /// Update the tooltip shown when hovering the icon (the spec: "shows the
    /// active provider"). Truncates safely if `text` doesn't fit `szTip`.
    pub fn set_tooltip(&mut self, text: &str) {
        let mut nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ICON_ID,
            uFlags: NIF_TIP | NIF_SHOWTIP,
            ..Default::default()
        };
        set_sz_tip(&mut nid.szTip, text);
        let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
    }

    /// Set the display strings shown in the "Set Copilot key..." / "Set
    /// secondary key..." menu labels, e.g. `chord_to_string(&primary)`.
    /// Does not touch the shell icon; only affects the next menu built by
    /// [`Tray::on_tray_message`].
    pub fn set_key_bindings(&mut self, primary: &str, secondary: &str) {
        self.primary_label = primary.to_string();
        self.secondary_label = secondary.to_string();
    }

    /// Handle the `WM_APP_TRAY` message. Returns `Some(cmd::ASK_NOW)` on
    /// left-click/Enter activation, or the chosen menu command id on
    /// right-click/context-menu activation (`None` if the menu was
    /// dismissed without a choice, or the event was something else this
    /// tray ignores).
    pub fn on_tray_message(&mut self, lparam: LPARAM) -> Option<u32> {
        let event = (lparam.0 as u32) & 0xFFFF;
        match event {
            e if e == WM_LBUTTONUP || e == NIN_SELECT => Some(cmd::ASK_NOW),
            e if e == WM_CONTEXTMENU || e == WM_RBUTTONUP => self.show_menu(),
            _ => None,
        }
    }

    fn show_menu(&self) -> Option<u32> {
        let mut pt = POINT::default();
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            return None;
        }

        let hmenu = unsafe { CreatePopupMenu() }.ok()?;
        if self.build_menu(hmenu).is_err() {
            let _ = unsafe { DestroyMenu(hmenu) };
            return None;
        }

        // Documented Win32 workaround: without a foreground-window switch
        // right before TrackPopupMenu, the popup does not reliably dismiss
        // when the user clicks outside it (it can get "stuck").
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }

        let result = unsafe {
            TrackPopupMenu(
                hmenu,
                TPM_RETURNCMD | TPM_RIGHTBUTTON,
                pt.x,
                pt.y,
                None,
                self.hwnd,
                None,
            )
        };

        let _ = unsafe { DestroyMenu(hmenu) };

        if result.0 != 0 {
            Some(result.0 as u32)
        } else {
            None
        }
    }

    fn build_menu(&self, hmenu: HMENU) -> Result<()> {
        append_item(hmenu, cmd::ASK_NOW, "Ask now")?;
        append_item(hmenu, cmd::COPY_LAST, "Copy last answer")?;
        append_separator(hmenu)?;
        append_item(hmenu, cmd::SET_PRIMARY, &key_label("Set Copilot key", &self.primary_label))?;
        append_item(
            hmenu,
            cmd::SET_SECONDARY,
            &key_label("Set secondary key", &self.secondary_label),
        )?;
        append_item(hmenu, cmd::EDIT_SETTINGS, "Edit settings")?;
        append_item(hmenu, cmd::RELOAD, "Reload settings")?;
        append_separator(hmenu)?;
        append_item(hmenu, cmd::QUIT, "Quit")?;
        Ok(())
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        let nid = NOTIFYICONDATAW {
            cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
            hWnd: self.hwnd,
            uID: TRAY_ICON_ID,
            ..Default::default()
        };
        let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &nid) };
    }
}

fn key_label(base: &str, binding: &str) -> String {
    if binding.is_empty() {
        format!("{base}…")
    } else {
        format!("{base}…  ({binding})")
    }
}

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn append_item(hmenu: HMENU, id: u32, text: &str) -> Result<()> {
    let wide = to_wide(text);
    unsafe { AppendMenuW(hmenu, MF_STRING, id as usize, PCWSTR::from_raw(wide.as_ptr())) }
        .context("AppendMenuW failed")
}

fn append_separator(hmenu: HMENU) -> Result<()> {
    unsafe { AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null()) }
        .context("AppendMenuW(separator) failed")
}

/// Copy `text` into a fixed `szTip`-style buffer as null-terminated UTF-16,
/// truncating rather than overflowing if it doesn't fit.
fn set_sz_tip(dst: &mut [u16], text: &str) {
    if dst.is_empty() {
        return;
    }
    let wide: Vec<u16> = text.encode_utf16().collect();
    let n = wide.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&wide[..n]);
    for slot in &mut dst[n..] {
        *slot = 0;
    }
}

/// Try the embedded resource icon first (see [`EMBEDDED_ICON_ID`]); fall
/// back to the system's generic application icon, which always exists.
fn load_icon(instance: HINSTANCE) -> HICON {
    let embedded = PCWSTR(EMBEDDED_ICON_ID as usize as *const u16);
    if let Ok(icon) = unsafe { LoadIconW(Some(instance), embedded) } {
        if !icon.is_invalid() {
            return icon;
        }
    }
    unsafe { LoadIconW(None, IDI_APPLICATION) }.unwrap_or(HICON(std::ptr::null_mut()))
}
