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

use crate::config::ActiveProvider;
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
    GetIconInfo, GetSystemMetrics, LoadIconW, LoadImageW, RegisterWindowMessageW,
    SetForegroundWindow, SetMenuItemInfoW, TrackPopupMenu, HICON, HMENU, ICONINFO, IDI_APPLICATION,
    IMAGE_ICON, LR_DEFAULTCOLOR, MENUITEMINFOW, MFS_CHECKED, MFT_RADIOCHECK, MF_DISABLED,
    MF_GRAYED, MF_POPUP, MF_SEPARATOR, MF_STRING, MIIM_FTYPE, MIIM_STATE, SM_CXSMICON, SM_CYSMICON,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_CONTEXTMENU, WM_LBUTTONUP, WM_RBUTTONUP,
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
    /// Make Ollama (the local model server) the active provider (#183).
    pub const USE_OLLAMA: u32 = 1032;
    /// Make Gemini the active provider (#195).
    pub const USE_GEMINI: u32 = 1033;

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

    /// Issue #317: the disabled, greyed version line at the top of the
    /// menu. Never enabled, so it never sends `WM_COMMAND`; the id only
    /// needs to be unique among menu items.
    pub const VERSION_LABEL: u32 = 1029;

    /// Issue #202: "Set pause key" learn-mode item, the third sibling of
    /// `SET_PRIMARY`/`SET_SECONDARY`. See `App::start_learning` in `app.rs`.
    pub const SET_PAUSE: u32 = 1030;

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
    /// Quick Ask palette (#25) also lists and runs this action
    /// (`DispatchTarget::ExtractText` in `ui/palette_model.rs`); this tray
    /// item stays as a direct path that skips opening the palette first.
    pub const EXTRACT_TEXT: u32 = 1020;
    /// Issue #115: evaluates the current selection as an arithmetic
    /// expression or a unit conversion, no model involved. See
    /// `App::calculate_selection` in `app.rs`. The Quick Ask palette (#25)
    /// also lists and runs it (`CALCULATE_SELECTION_ACTION_ID` in
    /// `ui/palette_model.rs`); this tray item is the direct path.
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
    /// Propose, Confirm, Do). See `App::review_this_email` in `app.rs`. Same
    /// as `EXTRACT_TEXT` above: the Quick Ask palette (#25) also lists and
    /// runs this action (`DispatchTarget::ReviewEmail` in
    /// `ui/palette_model.rs`), and this tray item is the direct path.
    pub const REVIEW_EMAIL: u32 = 1025;
    /// Issue #40: runs the built-in "Fill this form" action (Look, Propose,
    /// Confirm, Do). See `App::fill_form_from_screen` in `app.rs`.
    pub const FILL_FORM: u32 = 1026;
    /// Issue #40: Undo for the most recent successful `fill_form` run. See
    /// `App::restore_last_form` in `app.rs`.
    pub const RESTORE_LAST_FORM: u32 = 1027;

    /// Issue #313: "Read QR code from screen" -- decode every QR code on the
    /// active monitor locally and copy the text. See `App::read_qr` in
    /// `app.rs` and `actions::read_qr`. The Quick Ask palette also lists it
    /// (`DispatchTarget::ReadQr`); this tray item is the direct path.
    pub const READ_QR: u32 = 1031;

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
    /// Display string for the pause-toggle binding (issue #202), or "not
    /// set". Set via [`Tray::set_pause_binding`].
    pause_label: String,
    /// Model lists and current-selection index for the two submenus. Set
    /// via [`Tray::set_models`]; empty until then, in which case the
    /// submenu shows a single greyed-out "none configured" entry.
    /// Which provider is first in `providers.order`; radio-checked in the
    /// Provider submenu.
    active_provider: ActiveProvider,
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
    /// Issue #327: a request is in flight / the last run failed. Set via
    /// [`Tray::set_busy`] and [`Tray::set_failed`]; see [`icon_state`].
    busy: bool,
    failed: bool,
    /// Lazily derived badge icons, same ownership rules as `greyed_icon`.
    working_icon: Option<HICON>,
    failed_icon: Option<HICON>,
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
            pause_label: String::new(),
            active_provider: ActiveProvider::OpenAi,
            openai_models: Vec::new(),
            openai_current: None,
            anthropic_models: Vec::new(),
            anthropic_current: None,
            base_icon,
            greyed_icon: None,
            paused: false,
            offline_icon: None,
            mode: crate::mode::Mode::default(),
            busy: false,
            failed: false,
            working_icon: None,
            failed_icon: None,
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
        if let Some(icon) = self.working_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
        if let Some(icon) = self.failed_icon.take() {
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

    /// Issue #327: show (or clear) the working badge. Only touches the
    /// shell icon when the resolved state actually changes.
    pub fn set_busy(&mut self, busy: bool) {
        self.update_state(|t| t.busy = busy);
    }

    /// Issue #327: show (or clear) the last-run-failed badge.
    pub fn set_failed(&mut self, failed: bool) {
        self.update_state(|t| t.failed = failed);
    }

    /// The state the icon is showing right now, for tooltip text.
    pub fn icon_state(&self) -> IconState {
        icon_state(self.paused, self.busy, self.failed, self.mode)
    }

    fn update_state(&mut self, change: impl FnOnce(&mut Self)) {
        let before = self.icon_state();
        change(self);
        if self.icon_state() != before {
            apply_icon(self.hwnd, self.resolve_icon());
        }
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
        match self.icon_state() {
            IconState::Paused => {
                if self.greyed_icon.is_none() {
                    self.greyed_icon = make_greyed_icon(self.base_icon);
                }
                self.greyed_icon.unwrap_or(self.base_icon)
            }
            IconState::Working => {
                if self.working_icon.is_none() {
                    self.working_icon = make_working_icon(self.instance, self.base_icon);
                }
                self.working_icon.unwrap_or(self.base_icon)
            }
            IconState::Failed => {
                if self.failed_icon.is_none() {
                    self.failed_icon = make_failed_icon(self.instance, self.base_icon);
                }
                self.failed_icon.unwrap_or(self.base_icon)
            }
            IconState::Offline => {
                if self.offline_icon.is_none() {
                    self.offline_icon = make_offline_icon(self.base_icon);
                }
                self.offline_icon.unwrap_or(self.base_icon)
            }
            IconState::Idle => self.base_icon,
        }
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

    /// Display string for the "Set pause key" label (issue #202), e.g.
    /// `hotkey::pause_binding_label(config.hotkeys.pause.as_ref())`.
    pub fn set_pause_binding(&mut self, pause: &str) {
        self.pause_label = pause.to_string();
    }

    /// Populates the two model submenus. `*_current` marks which entry gets
    /// the radio check; a current model that is not in its list is still
    /// shown, appended at the end, so a hand-edited config.toml value is
    /// never hidden or silently switched. Lists longer than
    /// [`cmd::MODEL_RANGE`] entries are clamped (dropping from the end,
    /// keeping room to append `*_current` if it would otherwise be
    /// dropped). Does not touch the shell icon; only affects the next menu
    /// built by [`Tray::on_tray_message`].
    /// Marks which provider is active, for the Provider submenu's radio check.
    pub fn set_active_provider(&mut self, active: ActiveProvider) {
        self.active_provider = active;
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

    /// Whether this tray message opens the menu, which takes the foreground
    /// (see `show_menu`). The caller reads who was in front BEFORE
    /// `on_tray_message` runs (#339).
    pub fn opens_menu(lparam: LPARAM) -> bool {
        let event = (lparam.0 as u32) & 0xFFFF;
        event == WM_CONTEXTMENU || event == WM_RBUTTONUP
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
        // #317: a disabled, greyed first line showing the running version --
        // no `WM_COMMAND` arm needed since it's never enabled and thus never
        // clickable (see `append_item_state`'s doc comment on #185).
        append_item_state(hmenu, cmd::VERSION_LABEL, &version_label(), false)?;
        append_separator(hmenu)?;
        append_item(hmenu, cmd::ASK_NOW, "Ask now (same as the key)")?;

        // #348: every action lives in one submenu. Quick Ask is first
        // because it is the palette that lists the rest.
        let actions = append_group(hmenu, "Actions")?;
        // #25: honors Pause the same way "Calculate selection" does below --
        // opening the palette to run an action defeats the point of pausing.
        append_item_state(
            actions,
            cmd::QUICK_ASK,
            "Quick Ask (pick an action)",
            !self.paused,
        )?;
        append_separator(actions)?;
        append_item(actions, cmd::EXTRACT_TEXT, "Copy text from screen")?;
        append_item(actions, cmd::READ_QR, "Read QR code from screen")?;
        append_item(actions, cmd::COPY_REGION, "Copy region to clipboard")?;
        // #115: works with no provider at all, but still honors Pause
        // (issue #20's rule: nothing Wingman does runs while paused, even
        // the model-free actions), same greyed-while-paused treatment as
        // the "Set ... key" items below.
        append_item_state(
            actions,
            cmd::CALCULATE_SELECTION,
            "Calculate selection",
            !self.paused,
        )?;
        append_item(actions, cmd::ADD_TO_CALENDAR, "Add event from screen")?;
        append_item(actions, cmd::REVIEW_EMAIL, "Review this email")?;
        append_item(actions, cmd::FILL_FORM, "Fill this form")?;
        append_item(actions, cmd::RESTORE_LAST_FORM, "Restore last form")?;

        append_item(hmenu, cmd::COPY_LAST, "Copy last answer")?;
        append_separator(hmenu)?;
        if self.paused {
            append_item(hmenu, cmd::RESUME, "Resume")?;
        } else {
            append_pause_submenu(hmenu)?;
        }
        append_mode_submenu(hmenu, self.mode)?;

        // #348: providers, models and the key-learning items in one place.
        let setup = append_group(hmenu, "Providers and keys")?;
        append_provider_submenu(setup, self.active_provider)?;
        append_model_submenu(
            setup,
            "ChatGPT model",
            cmd::OPENAI_MODEL_BASE,
            &self.openai_models,
            self.openai_current,
        )?;
        append_model_submenu(
            setup,
            "Claude model",
            cmd::ANTHROPIC_MODEL_BASE,
            &self.anthropic_models,
            self.anthropic_current,
        )?;
        append_separator(setup)?;
        // #185: greyed out while paused. Arming learn mode can never
        // capture while paused (the WH_KEYBOARD_LL hook passes every
        // keydown straight through, per hotkey.rs's pause bypass), so
        // offering it would show a "press now" prompt that can never
        // succeed and leaves a stale armed deadline behind. Mirrors the
        // Pause/Resume item swap above.
        append_item_state(
            setup,
            cmd::SET_PRIMARY,
            &key_label("Set Copilot key", &self.primary_label),
            !self.paused,
        )?;
        append_item_state(
            setup,
            cmd::SET_SECONDARY,
            &key_label("Set secondary key", &self.secondary_label),
            !self.paused,
        )?;
        append_item_state(
            setup,
            cmd::SET_PAUSE,
            &key_label("Set pause key", &self.pause_label),
            !self.paused,
        )?;

        append_separator(hmenu)?;
        append_item(hmenu, cmd::OPEN_SETTINGS, "Settings...")?;
        let help = append_group(hmenu, "Help and diagnostics")?;
        append_item(help, cmd::EDIT_SETTINGS, "Open config.toml")?;
        append_item(help, cmd::RELOAD, "Reload settings")?;
        append_item(help, cmd::COPY_DIAGNOSTICS, "Copy diagnostics")?;
        append_item(help, cmd::COPY_EGRESS_LOG, "Copy egress log")?;
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
        if let Some(icon) = self.working_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
        if let Some(icon) = self.failed_icon.take() {
            let _ = unsafe { DestroyIcon(icon) };
        }
    }
}

/// Issue #317: the disabled first line of the tray menu, e.g. "Wingman
/// 0.1.0". Pure so it can be unit-tested without a real menu; `build.rs`
/// does not currently embed a git hash (only `diagnostics.rs`'s
/// `CARGO_PKG_VERSION` exists), so there is nothing to append yet.
fn version_label() -> String {
    format!("Wingman {}", env!("CARGO_PKG_VERSION"))
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

/// Create an empty submenu labeled `text`, attached to `parent` before it is
/// populated (same ownership rule as [`attach_submenu_or_destroy`]), and
/// return its handle so the caller can fill it. Used by [`Tray::build_menu`]
/// for the #348 groups.
fn append_group(parent: HMENU, text: &str) -> Result<HMENU> {
    let sub = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, sub, text)?;
    Ok(sub)
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
fn append_provider_submenu(parent: HMENU, active: ActiveProvider) -> Result<()> {
    let sub = unsafe { CreatePopupMenu() }.context("CreatePopupMenu failed")?;
    attach_submenu_or_destroy(parent, sub, "Provider")?;
    for provider in ActiveProvider::ALL {
        append_item(sub, provider_cmd(provider), provider_menu_label(provider))?;
    }
    mark_radio_checked(sub, provider_cmd(active));
    Ok(())
}

/// The menu command each Provider entry sends.
fn provider_cmd(provider: ActiveProvider) -> u32 {
    match provider {
        ActiveProvider::OpenAi => cmd::USE_OPENAI,
        ActiveProvider::Anthropic => cmd::USE_ANTHROPIC,
        ActiveProvider::Ollama => cmd::USE_OLLAMA,
        ActiveProvider::Gemini => cmd::USE_GEMINI,
    }
}

/// Menu text. Ollama says "(local)" so it reads as the on-device choice next
/// to the two cloud ones.
fn provider_menu_label(provider: ActiveProvider) -> &'static str {
    match provider {
        ActiveProvider::OpenAi => "ChatGPT",
        ActiveProvider::Anthropic => "Claude",
        ActiveProvider::Ollama => "Ollama (local)",
        ActiveProvider::Gemini => "Gemini",
    }
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
    let mut n = wide.len().min(dst.len() - 1);
    // #231: a cut point purely by code-unit count can land between a
    // surrogate pair (an astral character, e.g. an emoji, encodes as a
    // high surrogate 0xD800..=0xDBFF followed by a low surrogate). Back
    // the cut up by one so a lone high surrogate is never left dangling
    // right before the null terminator.
    if n > 0 && n < wide.len() && (0xD800..=0xDBFF).contains(&wide[n - 1]) {
        n -= 1;
    }
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

/// Badge colours (BGRA) for the Working and Failed icons (issue #327).
const WORKING_BADGE_BGRA: [u8; 4] = [0, 150, 255, 255];
const FAILED_BADGE_BGRA: [u8; 4] = [40, 40, 220, 255];

/// What the tray icon shows (issue #327). Event-driven: it is recomputed
/// only when one of its inputs changes, never on a timer, and every state is
/// a static icon (no animation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IconState {
    Idle,
    Working,
    Paused,
    Offline,
    Failed,
}

/// Pure decision for which state wins. Paused is dominant (nothing runs, the
/// safest thing to show). A request in flight beats everything else, since
/// "did my press register" is the point of the badge. A failed last run
/// beats Offline because it is news; Offline is a standing mode.
pub fn icon_state(paused: bool, busy: bool, failed: bool, mode: crate::mode::Mode) -> IconState {
    if paused {
        IconState::Paused
    } else if busy {
        IconState::Working
    } else if failed {
        IconState::Failed
    } else if mode == crate::mode::Mode::Offline {
        IconState::Offline
    } else {
        IconState::Idle
    }
}

/// Tooltip for `state` wrapped around the normal `base` text. The state
/// leads, so `set_sz_tip`'s truncation of long text drops the tail of
/// `base`, never the state. Paused text comes from `pause::tooltip_text`
/// and is passed through unchanged.
pub fn state_tooltip(state: IconState, base: &str) -> String {
    match state {
        IconState::Idle | IconState::Paused => base.to_string(),
        IconState::Working => format!("Working... · {base}"),
        IconState::Failed => format!("Last run failed · {base}"),
        IconState::Offline => format!("Offline · {base}"),
    }
}

/// Draws a filled dot with a white rim in the bottom-right corner of a
/// top-down `width` x `height` BGRA buffer. Pure; a buffer shorter than
/// `width * height` or a non-positive size is left alone.
fn draw_badge(px: &mut [[u8; 4]], width: i32, height: i32, color: [u8; 4]) {
    if width < 2 || height < 2 || px.len() < (width as usize) * (height as usize) {
        return;
    }
    let r = (width.min(height) / 4).max(2);
    let (cx, cy) = (width - r - 1, height - r - 1);
    for y in 0..height {
        for x in 0..width {
            let d2 = (x - cx).pow(2) + (y - cy).pow(2);
            if d2 > r * r {
                continue;
            }
            let inner = r < 4 || d2 <= (r - 1) * (r - 1);
            px[(y * width + x) as usize] = if inner { color } else { [255, 255, 255, 255] };
        }
    }
}

/// Desaturate and dim a single 32bpp BGRA pixel toward the "greyed out"
/// look, blending its color toward its own luma and scaling its alpha, both
/// by `factor`. Pure and allocation-free so the transform itself is
/// unit-tested without touching GDI -- [`derive_icon_with_transform`] is
/// the only place that reads or writes real pixel memory, and is Win32-only
/// (checked by hand per AGENTS.md rule 8; the manual check itself is named
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

/// Issue #327: the icon with a small orange dot in its bottom-right corner,
/// shown while a request is in flight.
fn make_working_icon(instance: HINSTANCE, base: HICON) -> Option<HICON> {
    derive_badged_icon(instance, base, WORKING_BADGE_BGRA)
}

/// Issue #327: the icon with a small red dot, shown after a run failed until
/// the next one starts.
fn make_failed_icon(instance: HINSTANCE, base: HICON) -> Option<HICON> {
    derive_badged_icon(instance, base, FAILED_BADGE_BGRA)
}

/// Loads the embedded icon at the small-icon size the tray actually shows
/// (`SM_CXSMICON` x `SM_CYSMICON`, 16px at 100%, 24px at 150%), so a badge
/// drawn on it is crisp rather than a downsampled 32px blob. The caller owns
/// the result and must `DestroyIcon` it. `None` if the resource is missing
/// (e.g. in a test binary), in which case the caller uses the base icon.
fn load_small_icon(instance: HINSTANCE) -> Option<HICON> {
    let (w, h) = unsafe { (GetSystemMetrics(SM_CXSMICON), GetSystemMetrics(SM_CYSMICON)) };
    if w <= 0 || h <= 0 {
        return None;
    }
    let embedded = PCWSTR(EMBEDDED_ICON_ID as usize as *const u16);
    let handle =
        unsafe { LoadImageW(Some(instance), embedded, IMAGE_ICON, w, h, LR_DEFAULTCOLOR) }.ok()?;
    Some(HICON(handle.0))
}

/// Draws `color`'s badge on the small-size icon (see [`load_small_icon`]),
/// falling back to `base` when no small icon can be loaded.
fn derive_badged_icon(instance: HINSTANCE, base: HICON, color: [u8; 4]) -> Option<HICON> {
    let small = load_small_icon(instance);
    let derived = derive_icon_with_edit(small.unwrap_or(base), |px, w, h| {
        draw_badge(px, w, h, color)
    });
    if let Some(s) = small {
        let _ = unsafe { DestroyIcon(s) };
    }
    derived
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
    derive_icon_with_edit(icon, |pixels, _, _| {
        for px in pixels.iter_mut() {
            *px = transform(*px);
        }
    })
}

/// [`derive_icon_with_transform`]'s plumbing, generalized to an edit of the
/// whole pixel buffer (top-down BGRA, `width` x `height`) so a variant can
/// depend on pixel position, as the corner badges of [`draw_badge`] do.
fn derive_icon_with_edit(
    icon: HICON,
    edit: impl FnOnce(&mut [[u8; 4]], i32, i32),
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
        edit(pixels, width, height);

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

    // -- Version label (#317) ------------------------------------------------

    #[test]
    fn version_label_shows_the_crate_version() {
        let label = version_label();
        assert_eq!(label, format!("Wingman {}", env!("CARGO_PKG_VERSION")));
        assert!(!label.contains('\u{2014}'), "no em dashes in menu text");
    }

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
        assert!(
            !item_is_greyed(hmenu_running, cmd::SET_PAUSE),
            "SET_PAUSE must not be greyed while running"
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
        assert!(
            item_is_greyed(hmenu_paused, cmd::SET_PAUSE),
            "SET_PAUSE must be greyed while paused"
        );
        let _ = unsafe { DestroyMenu(hmenu_paused) };

        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
    }

    // -- #348: the menu tree --------------------------------------------------

    /// One node of a built menu, read back from the real `HMENU`.
    #[derive(Debug)]
    enum Node {
        Sep,
        /// `(command id, label, greyed, radio-checked)`
        Item(u32, String, bool, bool),
        Sub(String, Vec<Node>),
    }

    impl Node {
        fn label(&self) -> Option<&str> {
            match self {
                Node::Sep => None,
                Node::Item(_, t, ..) | Node::Sub(t, _) => Some(t),
            }
        }
    }

    fn read_menu(hmenu: HMENU) -> Vec<Node> {
        use windows::Win32::UI::WindowsAndMessaging::{
            GetMenuItemCount, GetMenuItemInfoW, MFS_GRAYED, MIIM_ID, MIIM_STRING, MIIM_SUBMENU,
        };
        let n = unsafe { GetMenuItemCount(Some(hmenu)) };
        assert!(n >= 0, "GetMenuItemCount failed");
        let mut out = Vec::new();
        for pos in 0..n as u32 {
            let mut buf = [0u16; 128];
            let mut info = MENUITEMINFOW {
                cbSize: std::mem::size_of::<MENUITEMINFOW>() as u32,
                fMask: MIIM_ID | MIIM_FTYPE | MIIM_STATE | MIIM_STRING | MIIM_SUBMENU,
                dwTypeData: windows::core::PWSTR(buf.as_mut_ptr()),
                cch: buf.len() as u32 - 1,
                ..Default::default()
            };
            unsafe { GetMenuItemInfoW(hmenu, pos, true, &mut info) }.expect("GetMenuItemInfoW");
            if info.fType.0 & MF_SEPARATOR.0 != 0 {
                out.push(Node::Sep);
                continue;
            }
            let text = String::from_utf16_lossy(&buf[..info.cch as usize]);
            if info.hSubMenu.0.is_null() {
                out.push(Node::Item(
                    info.wID,
                    text,
                    info.fState.0 & MFS_GRAYED.0 != 0,
                    info.fState.0 & MFS_CHECKED.0 != 0,
                ));
            } else {
                out.push(Node::Sub(text, read_menu(info.hSubMenu)));
            }
        }
        out
    }

    /// Build the real menu for a throwaway tray window and read it back.
    fn built_menu_tree(configure: impl FnOnce(&mut Tray)) -> Vec<Node> {
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
                w!("Wingman tray menu tree test"),
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
        configure(&mut tray);
        let hmenu = unsafe { CreatePopupMenu() }.expect("CreatePopupMenu");
        tray.build_menu(hmenu).expect("build_menu");
        let tree = read_menu(hmenu);
        let _ = unsafe { DestroyMenu(hmenu) };
        drop(tray);
        let _ = unsafe { DestroyWindow(hwnd) };
        tree
    }

    fn ids_under(nodes: &[Node], out: &mut Vec<u32>) {
        for n in nodes {
            match n {
                Node::Item(id, ..) => out.push(*id),
                Node::Sub(_, kids) => ids_under(kids, out),
                Node::Sep => {}
            }
        }
    }

    fn find_sub<'a>(nodes: &'a [Node], label: &str) -> &'a [Node] {
        nodes
            .iter()
            .find_map(|n| match n {
                Node::Sub(t, kids) if t == label => Some(kids.as_slice()),
                _ => None,
            })
            .unwrap_or_else(|| panic!("no submenu {label:?}"))
    }

    fn direct_ids(nodes: &[Node]) -> Vec<u32> {
        nodes
            .iter()
            .filter_map(|n| match n {
                Node::Item(id, ..) => Some(*id),
                _ => None,
            })
            .collect()
    }

    fn find_item(nodes: &[Node], want: u32) -> Option<&Node> {
        for n in nodes {
            match n {
                Node::Item(id, ..) if *id == want => return Some(n),
                Node::Sub(_, kids) => {
                    if let Some(found) = find_item(kids, want) {
                        return Some(found);
                    }
                }
                _ => {}
            }
        }
        None
    }

    #[test]
    fn menu_top_level_is_ten_entries_or_fewer_and_grouped() {
        let tree = built_menu_tree(|_| {});
        let top: Vec<&str> = tree.iter().filter_map(Node::label).collect();
        assert!(
            top.len() <= 10,
            "top level has {} entries: {top:?}",
            top.len()
        );
        let expect = [
            format!("Wingman {}", env!("CARGO_PKG_VERSION")),
            "Ask now (same as the key)".to_string(),
            "Actions".to_string(),
            "Copy last answer".to_string(),
            "Pause".to_string(),
            "Mode".to_string(),
            "Providers and keys".to_string(),
            "Settings...".to_string(),
            "Help and diagnostics".to_string(),
            "Quit".to_string(),
        ];
        assert_eq!(top, expect.iter().map(String::as_str).collect::<Vec<_>>());
    }

    #[test]
    fn menu_groups_hold_the_expected_commands() {
        let tree = built_menu_tree(|_| {});

        assert_eq!(
            direct_ids(&tree),
            [
                cmd::VERSION_LABEL,
                cmd::ASK_NOW,
                cmd::COPY_LAST,
                cmd::OPEN_SETTINGS,
                cmd::QUIT
            ]
        );

        let mut actions = direct_ids(find_sub(&tree, "Actions"));
        actions.sort_unstable();
        let mut want = vec![
            cmd::QUICK_ASK,
            cmd::EXTRACT_TEXT,
            cmd::READ_QR,
            cmd::COPY_REGION,
            cmd::CALCULATE_SELECTION,
            cmd::ADD_TO_CALENDAR,
            cmd::REVIEW_EMAIL,
            cmd::FILL_FORM,
            cmd::RESTORE_LAST_FORM,
        ];
        want.sort_unstable();
        assert_eq!(actions, want);

        let mut help = direct_ids(find_sub(&tree, "Help and diagnostics"));
        help.sort_unstable();
        let mut want = vec![
            cmd::EDIT_SETTINGS,
            cmd::RELOAD,
            cmd::COPY_DIAGNOSTICS,
            cmd::COPY_EGRESS_LOG,
        ];
        want.sort_unstable();
        assert_eq!(help, want);

        let setup = find_sub(&tree, "Providers and keys");
        assert_eq!(
            direct_ids(setup),
            [cmd::SET_PRIMARY, cmd::SET_SECONDARY, cmd::SET_PAUSE]
        );
        let mut providers = Vec::new();
        ids_under(find_sub(setup, "Provider"), &mut providers);
        assert_eq!(providers.len(), ActiveProvider::ALL.len());
        find_sub(setup, "ChatGPT model");
        find_sub(setup, "Claude model");

        assert_eq!(
            direct_ids(find_sub(&tree, "Pause")),
            [
                cmd::PAUSE_1H,
                cmd::PAUSE_UNTIL_TOMORROW,
                cmd::PAUSE_UNTIL_RESUMED
            ]
        );
        assert_eq!(find_sub(&tree, "Mode").len(), 4);
    }

    #[test]
    fn menu_tree_has_no_em_dashes_and_every_fixed_id_is_reachable() {
        let mut all = Vec::new();
        for paused in [false, true] {
            let tree = built_menu_tree(|t| t.set_paused(paused));
            fn labels(nodes: &[Node], out: &mut Vec<String>) {
                for n in nodes {
                    if let Some(l) = n.label() {
                        out.push(l.to_string());
                    }
                    if let Node::Sub(_, k) = n {
                        labels(k, out);
                    }
                }
            }
            let mut ls = Vec::new();
            labels(&tree, &mut ls);
            assert!(ls.len() > 20, "walker saw only {} labels", ls.len());
            assert!(ls.iter().all(|l| !l.contains('\u{2014}')), "{ls:?}");
            ids_under(&tree, &mut all);
        }
        for (name, id) in ALL_FIXED_CMD_IDS {
            assert!(all.contains(id), "{name} is in no menu");
        }
    }

    /// Wired-to-nothing guard for dynamic state that now sits inside
    /// submenus: the pause greying, the Mode and Provider radio marks.
    #[test]
    fn dynamic_state_still_shows_inside_the_submenus() {
        let running = built_menu_tree(|t| {
            t.set_mode(crate::mode::Mode::Local);
            t.set_active_provider(ActiveProvider::Gemini);
        });
        let grey = |tree: &[Node], id| match find_item(tree, id) {
            Some(Node::Item(_, _, g, _)) => *g,
            _ => panic!("id {id} missing"),
        };
        let checked = |tree: &[Node], id| match find_item(tree, id) {
            Some(Node::Item(_, _, _, c)) => *c,
            _ => panic!("id {id} missing"),
        };
        assert!(checked(&running, cmd::MODE_LOCAL));
        assert!(!checked(&running, cmd::MODE_CLOUD));
        assert!(checked(&running, cmd::USE_GEMINI));
        assert!(!checked(&running, cmd::USE_OPENAI));
        assert!(!grey(&running, cmd::SET_PAUSE));
        assert!(!grey(&running, cmd::QUICK_ASK));

        let paused = built_menu_tree(|t| t.set_paused(true));
        for id in [
            cmd::SET_PRIMARY,
            cmd::SET_SECONDARY,
            cmd::SET_PAUSE,
            cmd::QUICK_ASK,
            cmd::CALCULATE_SELECTION,
        ] {
            assert!(grey(&paused, id), "{id} must be greyed while paused");
        }
        let top: Vec<&str> = paused.iter().filter_map(Node::label).collect();
        assert!(top.contains(&"Resume"), "{top:?}");
        assert!(!top.contains(&"Pause"));
        assert!(top.len() <= 10);
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
        ("SET_PAUSE", cmd::SET_PAUSE),
        ("EDIT_SETTINGS", cmd::EDIT_SETTINGS),
        ("RELOAD", cmd::RELOAD),
        ("QUIT", cmd::QUIT),
        ("OPEN_SETTINGS", cmd::OPEN_SETTINGS),
        ("USE_OPENAI", cmd::USE_OPENAI),
        ("USE_ANTHROPIC", cmd::USE_ANTHROPIC),
        ("USE_OLLAMA", cmd::USE_OLLAMA),
        ("USE_GEMINI", cmd::USE_GEMINI),
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
        ("READ_QR", cmd::READ_QR),
        ("VERSION_LABEL", cmd::VERSION_LABEL),
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

    #[test]
    fn provider_menu_offers_ollama_and_gemini_with_their_own_commands() {
        assert_eq!(provider_cmd(ActiveProvider::Ollama), cmd::USE_OLLAMA);
        assert_eq!(provider_cmd(ActiveProvider::Gemini), cmd::USE_GEMINI);
        assert_eq!(
            provider_menu_label(ActiveProvider::Ollama),
            "Ollama (local)"
        );
        assert_eq!(provider_menu_label(ActiveProvider::Gemini), "Gemini");
        let mut ids: Vec<u32> = ActiveProvider::ALL
            .iter()
            .map(|p| provider_cmd(*p))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(
            ids.len(),
            ActiveProvider::ALL.len(),
            "one command per entry"
        );
        for p in ActiveProvider::ALL {
            assert!(!provider_menu_label(p).contains('\u{2014}'));
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
    fn make_working_and_failed_icons_succeed_against_a_real_icon() {
        let icon = unsafe { LoadIconW(None, IDI_APPLICATION) }.expect("LoadIconW(IDI_APPLICATION)");
        // A null instance has no embedded resource, so this also exercises
        // the fall back to the base icon.
        let none = HINSTANCE(std::ptr::null_mut());
        for derived in [make_working_icon(none, icon), make_failed_icon(none, icon)] {
            let d = derived.expect("badge icon should derive from a real system icon");
            let _ = unsafe { DestroyIcon(d) };
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

    // -- #327: which icon for which state ----------------------------------

    use crate::mode::Mode;

    #[test]
    fn icon_state_has_one_row_per_state() {
        assert_eq!(icon_state(false, false, false, Mode::Auto), IconState::Idle);
        assert_eq!(
            icon_state(false, true, false, Mode::Auto),
            IconState::Working
        );
        assert_eq!(
            icon_state(true, false, false, Mode::Auto),
            IconState::Paused
        );
        assert_eq!(
            icon_state(false, false, false, Mode::Offline),
            IconState::Offline
        );
        assert_eq!(
            icon_state(false, false, true, Mode::Auto),
            IconState::Failed
        );
    }

    #[test]
    fn icon_state_precedence_is_paused_working_failed_offline() {
        // Everything at once: paused wins.
        assert_eq!(
            icon_state(true, true, true, Mode::Offline),
            IconState::Paused
        );
        // Not paused: a request in flight beats a stale failure and Offline.
        assert_eq!(
            icon_state(false, true, true, Mode::Offline),
            IconState::Working
        );
        // Idle but failed beats Offline.
        assert_eq!(
            icon_state(false, false, true, Mode::Offline),
            IconState::Failed
        );
    }

    #[test]
    fn state_tooltip_leads_with_the_state_and_idle_is_unchanged() {
        assert_eq!(state_tooltip(IconState::Idle, "Wingman: x"), "Wingman: x");
        assert_eq!(
            state_tooltip(IconState::Working, "Wingman: x"),
            "Working... · Wingman: x"
        );
        assert_eq!(
            state_tooltip(IconState::Failed, "Wingman: x"),
            "Last run failed · Wingman: x"
        );
        assert_eq!(
            state_tooltip(IconState::Offline, "Wingman: x"),
            "Offline · Wingman: x"
        );
        // Paused text is produced by `pause::tooltip_text`, not here.
        assert_eq!(state_tooltip(IconState::Paused, "Paused"), "Paused");
        for s in [IconState::Working, IconState::Failed, IconState::Offline] {
            assert!(!state_tooltip(s, "b").contains('\u{2014}'));
        }
    }

    #[test]
    fn draw_badge_paints_the_bottom_right_corner_only() {
        let (w, h) = (32i32, 32i32);
        // A fully transparent fixture, like the empty corner of a real icon:
        // a badge that forgot to set alpha would stay invisible.
        let clear = [10u8, 20, 30, 0];
        let mut px = vec![clear; (w * h) as usize];
        let red = [0u8, 0, 220, 255];
        draw_badge(&mut px, w, h, red);
        // Centre of the badge is the badge colour, fully opaque.
        assert_eq!(px[((h - 9) * w + (w - 9)) as usize], red);
        // Every pixel the badge touched, rim included, is opaque.
        let painted: Vec<_> = px.iter().filter(|p| **p != clear).collect();
        assert!(!painted.is_empty());
        assert!(painted.iter().all(|p| p[3] == 255), "{painted:?}");
        // The opposite corners are untouched.
        assert_eq!(px[0], clear);
        assert_eq!(px[(w - 1) as usize], clear);
    }

    #[test]
    fn draw_badge_tolerates_tiny_and_degenerate_sizes() {
        for (w, h) in [(1i32, 1i32), (2, 2), (16, 16), (0, 0)] {
            let mut px = vec![[0u8; 4]; (w * h) as usize];
            draw_badge(&mut px, w, h, [1, 2, 3, 255]);
        }
    }

    // -- #231: set_sz_tip never truncates mid-surrogate-pair ----------------

    #[test]
    fn set_sz_tip_backs_up_the_cut_off_a_surrogate_pair() {
        // 126 ASCII chars (126 UTF-16 units) then one astral emoji (a
        // surrogate pair, 2 more units) is exactly 128 units -- the cut
        // point a naive `dst.len() - 1 == 127` would land on is index 127,
        // i.e. right after the emoji's high surrogate (index 126) and
        // before its low surrogate (index 127). Without the fix this would
        // keep the lone high surrogate at dst[126] and drop its pair.
        let text: String = "a".repeat(126) + "\u{1F600}"; // trailing emoji (U+1F600)
        let wide: Vec<u16> = text.encode_utf16().collect();
        assert_eq!(wide.len(), 128, "fixture must be exactly 128 UTF-16 units");
        assert!(
            (0xD800..=0xDBFF).contains(&wide[126]),
            "fixture's unit 126 must be a high surrogate"
        );

        let mut dst = [0u16; 128];
        set_sz_tip(&mut dst, &text);

        // The cut must have backed up to before the surrogate pair: all 126
        // ASCII units survive, and everything from there on is the null
        // terminator, not a lone high surrogate.
        assert_eq!(&dst[..126], &wide[..126]);
        assert_eq!(dst[126], 0, "must end with the null terminator");
        assert_eq!(dst[127], 0);
        assert!(
            !(0xD800..=0xDBFF).contains(&dst[125]),
            "no lone high surrogate must remain in the buffer"
        );
    }

    #[test]
    fn set_sz_tip_copies_a_short_string_exactly() {
        let mut dst = [0xFFFFu16; 16];
        set_sz_tip(&mut dst, "hi");

        let expected: Vec<u16> = "hi".encode_utf16().collect();
        assert_eq!(&dst[..2], expected.as_slice());
        assert!(
            dst[2..].iter().all(|&c| c == 0),
            "everything past the copied text must be null-terminated"
        );
    }

    #[test]
    fn set_sz_tip_on_an_empty_dst_is_a_no_op() {
        let mut dst: [u16; 0] = [];
        // Must not panic (no `dst.len() - 1` underflow) and must leave the
        // (empty) buffer untouched.
        set_sz_tip(&mut dst, "Wingman");
        assert!(dst.is_empty());
    }
}
