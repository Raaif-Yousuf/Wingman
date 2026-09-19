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
use windows::core::{w, BOOL, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, POINT};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, GetDIBits, GetObjectW, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_SHOWTIP, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NIM_SETVERSION, NIN_SELECT, NOTIFYICONDATAW, NOTIFYICON_VERSION_4,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    GetIconInfo, LoadIconW, RegisterWindowMessageW, SetForegroundWindow, SetMenuItemInfoW,
    TrackPopupMenu, HICON, HMENU, ICONINFO, IDI_APPLICATION, MENUITEMINFOW, MFS_CHECKED,
    MFT_RADIOCHECK, MF_DISABLED, MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_FTYPE,
    MIIM_STATE, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP,
};

/// Posted by the shell to this app's window proc on tray icon activity.
/// Adding another `WM_APP_*` constant anywhere in the crate also means
/// adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue #163), which is
/// enforced by `wm_app_ids_registry_is_exhaustive`.
pub const WM_APP_TRAY: u32 = WM_APP + 1;

/// Registers the shell's `TaskbarCreated` message and returns its (runtime,
/// not compile-time) id. Call once at startup; see the module docs' section
/// on `TaskbarCreated` for how to route the result.
pub fn register_taskbar_created() -> u32 {
    unsafe { RegisterWindowMessageW(w!("TaskbarCreated")) }
}

/// Identifies our one tray icon in every `NOTIFYICONDATAW` call.
const TRAY_ICON_ID: u32 = 1;

/// Resource id of the icon `build.rs` embeds: it compiles `assets/app.rc`
/// (`1 ICON "icon.ico"`) via `embed_resource::compile` on a Windows target,
/// so `LoadIconW(instance, 1)` in [`load_icon`] loads that embedded
/// `assets/icon.ico` on a real Windows build. `IDI_APPLICATION` is still the
/// fallback [`load_icon`] uses if the load ever fails (e.g. a non-Windows
/// build, or a corrupt resource section), and that fallback path must keep
/// working, but it is not the path that normally runs.
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

    /// Pause (issue #20): 1 hour / until tomorrow / until resumed. Shown as
    /// a "Pause" submenu while running; see [`super::cmd::RESUME`] for the
    /// single item shown instead while already paused.
    pub const PAUSE_1H: u32 = 1011;
    pub const PAUSE_UNTIL_TOMORROW: u32 = 1012;
    pub const PAUSE_UNTIL_RESUMED: u32 = 1013;
    /// Shown in place of the "Pause" submenu while paused.
    pub const RESUME: u32 = 1014;

    /// Mode (issue #19): Cloud / Local / Auto / Offline, shown as a
    /// radio-checked "Mode" submenu (see [`super::append_mode_submenu`]).
    pub const MODE_CLOUD: u32 = 1015;
    pub const MODE_LOCAL: u32 = 1016;
    pub const MODE_AUTO: u32 = 1017;
    pub const MODE_OFFLINE: u32 = 1018;

    /// Issue #124: builds a plain-text diagnostics report and copies it to
    /// the clipboard. See `App::copy_diagnostics` in `app.rs`.
    pub const COPY_DIAGNOSTICS: u32 = 1019;

    /// Issue #106: copies the local egress log (every request this app has
    /// made: time, provider, model, what was attached, bytes, outcome) to
    /// the clipboard. There is no settings window to give it a page yet
    /// (#44/#51), so this is the same answer #124 gave diagnostics. See
    /// `App::copy_egress_log` in `app.rs` and `egress::read_all_human`.
    pub const COPY_EGRESS_LOG: u32 = 1028;

    /// Issue #41: "Copy text from screen" -- OCR the active monitor and copy
    /// the text to the clipboard, offline and model-free. See
    /// `App::extract_text` in `app.rs` and `actions::extract_text`. The
    /// palette (#25) does not exist yet, so this tray item is the only way
    /// to reach the action today.
    pub const EXTRACT_TEXT: u32 = 1020;
    /// Issue #115: evaluates the current selection as an arithmetic
    /// expression or a unit conversion, no model involved. See
    /// `App::calculate_selection` in `app.rs`. Stands in for a palette entry
    /// until #25's action palette exists.
    pub const CALCULATE_SELECTION: u32 = 1021;

    /// Issue #29: "Copy region to clipboard" -- opens the full-desktop
    /// region/window-selection overlay (`ui::region::select_region`) and
    /// copies the chosen crop to the clipboard as `CF_DIB`. See
    /// `App::copy_region` in `app.rs`.
    pub const COPY_REGION: u32 = 1022;

    /// Issue #39: runs the built-in "Add event from screen" action (Look,
    /// Propose, Confirm, Do). See `App::add_event_from_screen` in
    /// `app.rs`.
    pub const ADD_TO_CALENDAR: u32 = 1023;

    /// Issue #25: opens the Quick Ask palette. See `App::toggle_palette` in
    /// `app.rs`. Highest existing fixed cmd id was 1023 (`ADD_TO_CALENDAR`);
    /// this is appended after it per the uniqueness test's own convention
    /// (`fixed_cmd_ids_are_pairwise_unique`).
    pub const QUICK_ASK: u32 = 1024;
    /// Issue #38: runs the built-in "Review this email" action (Look,
    /// Propose, Confirm, Do). See `App::review_this_email` in `app.rs`. The
    /// palette (#25) does not exist on master yet, so -- same as
    /// `EXTRACT_TEXT` above -- this tray item is the only way to reach the
    /// action today; the palette's own action dispatch table should add
    /// `actions::review_email::ACTION_ID` to its built-ins once it lands.
    pub const REVIEW_EMAIL: u32 = 1025;
    /// Issue #40: runs the built-in "Fill this form" action (Look, Propose,
    /// Confirm, Do). See `App::fill_form_from_screen` in `app.rs`.
    pub const FILL_FORM: u32 = 1026;
    /// Issue #40: Undo for the most recent successful `fill_form` run. See
    /// `App::restore_last_form` in `app.rs`.
    pub const RESTORE_LAST_FORM: u32 = 1027;

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
    /// The icon loaded by [`add_icon`] (embedded resource or the
    /// `IDI_APPLICATION` fallback). `LoadIconW`'s result is a *shared*
    /// system handle -- unlike [`greyed_icon`](Self::greyed_icon), it is
    /// never `DestroyIcon`'d.
    base_icon: HICON,
    /// Lazily derived from `base_icon` the first time pausing needs it (see
    /// [`make_greyed_icon`]). Unlike `base_icon` this HICON is *owned*
    /// (built via `CreateIconIndirect`) and must be `DestroyIcon`'d, which
    /// happens in [`Tray::drop`] and whenever it is rebuilt.
    greyed_icon: Option<HICON>,
    /// Whether the tray is currently showing the paused icon/menu. Set via
    /// [`Tray::set_paused`].
    paused: bool,
    /// Lazily derived from `base_icon` the first time Offline mode needs it
    /// (see [`make_offline_icon`]). Same ownership rules as `greyed_icon`.
    offline_icon: Option<HICON>,
    /// Issue #19. Which "Mode" submenu item is radio-checked, and (via
    /// [`Tray::resolve_icon`]) whether the icon should show the Offline
    /// tint. Defaults to `Auto`, same as [`crate::mode::Mode::default`],
    /// until [`Tray::set_mode`] is called with the loaded config's value.
    mode: crate::mode::Mode,
}

impl Tray {
    /// Adds the tray icon (`NIM_ADD`) and switches it to
    /// `NOTIFYICON_VERSION_4` (`NIM_SETVERSION`) -- see the module docs for
    /// why. `instance` is used to try loading an embedded icon resource
    /// first; `IDI_APPLICATION` is the real fallback (see
    /// [`EMBEDDED_ICON_ID`]).
    pub fn new(hwnd: HWND, instance: HINSTANCE) -> Result<Self> {
        let base_icon = add_icon(hwnd, instance)?;

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
            base_icon,
            greyed_icon: None,
            paused: false,
            offline_icon: None,
            mode: crate::mode::Mode::default(),
        })
    }

    /// Re-add the icon after the shell's `TaskbarCreated` broadcast (see the
    /// module docs). Runs the exact same `NIM_ADD` + `NIM_SETVERSION` calls
    /// as [`Tray::new`]; the cached labels/models on `self` are untouched
    /// (they were never shell-side state), but the tooltip was, so the
    /// caller should re-issue [`Tray::set_tooltip`] afterwards (`app.rs`
    /// does this by calling its existing `refresh_tray_labels`).
    ///
    /// `NIM_ADD` always sets the freshly loaded *base* icon, so if the tray
    /// was showing the greyed paused icon (or the Offline tint) before
    /// Explorer restarted, this re-applies it (dropping any stale cached
    /// `greyed_icon`/`offline_icon`, since `base_icon` may now be a
    /// different handle).
    pub fn readd(&mut self) -> Result<()> {
        self.base_icon = add_icon(self.hwnd, self.instance)?;
        if let Some(icon) = self.greyed_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
        if let Some(icon) = self.offline_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
        apply_icon(self.hwnd, self.resolve_icon());
        Ok(())
    }

    /// Grey (or restore) the tray icon and switch the context menu between
    /// the "Pause" submenu and the single "Resume" item (issue #20).
    /// Pausing takes precedence over the Offline tint when both apply --
    /// see [`Tray::resolve_icon`]. The greyed icon is derived once from
    /// `base_icon` and cached; if derivation fails (best-effort, see
    /// [`make_greyed_icon`]), the icon stays normal-colored -- the tooltip
    /// text ("Paused...") remains the authoritative signal either way,
    /// this is cosmetic.
    pub fn set_paused(&mut self, paused: bool) {
        self.paused = paused;
        apply_icon(self.hwnd, self.resolve_icon());
    }

    /// Issue #19: radio-check `mode` in the tray's "Mode" submenu and, for
    /// `Offline` specifically, tint the icon so it is visually
    /// distinguishable at a glance -- see [`Tray::resolve_icon`] for how
    /// this composes with [`Tray::set_paused`] (Paused always wins).
    pub fn set_mode(&mut self, mode: crate::mode::Mode) {
        self.mode = mode;
        apply_icon(self.hwnd, self.resolve_icon());
    }

    /// Which icon should be showing right now, deriving and caching
    /// `greyed_icon`/`offline_icon` on first use. Paused is visually
    /// dominant: a paused-AND-offline tray still shows the greyed icon,
    /// never a blend of the two and never the Offline tint alone, so the
    /// single most safety-relevant state (nothing runs, rule 5/7) is never
    /// masked by a less urgent one.
    fn resolve_icon(&mut self) -> HICON {
        if self.paused {
            if self.greyed_icon.is_none() {
                self.greyed_icon = make_greyed_icon(self.base_icon);
            }
            return self.greyed_icon.unwrap_or(self.base_icon);
        }
        if self.mode == crate::mode::Mode::Offline {
            if self.offline_icon.is_none() {
                self.offline_icon = make_offline_icon(self.base_icon);
            }
            return self.offline_icon.unwrap_or(self.base_icon);
        }
        self.base_icon
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
        // #25: honors Pause the same way "Calculate selection" does just
        // below -- opening the palette to run an action defeats the point
        // of pausing.
        append_item_state(hmenu, cmd::QUICK_ASK, "Quick Ask", !self.paused)?;
        append_item(hmenu, cmd::EXTRACT_TEXT, "Copy text from screen")?;
        append_item(hmenu, cmd::COPY_REGION, "Copy region to clipboard")?;
        append_item(hmenu, cmd::COPY_LAST, "Copy last answer")?;
        // #115: works with no provider at all, but still honors Pause
        // (issue #20's rule: nothing Wingman does runs while paused, even
        // the model-free actions), same greyed-while-paused treatment as
        // the "Set ... key" items just below.
        append_item_state(
            hmenu,
            cmd::CALCULATE_SELECTION,
            "Calculate selection",
            !self.paused,
        )?;
        append_item(hmenu, cmd::ADD_TO_CALENDAR, "Add event from screen")?;
        append_item(hmenu, cmd::REVIEW_EMAIL, "Review this email")?;
        append_item(hmenu, cmd::FILL_FORM, "Fill this form")?;
        append_item(hmenu, cmd::RESTORE_LAST_FORM, "Restore last form")?;
        append_separator(hmenu)?;
        if self.paused {
            append_item(hmenu, cmd::RESUME, "Resume")?;
        } else {
            append_pause_submenu(hmenu)?;
        }
        append_separator(hmenu)?;
        append_mode_submenu(hmenu, self.mode)?;
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
        // #185: greyed out while paused. Arming learn mode can never
        // capture while paused (the WH_KEYBOARD_LL hook passes every
        // keydown straight through, per hotkey.rs's pause bypass), so
        // offering it would show a "press now" prompt that can never
        // succeed and leaves a stale armed deadline behind. Mirrors the
        // Pause/Resume item swap just above.
        append_item_state(
            hmenu,
            cmd::SET_PRIMARY,
            &key_label("Set Copilot key", &self.primary_label),
            !self.paused,
        )?;
        append_item_state(
            hmenu,
            cmd::SET_SECONDARY,
            &key_label("Set secondary key", &self.secondary_label),
            !self.paused,
        )?;
        append_item(hmenu, cmd::OPEN_SETTINGS, "Settings...")?;
        append_item(hmenu, cmd::EDIT_SETTINGS, "Open config.toml")?;
        append_item(hmenu, cmd::RELOAD, "Reload settings")?;
        append_item(hmenu, cmd::COPY_DIAGNOSTICS, "Copy diagnostics")?;
        append_item(hmenu, cmd::COPY_EGRESS_LOG, "Copy egress log")?;
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

        // `base_icon` is a shared system handle (see its doc comment) and
        // must not be destroyed; `greyed_icon`/`offline_icon`, if ever
        // built, are owned by this struct and must be.
        if let Some(icon) = self.greyed_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
        if let Some(icon) = self.offline_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
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
    unsafe {
        AppendMenuW(
            hmenu,
            MF_STRING,
            id as usize,
            PCWSTR::from_raw(wide.as_ptr()),
        )
    }
    .context("AppendMenuW failed")
}

/// Same as [`append_item`], but greyed out and disabled when `enabled` is
/// false -- a disabled item sends no `WM_COMMAND` when clicked. Used by
/// [`Tray::build_menu`] for #185. Passes both `MF_GRAYED` and `MF_DISABLED`
/// explicitly, rather than relying on `MF_GRAYED` alone: MSDN documents
/// `MF_GRAYED` as "functionally equivalent" to `MF_DISABLED`, but a real
/// `GetMenuItemInfoW` readback (this module's own test) found the
/// `MFS_DISABLED` bit is not set by `MF_GRAYED` alone.
fn append_item_state(hmenu: HMENU, id: u32, text: &str, enabled: bool) -> Result<()> {
    let wide = to_wide(text);
    let flags = if enabled {
        MF_STRING
    } else {
        MF_STRING | MF_GRAYED | MF_DISABLED
    };
    unsafe { AppendMenuW(hmenu, flags, id as usize, PCWSTR::from_raw(wide.as_ptr())) }
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

/// Build the "Pause" submenu shown while running (issue #20): the three
/// choices from the spec, in the order the spec lists them. Swapped for a
/// single "Resume" item while already paused -- see [`Tray::build_menu`].
fn append_pause_submenu(parent: HMENU) -> Result<()> {
    let sub = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, sub, "Pause")?;
    append_item(sub, cmd::PAUSE_1H, "1 hour")?;
    append_item(sub, cmd::PAUSE_UNTIL_TOMORROW, "Until tomorrow")?;
    append_item(sub, cmd::PAUSE_UNTIL_RESUMED, "Until resumed")?;
    Ok(())
}

/// Build the "Mode" submenu (issue #19): Cloud / Local / Auto / Offline,
/// radio-checked to `current`. Order matches the expansion plan's Modes
/// table.
fn append_mode_submenu(parent: HMENU, current: crate::mode::Mode) -> Result<()> {
    use crate::mode::Mode;

    let sub = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, sub, "Mode")?;

    let items = [
        (cmd::MODE_CLOUD, Mode::Cloud),
        (cmd::MODE_LOCAL, Mode::Local),
        (cmd::MODE_AUTO, Mode::Auto),
        (cmd::MODE_OFFLINE, Mode::Offline),
    ];
    for (id, mode) in items {
        append_item(sub, id, mode.label())?;
    }
    if let Some((id, _)) = items.iter().find(|(_, m)| *m == current) {
        mark_radio_checked(sub, *id);
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
fn add_icon(hwnd: HWND, instance: HINSTANCE) -> Result<HICON> {
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
    set_sz_tip(&mut nid.szTip, "Wingman");

    let added = unsafe { Shell_NotifyIconW(NIM_ADD, &nid) };
    if !added.as_bool() {
        return Err(anyhow!("Shell_NotifyIconW(NIM_ADD) failed"));
    }

    // Best-effort: if the shell won't upgrade us to v4 we still work, just
    // with legacy (non-semantic) mouse messages.
    nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
    let _ = unsafe { Shell_NotifyIconW(NIM_SETVERSION, &nid) };

    Ok(icon)
}

/// `NIM_MODIFY` just the icon (used by [`Tray::set_paused`] to swap between
/// the normal and greyed icon without touching the tooltip or message
/// routing). Best-effort: a failure here leaves the previous icon showing,
/// which is the same degrade [`Tray::set_paused`] already documents.
fn apply_icon(hwnd: HWND, icon: HICON) {
    let nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_ICON,
        hIcon: icon,
        ..Default::default()
    };
    let _ = unsafe { Shell_NotifyIconW(NIM_MODIFY, &nid) };
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

/// How much of the original color/alpha survives in the paused icon (issue
/// #20): `0.0` = fully grey and transparent, `1.0` = unchanged. See
/// [`grey_pixel`].
const GREY_FACTOR: f32 = 0.45;

/// How strongly the Offline icon (issue #19) shifts toward blue: `0.0` = no
/// tint (identical to the base icon), `1.0` = fully blue. Deliberately
/// modest -- unlike Pause, Offline is not meant to look "disabled", just
/// distinguishable at a glance; see [`offline_tint_pixel`] and
/// `Tray::resolve_icon`'s doc comment for why Paused still wins when both
/// apply.
const OFFLINE_TINT_FACTOR: f32 = 0.35;

/// Desaturate and dim a single 32bpp BGRA pixel toward the "greyed out"
/// look, blending its color toward its own luma and scaling its alpha, both
/// by `factor`. Pure and allocation-free so the transform itself is
/// unit-tested without touching GDI -- [`derive_icon_with_transform`] is
/// the only place that reads or writes real pixel memory, and is Win32-only
/// (checked by hand per CLAUDE.md rule 8; the manual check itself is named
/// on issue #20's closing comment and issue #166).
fn grey_pixel([b, g, r, a]: [u8; 4], factor: f32) -> [u8; 4] {
    let luma = 0.114 * b as f32 + 0.587 * g as f32 + 0.299 * r as f32;
    let mix = |c: u8| -> u8 {
        (luma + (c as f32 - luma) * factor)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    let new_a = (a as f32 * factor).round().clamp(0.0, 255.0) as u8;
    [mix(b), mix(g), mix(r), new_a]
}

/// Shift a single 32bpp BGRA pixel toward blue by `factor` (issue #19):
/// unlike [`grey_pixel`], alpha is left untouched -- Offline is not meant
/// to look dimmed or disabled, only cool-toned and distinguishable from the
/// normal icon at a glance. Pure and allocation-free, same reasoning as
/// `grey_pixel`'s doc comment.
fn offline_tint_pixel([b, g, r, a]: [u8; 4], factor: f32) -> [u8; 4] {
    let bluer = (b as f32 + (255.0 - b as f32) * factor)
        .round()
        .clamp(0.0, 255.0) as u8;
    let shrink =
        |c: u8| -> u8 { (c as f32 * (1.0 - factor * 0.5)).round().clamp(0.0, 255.0) as u8 };
    [bluer, shrink(g), shrink(r), a]
}

/// Derive a greyed version of `icon` at runtime (issue #20): reads its
/// 32bpp color bitmap, desaturates and dims every pixel with
/// [`grey_pixel`], and builds a new icon from the result plus the original
/// (untouched) mask bitmap. Thin wrapper around
/// [`derive_icon_with_transform`]; see that function's doc for the shared
/// GDI plumbing and failure/cleanup behavior.
fn make_greyed_icon(icon: HICON) -> Option<HICON> {
    derive_icon_with_transform(icon, |px| grey_pixel(px, GREY_FACTOR))
}

/// Derive the Offline-tinted version of `icon` at runtime (issue #19), the
/// same way [`make_greyed_icon`] derives the paused one, using
/// [`offline_tint_pixel`] instead of [`grey_pixel`] as the per-pixel
/// transform.
fn make_offline_icon(icon: HICON) -> Option<HICON> {
    derive_icon_with_transform(icon, |px| offline_tint_pixel(px, OFFLINE_TINT_FACTOR))
}

/// Shared GDI plumbing behind [`make_greyed_icon`] and [`make_offline_icon`]
/// (issue #19 factored this out of what was originally `make_greyed_icon`
/// alone, so the two icon variants can never let their DIB-handling drift
/// apart from each other): reads `icon`'s 32bpp color bitmap, applies
/// `transform` to every pixel, and builds a new icon from the result plus
/// the original (untouched) mask bitmap.
///
/// Returns `None` on any GDI failure; callers fall back to the normal icon
/// -- the underlying state (Paused / Offline) is still fully in effect
/// either way, this is cosmetic. Cleans up every GDI object it creates or
/// that `GetIconInfo` hands back on every path, including the early-return
/// failure paths.
fn derive_icon_with_transform(
    icon: HICON,
    transform: impl Fn([u8; 4]) -> [u8; 4],
) -> Option<HICON> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(icon, &mut info).ok()?;
        // GetIconInfo hands back NEW bitmaps this function owns; both must
        // be deleted on every path below, success or failure.
        let hbm_color = info.hbmColor;
        let hbm_mask = info.hbmMask;

        let mut bmp = BITMAP::default();
        let got_size = GetObjectW(
            hbm_color.into(),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bmp as *mut _ as *mut _),
        );
        if got_size == 0 || bmp.bmWidth <= 0 || bmp.bmHeight <= 0 {
            let _ = DeleteObject(hbm_color.into());
            let _ = DeleteObject(hbm_mask.into());
            return None;
        }
        let (width, height) = (bmp.bmWidth, bmp.bmHeight);

        let dc = CreateCompatibleDC(None);
        if dc.is_invalid() {
            let _ = DeleteObject(hbm_color.into());
            let _ = DeleteObject(hbm_mask.into());
            return None;
        }

        let mut bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height, // negative: top-down, matches the loop below
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        let color_dib =
            match CreateDIBSection(Some(dc), &bmi, DIB_RGB_COLORS, &mut bits_ptr, None, 0) {
                Ok(h) => h,
                Err(_) => {
                    let _ = DeleteDC(dc);
                    let _ = DeleteObject(hbm_color.into());
                    let _ = DeleteObject(hbm_mask.into());
                    return None;
                }
            };

        let lines = GetDIBits(
            dc,
            hbm_color,
            0,
            height as u32,
            Some(bits_ptr),
            &mut bmi,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        let _ = DeleteObject(hbm_color.into());
        if lines == 0 {
            let _ = DeleteObject(color_dib.into());
            let _ = DeleteObject(hbm_mask.into());
            return None;
        }

        let pixel_count = width as usize * height as usize;
        let pixels = std::slice::from_raw_parts_mut(bits_ptr as *mut [u8; 4], pixel_count);
        for px in pixels.iter_mut() {
            *px = transform(*px);
        }

        let new_info = ICONINFO {
            fIcon: BOOL(1),
            xHotspot: info.xHotspot,
            yHotspot: info.yHotspot,
            hbmMask: hbm_mask,
            hbmColor: color_dib,
        };
        let result = CreateIconIndirect(&new_info);

        // CreateIconIndirect copies the bitmaps it's given internally, so
        // both of ours are freed here regardless of whether it succeeded.
        let _ = DeleteObject(color_dib.into());
        let _ = DeleteObject(hbm_mask.into());

        result.ok()
    }
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
                w!("Wingman tray test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");
        // The exact scenario #145 is about: re-adding after the icon was
        // dropped (here simulated by just calling it again on the same,
        // already-added icon id -- NIM_ADD is idempotent for that case).
        tray.readd().expect("Tray::readd should re-add the icon");

        drop(tray); // NIM_DELETE via Drop
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #185: SET_PRIMARY/SET_SECONDARY greyed while paused ---------------

    #[test]
    fn set_primary_and_secondary_are_greyed_out_only_while_paused() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, MFS_GRAYED,
            WINDOW_EX_STYLE, WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray menu test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");

        fn item_is_greyed(hmenu: HMENU, id: u32) -> bool {
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_STATE,
                ..Default::default()
            };
            unsafe { GetMenuItemInfoW(hmenu, id, false, &mut info) }.expect("GetMenuItemInfoW");
            (info.fState.0 & MFS_GRAYED.0) == MFS_GRAYED.0
        }

        let hmenu_running = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu_running)
            .expect("build_menu while running");
        assert!(
            !item_is_greyed(hmenu_running, cmd::SET_PRIMARY),
            "SET_PRIMARY must not be greyed while running"
        );
        assert!(
            !item_is_greyed(hmenu_running, cmd::SET_SECONDARY),
            "SET_SECONDARY must not be greyed while running"
        );
        let _ = unsafe { DestroyMenu(hmenu_running) };

        tray.set_paused(true);
        let hmenu_paused = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu_paused)
            .expect("build_menu while paused");
        assert!(
            item_is_greyed(hmenu_paused, cmd::SET_PRIMARY),
            "SET_PRIMARY must be greyed while paused -- arming learn mode can never capture"
        );
        assert!(
            item_is_greyed(hmenu_paused, cmd::SET_SECONDARY),
            "SET_SECONDARY must be greyed while paused"
        );
        let _ = unsafe { DestroyMenu(hmenu_paused) };

        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #25: "Quick Ask" is reachable from the menu -------------------------

    #[test]
    fn quick_ask_item_is_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray quick-ask test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::QUICK_ASK, false, &mut info) }
            .expect("cmd::QUICK_ASK must be a real item id in the built menu, not orphaned data");

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #41: "Copy text from screen" is reachable from the menu ------------

    #[test]
    fn extract_text_item_is_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray extract-text test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::EXTRACT_TEXT, false, &mut info) }.expect(
            "cmd::EXTRACT_TEXT must be a real item id in the built menu, not orphaned data",
        );

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #29: "Copy region to clipboard" is reachable from the menu --------

    #[test]
    fn copy_region_item_is_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray copy-region test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::COPY_REGION, false, &mut info) }
            .expect("cmd::COPY_REGION must be a real item id in the built menu, not orphaned data");

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    #[test]
    fn copy_egress_log_item_is_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray copy-egress-log test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::COPY_EGRESS_LOG, false, &mut info) }.expect(
            "cmd::COPY_EGRESS_LOG must be a real item id in the built menu, not orphaned data",
        );

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #38: "Review this email" is reachable from the menu -----------------

    #[test]
    fn review_email_item_is_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray review-email test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::REVIEW_EMAIL, false, &mut info) }.expect(
            "cmd::REVIEW_EMAIL must be a real item id in the built menu, not orphaned data",
        );

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #40: "Fill this form" and "Restore last form" are reachable --------

    #[test]
    fn fill_form_and_restore_last_form_items_are_present_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray fill-form test"),
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

        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");

        let mut info = MENUITEMINFOW {
            cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
            fMask: MIIM_STATE,
            ..Default::default()
        };
        unsafe { GetMenuItemInfoW(hmenu, cmd::FILL_FORM, false, &mut info) }
            .expect("cmd::FILL_FORM must be a real item id in the built menu, not orphaned data");
        unsafe { GetMenuItemInfoW(hmenu, cmd::RESTORE_LAST_FORM, false, &mut info) }.expect(
            "cmd::RESTORE_LAST_FORM must be a real item id in the built menu, not orphaned data",
        );

        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
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
        ("PAUSE_1H", cmd::PAUSE_1H),
        ("PAUSE_UNTIL_TOMORROW", cmd::PAUSE_UNTIL_TOMORROW),
        ("PAUSE_UNTIL_RESUMED", cmd::PAUSE_UNTIL_RESUMED),
        ("RESUME", cmd::RESUME),
        ("MODE_CLOUD", cmd::MODE_CLOUD),
        ("MODE_LOCAL", cmd::MODE_LOCAL),
        ("MODE_AUTO", cmd::MODE_AUTO),
        ("MODE_OFFLINE", cmd::MODE_OFFLINE),
        ("COPY_DIAGNOSTICS", cmd::COPY_DIAGNOSTICS),
        ("COPY_EGRESS_LOG", cmd::COPY_EGRESS_LOG),
        ("EXTRACT_TEXT", cmd::EXTRACT_TEXT),
        ("CALCULATE_SELECTION", cmd::CALCULATE_SELECTION),
        ("COPY_REGION", cmd::COPY_REGION),
        ("ADD_TO_CALENDAR", cmd::ADD_TO_CALENDAR),
        ("QUICK_ASK", cmd::QUICK_ASK),
        ("REVIEW_EMAIL", cmd::REVIEW_EMAIL),
        ("FILL_FORM", cmd::FILL_FORM),
        ("RESTORE_LAST_FORM", cmd::RESTORE_LAST_FORM),
    ];

    /// Issue #234, half of it: every id in [`ALL_FIXED_CMD_IDS`] must be a
    /// real item in some menu the user can open, not orphaned data. This
    /// generalizes the hand-written `*_item_is_present_in_the_built_menu`
    /// tests above so a newly added id cannot be forgotten.
    ///
    /// The menu is state-dependent, so "some menu" means the union over
    /// both pause states: `RESUME` appears only while paused, and the whole
    /// `PAUSE_*` submenu only while running (see `build_menu`). Checking one
    /// state alone reports the other state's items as orphaned, which is
    /// how this test failed the first time it ran.
    ///
    /// The other half of #234 (proving each id also has a `WM_COMMAND` arm
    /// in `app.rs`'s `wnd_proc`) is not covered here: that match lives in
    /// another module and a test cannot see it.
    #[test]
    fn every_fixed_cmd_id_is_a_real_item_in_the_built_menu() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, GetMenuItemInfoW, CW_USEDEFAULT, WINDOW_EX_STYLE,
            WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray cmd-id exhaustiveness test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");

        let mut found = std::collections::HashSet::new();
        for paused in [false, true] {
            tray.set_paused(paused);
            let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
            tray.build_menu(hmenu).expect("build_menu");
            for (_, id) in ALL_FIXED_CMD_IDS {
                let mut info = MENUITEMINFOW {
                    cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                    fMask: MIIM_STATE,
                    ..Default::default()
                };
                if unsafe { GetMenuItemInfoW(hmenu, *id, false, &mut info) }.is_ok() {
                    found.insert(*id);
                }
            }
            let _ = unsafe { DestroyMenu(hmenu) };
        }

        let missing: Vec<String> = ALL_FIXED_CMD_IDS
            .iter()
            .filter(|(_, id)| !found.contains(id))
            .map(|(name, id)| format!("{name} ({id})"))
            .collect();

        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };

        assert!(
            missing.is_empty(),
            "these cmd ids exist but are in no menu the user can open, in either pause state: {}",
            missing.join(", ")
        );
    }

    /// Issue #234: the registry above has to stay exhaustive, or the
    /// uniqueness and menu-presence tests quietly stop covering a new id.
    /// Same source-scanning technique `app.rs` uses for `ALL_WM_APP_IDS`.
    #[test]
    fn all_fixed_cmd_ids_lists_every_declared_command() {
        // Not fixed menu commands. The two *_MODEL_BASE values start the
        // dynamic model-submenu ranges (dispatched by MenuChoice::OpenAiModel /
        // AnthropicModel, not by exact id) and MODEL_RANGE is that width.
        const DYNAMIC: &[&str] = &["MODEL_RANGE", "OPENAI_MODEL_BASE", "ANTHROPIC_MODEL_BASE"];

        let declared: Vec<&str> = include_str!("tray.rs")
            .lines()
            .filter_map(|line| {
                let t = line.trim_start();
                let rest = t.strip_prefix("pub const ")?;
                let name = rest.split(':').next()?.trim();
                (rest.contains(": u32 =")
                    && !name.is_empty()
                    && name.chars().all(|c| c.is_ascii_uppercase() || c == '_'))
                .then_some(name)
            })
            .filter(|name| !DYNAMIC.contains(name) && !name.starts_with("WM_APP_"))
            .collect();

        let listed: Vec<&str> = ALL_FIXED_CMD_IDS.iter().map(|(n, _)| *n).collect();
        let missing: Vec<&&str> = declared.iter().filter(|n| !listed.contains(n)).collect();

        assert!(
            missing.is_empty(),
            "these cmd constants are declared but absent from ALL_FIXED_CMD_IDS, so the uniqueness and menu-presence tests do not cover them: {missing:?}"
        );
    }

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

    // -- grey_pixel (issue #20's paused icon) -------------------------------

    #[test]
    fn grey_pixel_scales_alpha_by_factor() {
        let px = grey_pixel([10, 20, 30, 200], 0.5);
        assert_eq!(px[3], 100);
    }

    #[test]
    fn grey_pixel_leaves_a_neutral_grey_hue_unchanged() {
        // A pixel that is already grey (b == g == r) has luma equal to its
        // own channel value, so mix() must return it unchanged regardless
        // of factor -- only the alpha moves.
        let px = grey_pixel([128, 128, 128, 255], 0.45);
        assert_eq!(&px[..3], &[128, 128, 128]);
    }

    #[test]
    fn grey_pixel_fully_transparent_stays_transparent() {
        let px = grey_pixel([1, 2, 3, 0], 0.45);
        assert_eq!(px[3], 0);
    }

    #[test]
    fn grey_pixel_factor_one_is_identity() {
        let px = grey_pixel([10, 20, 30, 255], 1.0);
        assert_eq!(px, [10, 20, 30, 255]);
    }

    #[test]
    fn grey_pixel_factor_zero_is_pure_luma_and_transparent() {
        let px = grey_pixel([10, 20, 30, 255], 0.0);
        assert_eq!(px[3], 0);
        assert_eq!(px[0], px[1]);
        assert_eq!(px[1], px[2]);
    }

    #[test]
    fn grey_pixel_desaturates_a_saturated_color_toward_its_luma() {
        // A saturated red should move toward grey, not stay saturated: the
        // green/blue channels rise from 0 while red falls from 255, both
        // toward the same luma value.
        let px = grey_pixel([0, 0, 255, 255], GREY_FACTOR);
        assert!(px[0] > 0, "blue should rise toward luma, got {}", px[0]);
        assert!(px[1] > 0, "green should rise toward luma, got {}", px[1]);
        assert!(px[2] < 255, "red should fall toward luma, got {}", px[2]);
    }

    // -- make_greyed_icon / Tray::set_paused, against real GDI (issue #20) --
    //
    // These exercise the actual Win32 path (not just the pure pixel
    // transform above), in the same spirit as `tray_new_then_readd_...`
    // above: this sandbox does have a real desktop session, so it is worth
    // proving the GDI calls succeed and clean up rather than only asserting
    // that on paper. Whether the result *looks* right in a live tray is
    // still a named manual check (issue #20's closing comment / issue
    // #166), not something a test can see.

    #[test]
    fn make_greyed_icon_succeeds_against_a_real_icon() {
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.expect("LoadIconW(IDI_APPLICATION)");
        let grey = make_greyed_icon(icon);
        assert!(
            grey.is_some(),
            "make_greyed_icon should derive a greyed icon from a real system icon"
        );
        if let Some(g) = grey {
            let _ = unsafe { DestroyIcon(g) };
        }
    }

    #[test]
    fn tray_set_paused_derives_and_caches_the_greyed_icon_against_a_real_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, CW_USEDEFAULT, WINDOW_EX_STYLE, WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray pause test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");
        assert!(!tray.paused);

        tray.set_paused(true);
        assert!(tray.paused);
        assert!(
            tray.greyed_icon.is_some(),
            "set_paused(true) should have derived and cached a greyed icon"
        );

        tray.set_paused(false);
        assert!(!tray.paused);

        drop(tray); // Drop must DestroyIcon(greyed_icon) without panicking.
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- offline_tint_pixel (issue #19) --------------------------------------

    #[test]
    fn offline_tint_pixel_factor_zero_is_identity() {
        let px = offline_tint_pixel([10, 20, 30, 255], 0.0);
        assert_eq!(px, [10, 20, 30, 255]);
    }

    #[test]
    fn offline_tint_pixel_leaves_alpha_untouched() {
        // Unlike grey_pixel, Offline is not meant to look dimmed.
        for a in [0u8, 1, 128, 255] {
            let px = offline_tint_pixel([10, 20, 30, a], OFFLINE_TINT_FACTOR);
            assert_eq!(px[3], a);
        }
    }

    #[test]
    fn offline_tint_pixel_shifts_a_neutral_grey_toward_blue() {
        let px = offline_tint_pixel([128, 128, 128, 255], OFFLINE_TINT_FACTOR);
        assert!(px[0] > 128, "blue should rise, got {}", px[0]);
        assert!(px[1] < 128, "green should fall, got {}", px[1]);
        assert!(px[2] < 128, "red should fall, got {}", px[2]);
    }

    // -- make_offline_icon / Tray::set_mode, against real GDI (issue #19) --

    #[test]
    fn make_offline_icon_succeeds_against_a_real_icon() {
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.expect("LoadIconW(IDI_APPLICATION)");
        let tinted = make_offline_icon(icon);
        assert!(
            tinted.is_some(),
            "make_offline_icon should derive a tinted icon from a real system icon"
        );
        if let Some(t) = tinted {
            let _ = unsafe { DestroyIcon(t) };
        }
    }

    #[test]
    fn tray_set_mode_offline_derives_and_caches_the_tinted_icon_against_a_real_window() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, CW_USEDEFAULT, WINDOW_EX_STYLE, WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray mode test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");
        assert_eq!(tray.mode, crate::mode::Mode::Auto);
        assert!(tray.offline_icon.is_none());

        tray.set_mode(crate::mode::Mode::Offline);
        assert_eq!(tray.mode, crate::mode::Mode::Offline);
        assert!(
            tray.offline_icon.is_some(),
            "set_mode(Offline) should have derived and cached the tinted icon"
        );

        tray.set_mode(crate::mode::Mode::Auto);
        assert_eq!(tray.mode, crate::mode::Mode::Auto);

        drop(tray); // Drop must DestroyIcon(offline_icon) without panicking.
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    #[test]
    fn resolve_icon_prefers_paused_over_offline_when_both_apply() {
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, CW_USEDEFAULT, WINDOW_EX_STYLE, WS_OVERLAPPED,
        };

        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        let instance = HINSTANCE(h.0);
        let hwnd = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("Wingman tray precedence test"),
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

        let mut tray = Tray::new(hwnd, instance).expect("Tray::new should add the icon");
        tray.set_mode(crate::mode::Mode::Offline);
        tray.set_paused(true);

        // Both caches exist (both were derived at some point), but the
        // Paused icon must be the one actually resolved/applied while both
        // states are active -- see `resolve_icon`'s doc comment.
        let resolved = tray.resolve_icon();
        assert_eq!(
            resolved,
            tray.greyed_icon.unwrap(),
            "Paused must be visually dominant over the Offline tint"
        );

        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }
}
