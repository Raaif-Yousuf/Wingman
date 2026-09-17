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
//!
//! # `TaskbarCreated` (icon survives an Explorer restart)
//!
//! `NIM_ADD` only adds the icon to the taskbar's *current* notification
//! area; if Explorer crashes or is restarted, that area is destroyed along
//! with every process's icon in it, and nothing re-adds ours automatically.
//! The documented recovery is the shell's `TaskbarCreated` message,
//! broadcast to every top-level window once a new taskbar exists. The
//! integrating agent must call [`register_taskbar_created`] once at
//! startup and route the id it returns, in the main window proc, to
//! [`Tray::readd`] -- see `app.rs`'s `wnd_proc` for the wiring. Unlike
//! [`WM_APP_TRAY`] this id is not a compile-time constant (it comes from
//! `RegisterWindowMessageW` at runtime), so it cannot be matched as an
//! ordinary `match` arm; a guard (`id if id == app.taskbar_created_msg`)
//! is required.

use anyhow::{anyhow, Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, DestroyMenu, GetCursorPos, LoadIconW, RegisterWindowMessageW,
    SetForegroundWindow, SetMenuItemInfoW, TrackPopupMenu, HICON, HMENU, IDI_APPLICATION,
    MENUITEMINFOW, MFS_CHECKED, MFT_RADIOCHECK, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING,
    MIIM_FTYPE, MIIM_STATE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU,
    WM_LBUTTONUP, WM_RBUTTONUP,
};

/// Posted by the shell to this app's window proc on tray icon activity.
pub const WM_APP_TRAY: u32 = WM_APP + 1;

/// Registers the shell's `TaskbarCreated` message and returns its (runtime,
/// not compile-time) id. Call once at startup; see the module docs' section
/// on `TaskbarCreated` for how to route the result.
pub fn register_taskbar_created() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

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
    /// Open the settings window. This is also what a left-click on the tray
    /// icon does -- deliberately NOT `ASK_NOW`, because the icon is easy to
    /// hit by accident and firing a paid API call on a stray click is worse
    /// than opening a window.
    pub const OPEN_SETTINGS: u32 = 1008;
    /// Make ChatGPT the active provider (moves it to the front of the order).
    pub const USE_OPENAI: u32 = 1009;
    /// Make Claude the active provider.
    pub const USE_ANTHROPIC: u32 = 1010;

    /// Base id for the OpenAI model submenu. The chosen model is
    /// `OPENAI_MODEL_BASE + index` into the slice passed to `set_models`.
    pub const OPENAI_MODEL_BASE: u32 = 2000;
    /// Base id for the Anthropic model submenu, same scheme.
    pub const ANTHROPIC_MODEL_BASE: u32 = 2100;
    /// Exclusive upper bound for each range — keep the lists under this many
    /// entries, and clamp if a longer list is ever passed in.
    pub const MODEL_RANGE: u32 = 100;
}

/// Decoded meaning of a command id returned by [`Tray::on_tray_message`].
///
/// `on_tray_message`'s own signature and return type (`Option<u32>`) are
/// unchanged -- this is purely a convenience for the caller. Call
/// [`decode`] on the id from a `Some(id)` result to find out whether it was
/// one of the fixed [`cmd`] items or a pick from one of the two model
/// submenus (an index into the slice most recently passed to
/// [`Tray::set_models`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuChoice {
    /// One of the fixed `cmd::*` items (or, in principle, an id this module
    /// doesn't otherwise recognize -- callers should treat unrecognized
    /// values as a no-op rather than panicking).
    Command(u32),
    /// Index into the `openai` slice last passed to `set_models`.
    OpenAiModel(usize),
    /// Index into the `anthropic` slice last passed to `set_models`.
    AnthropicModel(usize),
}

/// Decode a command id returned by [`Tray::on_tray_message`] into a
/// [`MenuChoice`]. See that enum's docs for how callers should use this.
pub fn decode(id: u32) -> MenuChoice {
    if (cmd::OPENAI_MODEL_BASE..cmd::OPENAI_MODEL_BASE + cmd::MODEL_RANGE).contains(&id) {
        MenuChoice::OpenAiModel((id - cmd::OPENAI_MODEL_BASE) as usize)
    } else if (cmd::ANTHROPIC_MODEL_BASE..cmd::ANTHROPIC_MODEL_BASE + cmd::MODEL_RANGE)
        .contains(&id)
    {
        MenuChoice::AnthropicModel((id - cmd::ANTHROPIC_MODEL_BASE) as usize)
    } else {
        MenuChoice::Command(id)
    }
}

pub struct Tray {
    hwnd: HWND,
    /// Kept so [`Tray::readd`] can reload the icon exactly as [`Tray::new`]
    /// did, without the caller having to thread it through again.
    instance: HINSTANCE,
    /// Display strings for the current primary/secondary bindings (e.g.
    /// `"Win+Shift+F23"`), shown in the "Set ... key" menu labels. Set via
    /// [`Tray::set_key_bindings`]; empty until then, in which case the
    /// parenthetical suffix is simply omitted.
    primary_label: String,
    secondary_label: String,
    /// Model lists and current-selection index for the two submenus. Set
    /// via [`Tray::set_models`]; empty until then, in which case the
    /// submenu shows a single greyed-out "none configured" entry.
    /// Which provider is first in `providers.order`; radio-checked in the
    /// Provider submenu.
    active_provider_openai: bool,
    openai_models: Vec<String>,
    openai_current: Option<usize>,
    anthropic_models: Vec<String>,
    anthropic_current: Option<usize>,
}

impl Tray {
    /// Adds the tray icon (`NIM_ADD`) and switches it to
    /// `NOTIFYICON_VERSION_4` (`NIM_SETVERSION`) -- see the module docs for
    /// why. `instance` is used to try loading an embedded icon resource
    /// first; `IDI_APPLICATION` is the real fallback (see
    /// [`EMBEDDED_ICON_ID`]).
    pub fn new(hwnd: HWND, instance: HINSTANCE) -> Result<Self> {
        add_icon(hwnd, instance)?;

        Ok(Self {
            hwnd,
            instance,
            primary_label: String::new(),
            secondary_label: String::new(),
            active_provider_openai: true,
            openai_models: Vec::new(),
            openai_current: None,
            anthropic_models: Vec::new(),
            anthropic_current: None,
        })
    }

    /// Re-add the icon after the shell's `TaskbarCreated` broadcast (see the
    /// module docs). Runs the exact same `NIM_ADD` + `NIM_SETVERSION` calls
    /// as [`Tray::new`]; the cached labels/models on `self` are untouched
    /// (they were never shell-side state), but the tooltip was, so the
    /// caller should re-issue [`Tray::set_tooltip`] afterwards (`app.rs`
    /// does this by calling its existing `refresh_tray_labels`).
    pub fn readd(&self) -> Result<()> {
        add_icon(self.hwnd, self.instance)
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

    /// Populates the two model submenus. `*_current` marks which entry gets
    /// the radio check; a current model that is not in its list is still
    /// shown, appended at the end, so a hand-edited config.toml value is
    /// never hidden or silently switched. Lists longer than
    /// [`cmd::MODEL_RANGE`] entries are clamped (dropping from the end,
    /// keeping room to append `*_current` if it would otherwise be
    /// dropped). Does not touch the shell icon; only affects the next menu
    /// built by [`Tray::on_tray_message`].
    /// Marks which provider is active. `true` = ChatGPT, `false` = Claude.
    pub fn set_active_provider(&mut self, openai: bool) {
        self.active_provider_openai = openai;
    }

    pub fn set_models(
        &mut self,
        openai: &[String],
        openai_current: &str,
        anthropic: &[String],
        anthropic_current: &str,
    ) {
        let (models, current) = clamp_model_list(openai, openai_current);
        self.openai_models = models;
        self.openai_current = current;

        let (models, current) = clamp_model_list(anthropic, anthropic_current);
        self.anthropic_models = models;
        self.anthropic_current = current;
    }

    /// Handle the `WM_APP_TRAY` message. Returns `Some(cmd::OPEN_SETTINGS)` on
    /// left-click/Enter activation, or the chosen menu command id on
    /// right-click/context-menu activation (`None` if the menu was
    /// dismissed without a choice, or the event was something else this
    /// tray ignores).
    /// Resolve a [`MenuChoice::OpenAiModel`] index back to a model id.
    ///
    /// The index addresses this struct's own clamped list, which can differ
    /// from what was passed to [`Tray::set_models`] (a current model missing
    /// from the list gets appended), so callers must resolve through here
    /// rather than re-indexing their own copy.
    pub fn openai_model_at(&self, index: usize) -> Option<&str> {
        self.openai_models.get(index).map(String::as_str)
    }

    /// Resolve a [`MenuChoice::AnthropicModel`] index back to a model id.
    /// See [`Tray::openai_model_at`].
    pub fn anthropic_model_at(&self, index: usize) -> Option<&str> {
        self.anthropic_models.get(index).map(String::as_str)
    }

    pub fn on_tray_message(&mut self, lparam: LPARAM) -> Option<u32> {
        let event = (lparam.0 as u32) & 0xFFFF;
        match event {
            e if e == WM_LBUTTONUP || e == NIN_SELECT => Some(cmd::OPEN_SETTINGS),
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
        append_provider_submenu(hmenu, self.active_provider_openai)?;
        append_model_submenu(
            hmenu,
            "ChatGPT model",
            cmd::OPENAI_MODEL_BASE,
            &self.openai_models,
            self.openai_current,
        )?;
        append_model_submenu(
            hmenu,
            "Claude model",
            cmd::ANTHROPIC_MODEL_BASE,
            &self.anthropic_models,
            self.anthropic_current,
        )?;
        append_separator(hmenu)?;
        append_item(hmenu, cmd::SET_PRIMARY, &key_label("Set Copilot key", &self.primary_label))?;
        append_item(
            hmenu,
            cmd::SET_SECONDARY,
            &key_label("Set secondary key", &self.secondary_label),
        )?;
        append_item(hmenu, cmd::OPEN_SETTINGS, "Settings...")?;
        append_item(hmenu, cmd::EDIT_SETTINGS, "Open config.toml")?;
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

/// Attach `submenu` to `parent` as a `MF_POPUP` item labeled `text`. Per
/// `AppendMenuW`'s docs, when `MF_POPUP` is set the `uIDNewItem` parameter
/// (here typed `usize`) is actually the submenu's `HMENU` reinterpreted, not
/// a command id -- that's the "fiddly" conversion: `HMENU` wraps a raw
/// pointer (`*mut c_void`), so `submenu.0 as usize` is the correct (and
/// only) way to pass it through the `usize` slot.
///
/// Ownership: once attached this way, `submenu` becomes a child of `parent`
/// and is destroyed along with it by `DestroyMenu(parent)` -- callers must
/// NOT call `DestroyMenu` on `submenu` themselves (double-free/UAF).
fn append_submenu(parent: HMENU, submenu: HMENU, text: &str) -> Result<()> {
    let wide = to_wide(text);
    unsafe {
        AppendMenuW(
            parent,
            MF_POPUP | MF_STRING,
            submenu.0 as usize,
            PCWSTR::from_raw(wide.as_ptr()),
        )
    }
    .context("AppendMenuW(popup) failed")
}

/// Mark the item identified by command id `id` in `hmenu` as the checked
/// radio entry (`MFT_RADIOCHECK` + `MFS_CHECKED`), so it renders as a radio
/// dot rather than a tick. Best-effort: a failure here (e.g. the id
/// somehow isn't present) just leaves the item unmarked rather than
/// panicking or aborting menu construction.
fn mark_radio_checked(hmenu: HMENU, id: u32) {
    let info = MENUITEMINFOW {
        cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
        fMask: MIIM_STATE | MIIM_FTYPE,
        fType: MFT_RADIOCHECK,
        fState: MFS_CHECKED,
        ..Default::default()
    };
    let _ = unsafe { SetMenuItemInfoW(hmenu, id, false, &info) };
}

/// Attach `submenu` to `parent` (see [`append_submenu`]). If the attach
/// itself fails, `submenu` was never reachable from `parent`, so nothing
/// else will ever free it -- destroy it here before propagating the error.
fn attach_submenu_or_destroy(parent: HMENU, submenu: HMENU, text: &str) -> Result<()> {
    if let Err(e) = append_submenu(parent, submenu, text) {
        let _ = unsafe { DestroyMenu(submenu) };
        return Err(e);
    }
    Ok(())
}

/// Build the "Provider" submenu: which service actually answers. This is the
/// `providers.order` front-runner, distinct from the per-provider model
/// submenus below -- picking Claude here does not change which ChatGPT model
/// is configured, it changes who gets asked first.
///
/// Like [`append_model_submenu`], `sub` is attached to `parent` before it is
/// populated (via [`attach_submenu_or_destroy`]), not after: once attached,
/// a population failure is still cleaned up by the caller's eventual
/// `DestroyMenu(parent)`, and a failed attach is cleaned up immediately by
/// `attach_submenu_or_destroy` itself. Previously this populated first and
/// attached last, so a failure in the final `append_submenu` call leaked
/// `sub` -- see #147.
fn append_provider_submenu(parent: HMENU, openai_active: bool) -> Result<()> {
    let sub = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, sub, "Provider")?;
    append_item(sub, cmd::USE_OPENAI, "ChatGPT")?;
    append_item(sub, cmd::USE_ANTHROPIC, "Claude")?;
    mark_radio_checked(
        sub,
        if openai_active {
            cmd::USE_OPENAI
        } else {
            cmd::USE_ANTHROPIC
        },
    );
    Ok(())
}

/// Build one model submenu (`CreatePopupMenu`, populate, attach to
/// `parent`) and radio-check `current` if present. If `models` is empty, a
/// single greyed-out "none configured" entry is shown instead of an empty
/// submenu.
///
/// The submenu is attached to `parent` (via [`attach_submenu_or_destroy`])
/// before it is populated, not after, specifically so that if population
/// fails partway through, the already-attached submenu is still reachable
/// from `parent` and gets cleaned up by the caller's eventual
/// `DestroyMenu(parent)` -- avoiding a leaked, never-attached `HMENU` on the
/// error path. A failure in the attach itself is cleaned up immediately by
/// `attach_submenu_or_destroy`.
fn append_model_submenu(
    parent: HMENU,
    label: &str,
    base_id: u32,
    models: &[String],
    current: Option<usize>,
) -> Result<()> {
    let submenu = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, submenu, label)?;

    if models.is_empty() {
        let wide = to_wide("(none configured)");
        unsafe {
            AppendMenuW(
                submenu,
                MF_STRING | MF_GRAYED,
                base_id as usize,
                PCWSTR::from_raw(wide.as_ptr()),
            )
        }
        .context("AppendMenuW(none configured) failed")?;
        return Ok(());
    }

    // Clamped by clamp_model_list to at most MODEL_RANGE entries, so every
    // `base_id + i` stays inside this submenu's id range and never collides
    // with the other submenu or the fixed cmd::* ids.
    for (i, model) in models.iter().enumerate() {
        let id = base_id + i as u32;
        append_item(submenu, id, model)?;
        if current == Some(i) {
            mark_radio_checked(submenu, id);
        }
    }

    Ok(())
}

/// Clamp `list` to at most `cmd::MODEL_RANGE` entries and make sure
/// `current` is represented: if it's already in the (possibly truncated)
/// list, return its index; otherwise append it (evicting the last entry
/// first if the list is already at the cap), unless `current` is empty.
fn clamp_model_list(list: &[String], current: &str) -> (Vec<String>, Option<usize>) {
    let range = cmd::MODEL_RANGE as usize;
    let mut models: Vec<String> = list.iter().take(range).cloned().collect();

    if let Some(idx) = models.iter().position(|m| m == current) {
        return (models, Some(idx));
    }
    if current.is_empty() {
        return (models, None);
    }

    if models.len() >= range {
        models.pop();
    }
    models.push(current.to_string());
    let idx = models.len() - 1;
    (models, Some(idx))
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

/// `NIM_ADD` the icon and switch it to `NOTIFYICON_VERSION_4`
/// (`NIM_SETVERSION`) -- see the module docs for why. Shared by
/// [`Tray::new`] and [`Tray::readd`] so the two can never drift apart.
/// `instance` is used to try loading an embedded icon resource first;
/// `IDI_APPLICATION` is the real fallback (see [`EMBEDDED_ICON_ID`]).
fn add_icon(hwnd: HWND, instance: HINSTANCE) -> Result<()> {
    let icon = load_icon(instance);

    // Best-effort: NIM_ADD fails outright if this (hWnd, uID) pair is
    // already registered with the shell -- e.g. Tray::readd running while
    // the previous registration is still live, which is exactly what
    // happens in a test with no real Explorer restart in between. After a
    // genuine Explorer crash this NIM_DELETE itself fails harmlessly (the
    // shell's own icon table died with the old Explorer process), so it is
    // safe to attempt unconditionally in both cases.
    let del = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = unsafe { Shell_NotifyIconW(NIM_DELETE, &del) };

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

    // Best-effort: if the shell won't upgrade us to v4 we still work, just
    // with legacy (non-semantic) mouse messages.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };

    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    // -- TaskbarCreated recovery (#145) -------------------------------------

    #[test]
    fn register_taskbar_created_returns_a_nonzero_id() {
        // RegisterWindowMessageW returns 0 on failure; a real id is always
        // in the 0xC000-0xFFFF range, but the only contract this module
        // relies on is "nonzero and stable for the process".
        assert_ne!(register_taskbar_created(), 0);
    }

    #[test]
    fn tray_new_then_readd_both_succeed_against_a_real_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, CW_USEDEFAULT, WINDOW_EX_STYLE, WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);

        // "STATIC" is a predefined system window class -- no RegisterClassExW
        // needed, unlike settings.rs's own smoke test.
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("copilot-ask tray test"),
                WS_OVERLAPPED,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                0,
                0,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("CreateWindowExW");

        let tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");
        // The exact scenario #145 is about: re-adding after the icon was
        // dropped (here simulated by just calling it again on the same,
        // already-added icon id -- NIM_ADD is idempotent for that case).
        tray.readd().expect("Tray::readd should re-add the icon");

        drop(tray); // NIM_DELETE via Drop
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- submenu attach ordering (#147) -------------------------------------

    #[test]
    fn attach_submenu_or_destroy_frees_the_submenu_when_attach_fails() {
        use windows::Win32::UI::WindowsAndMessaging::IsMenu;

        let sub = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        // A null HMENU never identifies a menu, so AppendMenuW's attach call
        // inside `append_submenu` fails deterministically here -- no need to
        // exhaust real USER objects to hit the same error path.
        let invalid_parent = HMENU(std::ptr::null_mut());

        let result = attach_submenu_or_destroy(invalid_parent, sub, "Provider");

        assert!(result.is_err(), "attaching to a null HMENU should fail");
        assert!(
            !unsafe { IsMenu(sub) }.as_bool(),
            "attach_submenu_or_destroy leaked `sub`: it is still a valid menu \
             handle after the attach it was meant to guard failed"
        );
    }

    #[test]
    fn attach_submenu_or_destroy_attaches_on_success() {
        use windows::Win32::UI::WindowsAndMessaging::GetMenuItemCount;

        let parent = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        let sub = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");

        let result = attach_submenu_or_destroy(parent, sub, "Provider");

        assert!(result.is_ok());
        assert_eq!(unsafe { GetMenuItemCount(Some(parent)) }, 1);
        let _ = unsafe { DestroyMenu(parent) }; // also frees `sub`, now a child
    }

    // -- command ids -------------------------------------------------------
    // Companion to settings.rs's `control_ids_are_pairwise_unique` (#144):
    // this module's ids live in a completely separate WM_COMMAND namespace
    // (the tray context menu, not the settings dialog's children), but the
    // same failure shape -- two constants sharing a value so one handler
    // silently steals the other's clicks -- applies here too.

    /// Every fixed `cmd::*` command id, paired with its constant name. The
    /// two model submenus use dynamic ranges instead (`base_id + index`) and
    /// are checked separately below, since they aren't single ids.
    const ALL_FIXED_CMD_IDS: &[(&str, u32)] = &[
        ("ASK_NOW", cmd::ASK_NOW),
        ("COPY_LAST", cmd::COPY_LAST),
        ("SET_PRIMARY", cmd::SET_PRIMARY),
        ("SET_SECONDARY", cmd::SET_SECONDARY),
        ("EDIT_SETTINGS", cmd::EDIT_SETTINGS),
        ("RELOAD", cmd::RELOAD),
        ("QUIT", cmd::QUIT),
        ("OPEN_SETTINGS", cmd::OPEN_SETTINGS),
        ("USE_OPENAI", cmd::USE_OPENAI),
        ("USE_ANTHROPIC", cmd::USE_ANTHROPIC),
    ];

    #[test]
    fn fixed_cmd_ids_are_pairwise_unique() {
        for (i, (name_a, id_a)) in ALL_FIXED_CMD_IDS.iter().enumerate() {
            for (name_b, id_b) in ALL_FIXED_CMD_IDS.iter().skip(i + 1) {
                assert_ne!(id_a, id_b, "{name_a} and {name_b} share command id {id_a}");
            }
        }
    }

    #[test]
    fn model_submenu_ranges_do_not_overlap_each_other_or_the_fixed_ids() {
        let openai_range = cmd::OPENAI_MODEL_BASE..cmd::OPENAI_MODEL_BASE + cmd::MODEL_RANGE;
        let anthropic_range =
            cmd::ANTHROPIC_MODEL_BASE..cmd::ANTHROPIC_MODEL_BASE + cmd::MODEL_RANGE;

        assert!(
            openai_range.end <= anthropic_range.start
                || anthropic_range.end <= openai_range.start,
            "OpenAI model range {openai_range:?} overlaps Anthropic model range {anthropic_range:?}"
        );

        for (name, id) in ALL_FIXED_CMD_IDS {
            assert!(
                !openai_range.contains(id) && !anthropic_range.contains(id),
                "{name} ({id}) falls inside a model submenu's dynamic id range"
            );
        }
    }
}
