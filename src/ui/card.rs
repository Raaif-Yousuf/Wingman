//! The notification card: a single borderless, always-on-top, non-activating
//! popup window with three states (Pending / Collapsed / Expanded). See the
//! "UI: the notification card" section of the design spec.
//!
//! # Public API
//!
//! ```ignore
//! pub enum CardState { Hidden, Pending, Collapsed, Expanded }
//!
//! pub struct Card { .. }
//!
//! impl Card {
//!     pub fn new(instance: HINSTANCE) -> anyhow::Result<Self>;
//!     pub fn hwnd(&self) -> HWND;
//!     pub fn show_pending(&mut self, stage: PendingStage);
//!     pub fn set_pending_stage(&mut self, stage: PendingStage); // #354
//!     pub fn show_answer(&mut self, headline: &str, detail: &str, auto_dismiss_secs: u32, difficulty: Option<Difficulty>);
//!     pub fn show_error(&mut self, headline: &str, detail: &str);
//!     pub fn show_error_with_details(&mut self, headline: &str, detail: &str);
//!     pub fn hide(&mut self);
//!     pub fn set_text_scale(&mut self, scale: f32);
//!     pub fn state(&self) -> CardState;
//!     pub fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT>;
//! }
//! ```
//!
//! `Card` registers and owns its own window class (guarded by `Once` so it is
//! only ever registered once per process) and its own `WNDPROC`. That
//! `WNDPROC` is a thin static shim that looks up the `Card`'s inner state via
//! `GWLP_USERDATA` and forwards every message to [`Card::handle_message`], so
//! the integrating agent does not need to do any message routing for the
//! card's `HWND` beyond running the normal `GetMessageW`/`DispatchMessageW`
//! loop — `DispatchMessageW` calls straight into our `WNDPROC` for messages
//! addressed to `card.hwnd()`. `handle_message` is exposed publicly mainly so
//! callers (or tests) can feed it synthetic messages directly.
//!
//! Notes for the integrating agent:
//! - Call [`Card::new`] once, after the process has set
//!   `PROCESS_PER_MONITOR_DPI_AWARE_V2` (that call is process-global and is
//!   expected to live in `app.rs`/`main.rs`; this module only *reads* DPI via
//!   `GetDpiForWindow`, it never sets process DPI awareness itself).
//! - `show_pending` / `show_answer` / `show_error` are all safe to call at any
//!   time from any state; each one fully resets timers, activation style and
//!   geometry before showing.
//! - `show_error` intentionally has no `auto_dismiss_secs` parameter (per the
//!   spec's given signature): an error card never auto-dismisses, since it is
//!   the one state where staying on screen until the user notices it is more
//!   valuable than tidiness. Call `hide()` explicitly if different behaviour
//!   is wanted.
//! - `show_error` also has no `difficulty` parameter: errors have no
//!   difficulty rating, and `show_error` always clears any badge left over
//!   from a previous `show_answer` call so it can never linger on an error
//!   card.
//! - `show_answer`'s `difficulty: Option<Difficulty>` draws a small badge in
//!   the card's bottom-right corner (Collapsed and Expanded only, never
//!   Pending). Pass `None` to render exactly as before this feature existed
//!   (no badge, no reserved space, no layout shift) -- this is what the
//!   integrator should pass when the user has the feature disabled.
//! - `show_pending` likewise has no timeout parameter, but pending state still
//!   carries an internal safety-net auto-dismiss (`PENDING_SAFETY_TIMEOUT_SECS`)
//!   so a card is never stuck forever if the worker thread never reports back.
//! - Nothing in this module panics. Win32 calls that can fail are always
//!   handled by falling back to a degraded-but-functional default (square
//!   corners, stock font, light theme, primary-monitor work area, etc.).

use std::ffi::c_void;
use std::sync::{Once, OnceLock};

use crate::config::CardCorner;
use crate::speech::{spoken_text, Speaker};
use crate::ui::card_placement::{
    card_origin, dpi_to_apply, resolve_monitor, sort_left_to_right, MonitorChoice,
};
use crate::ui::text::draw_text_line;
use crate::ui::theme::{
    contrast_ratio, detect_theme, relative_luminance, rgb, setting_change_affects_theme,
    setting_change_name, type_height_px, SysColors, Theme, Tokens, BADGE_HIGH, BADGE_INK_DARK,
    BADGE_INK_LIGHT, BADGE_LOW, BADGE_MID, BADGE_ULTRA, CONTROL_RADIUS_DP, FALLBACK_FONT_FACE,
    GAP_DP, MARGIN_DP, PADDING_DP, PREFERRED_FONT_FACE, TYPE_BODY_PX, TYPE_CAPTION_PX,
};
use serde_json::Value;
use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    Arc, BeginPaint, BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, CreateFontIndirectW,
    CreatePen, CreateSolidBrush, DeleteDC, DeleteObject, DrawTextW, Ellipse, EndPaint,
    EnumDisplayMonitors, ExtCreatePen, FillRect, FrameRect, GetDC, GetMonitorInfoW, GetStockObject,
    GetTextMetricsW, IntersectClipRect, MapWindowPoints, MonitorFromPoint, MonitorFromWindow,
    RedrawWindow, ReleaseDC, RoundRect, SelectClipRgn, SelectObject, SetBkColor, SetBkMode,
    SetTextColor, BS_SOLID, DEFAULT_GUI_FONT, DT_CALCRECT, DT_CENTER, DT_END_ELLIPSIS, DT_LEFT,
    DT_NOPREFIX, DT_SINGLELINE, DT_TOP, DT_VCENTER, DT_WORDBREAK, FW_NORMAL, FW_SEMIBOLD, HBRUSH,
    HDC, HFONT, HGDIOBJ, LOGBRUSH, MONITORINFO, MONITOR_DEFAULTTONEAREST, NULL_BRUSH, NULL_PEN,
    PS_ENDCAP_ROUND, PS_GEOMETRIC, PS_JOIN_ROUND, PS_SOLID, RDW_ALLCHILDREN, RDW_INVALIDATE,
    SRCCOPY, TEXTMETRICW, TRANSPARENT,
};
use windows::Win32::UI::HiDpi::{GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT, VK_DOWN, VK_END,
    VK_ESCAPE, VK_HOME, VK_MENU, VK_NEXT, VK_PRIOR, VK_RETURN, VK_SHIFT, VK_SPACE, VK_TAB, VK_UP,
};
use windows::Win32::UI::Shell::{DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetCursorPos,
    GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextLengthW, GetWindowTextW,
    IsChild, IsWindow, KillTimer, LoadCursorW, PostMessageW, RegisterClassExW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    SystemParametersInfoW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, GWLP_USERDATA, GWL_EXSTYLE,
    HMENU, HWND_TOPMOST, IDC_ARROW, NONCLIENTMETRICSW, SPI_GETCLIENTAREAANIMATION,
    SPI_GETNONCLIENTMETRICS, SPI_GETWORKAREA, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE,
    SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    WM_APP, WM_COMMAND, WM_CTLCOLOREDIT, WM_DESTROY, WM_DPICHANGED, WM_DRAWITEM, WM_ERASEBKGND,
    WM_KEYDOWN, WM_KILLFOCUS, WM_LBUTTONDOWN, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NCCREATE,
    WM_NCDESTROY, WM_PAINT, WM_SETCURSOR, WM_SETFOCUS, WM_SETFONT, WM_SETTINGCHANGE,
    WM_SYSCOLORCHANGE, WM_THEMECHANGED, WM_TIMER, WM_USER, WNDCLASSEXW, WS_CHILD, WS_DISABLED,
    WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_POPUP, WS_TABSTOP, WS_VISIBLE,
};

/// Posted to the card's owner window (see [`Card::set_owner`]) whenever the
/// preview state (#26) closes with a decision -- "Do it" or Cancel/Esc
/// (issue #39). Carries no payload: `App::on_preview_decided` pulls the
/// result via [`Card::take_confirmed`], the same take-not-peek shape that
/// method already has, so there is nothing to box across the message
/// boundary and nothing to free even if this arrives while Settings is
/// open and gets dropped. Adding another `WM_APP_*` constant anywhere in
/// the crate also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS`
/// (issue #163) and its `count_declarations` file list -- see that test's
/// doc comment.
pub const WM_APP_PREVIEW_DECIDED: u32 = WM_APP + 9;

/// Posted to the card's owner window (see [`Card::set_owner`]) when the user
/// clicks a card shown via [`Card::show_settings_needed`] -- issue #347.
/// Carries no payload: `App::on_card_open_settings` just calls
/// `App::open_settings()`. Adding another `WM_APP_*` constant anywhere in the
/// crate also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue
/// #163) and its `count_declarations` file list -- see that test's doc
/// comment.
pub const WM_APP_CARD_OPEN_SETTINGS: u32 = WM_APP + 15;

/// Posted to the card's owner window (see [`Card::set_owner`]) when the user
/// clicks or activates (Enter) the "Copy details" affordance on an expanded
/// error card -- issue #425. Carries no payload: the full raw error chain
/// does not live on `Card` at all (only the already-humanized `detail` does),
/// so `App::copy_error_details` reads it from `App::last_error` (set by
/// `App::record_last_error`, the same store #426's Copy diagnostics already
/// reads) rather than anything this message would carry. Adding another
/// `WM_APP_*` constant anywhere in the crate also means adding it to
/// `app.rs`'s `tests::ALL_WM_APP_IDS` (issue #163) and its
/// `count_declarations` file list -- see that test's doc comment.
pub const WM_APP_CARD_COPY_DETAILS: u32 = WM_APP + 17;

use crate::provider::Difficulty;
pub use crate::ui::pending_status::PendingStage;
use crate::ui::pending_status::{pending_glyph_should_spin, pending_status_text};
use crate::ui::preview::{Field, PreviewModel};

// ---------------------------------------------------------------------------
// Public state
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardState {
    Hidden,
    Pending,
    Collapsed,
    Expanded,
    /// The confirmation state (#26): a proposal rendered as a compact form
    /// (title + one row per schema field), with "Do it" / "Edit" buttons.
    /// No Cancel button (#392): nothing has run yet, so there is nothing to
    /// undo -- Esc or a click elsewhere dismiss it instead. See
    /// [`Card::show_preview`].
    Preview,
}

/// A single owned notification-card window.
///
/// `Card` is a thin handle around a heap-allocated `CardInner`. The indirection
/// matters: the window's `WNDPROC` stashes a raw pointer to `CardInner` in
/// `GWLP_USERDATA` at creation time, and that pointer must stay valid for the
/// life of the window even if the `Card` handle itself is moved (e.g. stored
/// into a field of the caller's app struct). Boxing the real state means only
/// the box's *pointer* moves around; the pointee's address never changes.
pub struct Card {
    inner: Box<CardInner>,
}

impl Card {
    pub fn new(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the card window class");
        }
        Card::create(instance, CLASS_NAME)
    }

    /// Same as [`Card::new`], but registers (once) and uses a class name
    /// distinct from the production window class (rule 9: tests never touch
    /// production names). Used only by the preview state's real-Win32 test
    /// in this module, which needs an actual `HWND` with real child
    /// controls, not the production `Card`'s class.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn new_for_test(instance: HINSTANCE) -> anyhow::Result<Self> {
        if !ensure_test_class_registered(instance) {
            anyhow::bail!("Wingman: failed to register the test card window class");
        }
        Card::create(instance, TEST_CLASS_NAME)
    }

    fn create(instance: HINSTANCE, class_name: &str) -> anyhow::Result<Self> {
        let theme = detect_theme();
        // A 96 DPI guess used only to build a first, throwaway set of fonts
        // before we have a real HWND to ask GetDpiForWindow about. It is
        // replaced immediately below once the window exists.
        let inner = Box::new(CardInner {
            hwnd: HWND(std::ptr::null_mut()),
            instance,
            state: CardState::Hidden,
            headline: String::new(),
            detail: String::new(),
            difficulty: None,
            dpi: 96,
            theme,
            sys: SysColors::read(),
            theme_forced: false,
            dpi_forced: false,
            fonts: Fonts::null(),
            text_scale: 1.0,
            corner: CardCorner::default(),
            monitor: MonitorChoice::Cursor,
            follow_anchor: POINT::default(),
            place_from_cursor: false,
            anim_frame: 0,
            scroll_offset: 0,
            scroll_max: 0,
            scroll_page: 0,
            ex_noactivate_removed: false,
            preview: None,
            last_confirmed: None,
            owner: None,
            open_settings_on_click: false,
            copy_details_available: false,
            copy_details_rect: None,
            copy_available: false,
            focus_link: None,
            copy_rect: None,
            copy_feedback: CopyFeedback::None,
            read_aloud_available: false,
            read_aloud_rect: None,
            speaker: None,
            reading: false,
            reading_stream: 0,
            preview_decision_pending: false,
            preview_generation: 0,
            edit_bg_brush: HBRUSH(std::ptr::null_mut()),
            pending_stage: PendingStage::Working,
            animations_enabled: true,
        });
        let raw = Box::into_raw(inner);

        let class_name = wide_z(class_name);
        let title = wide_z("Wingman");
        let create_result = unsafe {
            CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance),
                Some(raw as *const c_void),
            )
        };

        let hwnd = match create_result {
            Ok(hwnd) => hwnd,
            Err(e) => {
                // Reclaim and drop the box we leaked into CreateWindowExW so
                // it does not leak on the error path.
                unsafe {
                    drop(Box::from_raw(raw));
                }
                return Err(anyhow::anyhow!("Wingman: CreateWindowExW failed: {e}"));
            }
        };

        // Safety: `raw` is still a valid, uniquely-owned allocation (nothing
        // has taken ownership of it yet — WM_NCCREATE only copied the pointer
        // value into GWLP_USERDATA). Reclaim it into the `Card` we return.
        let inner_ref = unsafe { &mut *raw };
        inner_ref.hwnd = hwnd;
        inner_ref.dpi = unsafe { GetDpiForWindow(hwnd) }.max(1);
        inner_ref.rebuild_fonts();

        // Best-effort rounded corners (Windows 11). Falling back to square
        // corners on older systems or DWM failures is an acceptable degrade.
        unsafe {
            let pref = DWMWCP_ROUND;
            if let Err(e) = DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &pref as *const DWM_WINDOW_CORNER_PREFERENCE as *const c_void,
                std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
            ) {
                // #356: say so in "Copy diagnostics" instead of failing silently.
                crate::diagnostics::note_warning(&format!(
                    "Card: rounded corners unavailable ({}), using square corners",
                    e.code()
                ));
            }
        }

        Ok(Card {
            inner: unsafe { Box::from_raw(raw) },
        })
    }

    pub fn hwnd(&self) -> HWND {
        self.inner.hwnd
    }

    /// Issue #39: tells the card which window to `PostMessageW`
    /// [`WM_APP_PREVIEW_DECIDED`] to when a preview closes with a
    /// decision. Call once, right after [`Card::new`] -- see `owner`'s
    /// doc comment on [`CardInner`].
    pub fn set_owner(&mut self, hwnd: HWND) {
        self.inner.owner = Some(hwnd);
    }

    /// `stage` is the caller's honest description of what is happening at
    /// the moment the card appears -- see `CardInner::show_pending`'s doc
    /// comment for why there is no default/implicit starting stage any
    /// more.
    pub fn show_pending(&mut self, stage: PendingStage) {
        self.inner.show_pending(stage);
    }

    /// Issue #354: moves an already-showing Pending card to a new sub-stage
    /// (e.g. "Asking {model}..." -> "Still working."), updating both the
    /// painted status line and the window's accessible text. A no-op if the
    /// card is not currently Pending (see `CardInner::set_pending_stage`).
    pub fn set_pending_stage(&mut self, stage: PendingStage) {
        self.inner.set_pending_stage(stage);
    }

    pub fn show_answer(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
    ) {
        self.inner
            .show_collapsed(headline, detail, auto_dismiss_secs, difficulty);
        // Issue #77: an answer card offers "Read aloud" once expanded.
        self.inner.read_aloud_available = true;
    }

    /// Issue #531: like [`Card::show_answer`], and the card also offers
    /// "Copy". Only for a model's answer; status and confirmation cards use
    /// [`Card::show_answer`], which has nothing worth copying.
    pub fn show_model_answer(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
    ) {
        self.show_answer(headline, detail, auto_dismiss_secs, difficulty);
        self.inner.copy_available = true;
    }

    pub fn show_error(&mut self, headline: &str, detail: &str) {
        // No timeout parameter is given for errors: they persist until the
        // user dismisses them (click to expand, then Esc / focus-loss), or
        // until a later show_* call replaces them. See module docs.
        //
        // difficulty is always None here: errors have no difficulty rating,
        // and passing None explicitly (rather than leaving a stale value)
        // ensures a previous answer's badge can never linger on an error
        // card.
        //
        // Cold review of #425 (PR #451): deliberately does NOT set
        // `copy_details_available`. `Card` has no way to know whether the
        // headline/detail it was just given is the same failure
        // `App::last_error` holds -- most `show_error` call sites in
        // `app.rs` never call `track_error`/`record_last_error` at all
        // (a static "Hotkeys unavailable" message, `show_tray_restore_error`,
        // "Couldn't locate config.toml", the startup unreadable-secrets
        // card...), so offering "Copy details" here would silently copy
        // whatever unrelated error happened to run last. Only
        // [`Card::show_error_with_details`] -- reserved for callers that
        // just recorded the matching chain -- grows the footer.
        self.inner.show_collapsed(headline, detail, 0, None);
    }

    /// Like [`Card::show_error`], except the card also offers "Copy
    /// details" once Expanded (#425). Callers MUST have just recorded the
    /// exact chain behind `headline`/`detail` as `App::last_error`
    /// (`App::track_error`/`record_last_error`) -- this method does not and
    /// cannot verify that itself, since `Card` never holds the raw chain or
    /// a reference back to `App` (see [`WM_APP_CARD_COPY_DETAILS`]'s doc
    /// comment). Using this for an untracked error would let "Copy details"
    /// silently copy a stale, unrelated chain (PR #451's cold review).
    pub fn show_error_with_details(&mut self, headline: &str, detail: &str) {
        self.inner.show_collapsed(headline, detail, 0, None);
        // Set AFTER show_collapsed, which unconditionally clears this for
        // every other caller (show_answer and plain show_error included) --
        // so a later untracked error, or a later answer, can never inherit
        // a footer left over from an earlier tracked one.
        self.inner.copy_details_available = true;
    }

    /// Issue #347: like [`Card::show_error`] (persists until dismissed, no
    /// difficulty badge), except a click on the card while it is still
    /// Collapsed posts [`WM_APP_CARD_OPEN_SETTINGS`] to the owner window
    /// (see [`Card::set_owner`]) and hides the card, instead of expanding it
    /// for more detail. Used for the readiness-gate cards ("No AI model set
    /// up yet", "Local mode needs Ollama configured") -- the actionable next
    /// step for those is Settings, not more text to read.
    pub fn show_settings_needed(&mut self, headline: &str, detail: &str) {
        self.inner
            .show_collapsed_with(headline, detail, 0, None, true);
    }

    pub fn hide(&mut self) {
        self.inner.hide();
    }

    /// Multiplies every font size in the card. 1.0 is the built-in default.
    /// Clamped to a sane range so a bad config value cannot make the card
    /// unreadable or enormous.
    pub fn set_text_scale(&mut self, scale: f32) {
        self.inner.set_text_scale(scale);
    }

    /// Issue #109: `ui.card_corner` and `ui.card_monitor`. Takes effect the
    /// next time a card is laid out.
    pub fn set_placement(&mut self, corner: CardCorner, monitor: &str) {
        self.inner.corner = corner;
        self.inner.monitor = MonitorChoice::parse(monitor);
    }

    pub fn state(&self) -> CardState {
        self.inner.state
    }

    /// Enters the preview (confirmation) state (#26). `schema` is the
    /// proposal's JSON Schema (see `actions::schema::schema_for`) and
    /// `proposal_value` its current value; together they build a
    /// [`PreviewModel`] the card renders as `title` plus one row per
    /// declared field, with an EDIT control for every field the schema
    /// marks `"editable"`.
    ///
    /// `main_window_exists` gates whether the "Edit" button is created at
    /// all (#352): with no main window to open, a permanently-disabled Edit
    /// button just squeezed "Do it" and "Cancel" for no reason, so the
    /// button is omitted entirely until a main window exists (#26's issue
    /// body: "Edit opens the main window when it exists"). No caller passes
    /// `true` yet -- Wingman has no main window today -- the same "inert
    /// until its caller exists" status `Action::hotkey` has; the button's
    /// create-or-not wiring is still exercised by
    /// `preview_edit_button_only_exists_once_main_window_exists` below.
    ///
    /// Nothing runs until the user presses "Do it" (Enter) or "Cancel"
    /// (Esc): see [`Card::take_confirmed`].
    ///
    /// **Focus** (documented per the task's "decide carefully"): unlike
    /// Pending/Collapsed, which are created `WS_EX_NOACTIVATE` so showing
    /// them never steals focus from whatever the user is doing, Preview
    /// removes that style and takes real keyboard focus immediately once
    /// shown. This is deliberate, not an oversight: the preview state exists
    /// *only* for a proposal that requires an explicit user decision before
    /// anything happens (typing into an editable field, or pressing
    /// Enter/Esc, both need real focus to work at all), so the interruption
    /// is the entire point of showing it -- there is no useful "shown but
    /// not yet interacted with" middle state to preserve focus through, the
    /// way there is for a passive Collapsed answer card. The interruption is
    /// temporary: `close_preview` (on Do it, Cancel, or Esc) always restores
    /// whatever window had the foreground immediately before `show_preview`
    /// was called, so focus returns to the user's previous work the moment
    /// the decision is made either way.
    #[allow(dead_code)] // wiring app.rs's worker to call this is a later issue's job
    /// Shows the preview and returns its generation (#225). Store it: the
    /// `WM_APP_PREVIEW_DECIDED` that eventually arrives carries the
    /// generation it belongs to in its `WPARAM`, and anything older is a
    /// late abandonment for a preview that is already gone, not a decision
    /// about this one.
    #[must_use = "store the generation; on_preview_decided needs it to reject a stale notification"]
    pub fn show_preview(
        &mut self,
        title: &str,
        schema: &serde_json::Value,
        proposal_value: &serde_json::Value,
        main_window_exists: bool,
    ) -> u32 {
        self.inner
            .show_preview(title, schema, proposal_value, main_window_exists);
        self.inner.preview_generation
    }

    /// [`Card::show_preview`] with an optional small line under the fields
    /// (#228: "Answered by X (Y was unavailable)"). Display only: it is not
    /// a field, so it never reaches the confirmed value or an executor.
    #[must_use = "store the generation; on_preview_decided needs it to reject a stale notification"]
    pub fn show_preview_with_note(
        &mut self,
        title: &str,
        schema: &serde_json::Value,
        proposal_value: &serde_json::Value,
        main_window_exists: bool,
        note: Option<&str>,
    ) -> u32 {
        self.inner.show_preview_labeled(
            title,
            schema,
            proposal_value,
            main_window_exists,
            "Do it",
            note,
        );
        self.inner.preview_generation
    }

    /// #105: the send preview. The same preview as [`Card::show_preview`],
    /// with the confirm button reading "Send" instead of "Do it", click-only (Enter
    /// does nothing, #105). Esc and
    /// click-away still cancel.
    #[must_use = "store the generation; on_preview_decided needs it to reject a stale notification"]
    pub fn show_send_preview(
        &mut self,
        title: &str,
        schema: &serde_json::Value,
        proposal_value: &serde_json::Value,
    ) -> u32 {
        self.inner.show_preview_labeled(
            title,
            schema,
            proposal_value,
            false,
            SEND_CONFIRM_LABEL,
            None,
        );
        self.inner.preview_generation
    }

    /// Takes the `Confirmed<Value>` produced by the last "Do it" / Enter, if
    /// any. A take, not a peek: the stored value is cleared by this call, so
    /// the same confirmation can never be handed to an executor twice.
    /// Returns `None` before "Do it" has been pressed, after Cancel/Esc, or
    /// on a repeated call.
    #[allow(dead_code)] // see show_preview's doc comment
    pub fn take_confirmed(&mut self) -> Option<crate::ui::confirm::Confirmed<serde_json::Value>> {
        self.inner.last_confirmed.take()
    }

    /// The UI gallery's `--theme`: draws in `theme` whatever the system is
    /// set to, and ignores later theme messages. High contrast uses Windows'
    /// high contrast black colours. Never touches the registry.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn force_theme_for_gallery(&mut self, theme: Theme) {
        self.inner.theme = theme;
        self.inner.sys = crate::ui::theme::HC_BLACK;
        self.inner.theme_forced = true;
        if !self.inner.hwnd.0.is_null() {
            self.inner.repaint_for_theme();
        }
    }

    /// The UI gallery's `--dpi`: lays the card out at `dpi` whatever the
    /// monitor says, and ignores later DPI changes. Fonts and every layout
    /// measure are rebuilt for it. Never touches display or registry settings.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn force_dpi_for_gallery(&mut self, dpi: u32) {
        self.inner.dpi = dpi.max(1);
        self.inner.dpi_forced = true;
        self.inner.rebuild_fonts();
    }

    /// The UI gallery's expanded answer: what a click on the card does.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn expand_for_gallery(&mut self) {
        self.inner.try_expand();
    }

    /// The UI gallery's focus rectangle: what the first Tab does.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn focus_first_link_for_gallery(&mut self) {
        self.inner.move_footer_focus(false);
    }

    /// The UI gallery's preview error: shows `message` under the preview's
    /// title, as a rejected edit does.
    #[cfg(any(test, debug_assertions))]
    pub(crate) fn set_preview_error_for_gallery(&mut self, message: &str) {
        if let Some(preview) = self.inner.preview.as_mut() {
            preview.error = Some(message.to_string());
            self.inner.relayout_current_state();
            self.inner.invalidate();
        }
    }

    /// The card's own window proc dispatches internally; this is the seam
    /// tests use to feed synthetic messages.
    #[allow(dead_code)]
    pub fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        self.inner.handle_message(msg, wparam, lparam)
    }
}

impl Drop for Card {
    fn drop(&mut self) {
        // Destroying the window is synchronous: WM_DESTROY/WM_NCDESTROY are
        // delivered (and handled by our WNDPROC via the still-valid raw
        // pointer) before DestroyWindow returns, so this is safe to do while
        // `self.inner` is still alive.
        unsafe {
            if !self.inner.hwnd.0.is_null() {
                let _ = DestroyWindow(self.inner.hwnd);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Window class registration (once per process)
// ---------------------------------------------------------------------------

/// Issue #77: the system voice posts this to the card's own window when an
/// utterance ends or is stopped (`speech::Speaker::new`'s notify message).
/// Deliberately in the `WM_USER` range, not `WM_APP_*`: it never leaves this
/// window, so it is not part of the crate-wide `WM_APP_*` registry.
const WM_CARD_SPEECH_END: u32 = WM_USER + 0x77;

/// #255: a preview command (Do it, Cancel, Edit) posted by a preview child
/// control's subclass: Enter, Esc, or a click-away `WM_KILLFOCUS`. `wparam`
/// is the command id, `lparam` the preview generation that was live when it
/// was posted. A real button click still arrives as `WM_COMMAND`. The tag is
/// why this is its own message: a `WM_COMMAND` `lparam` already holds the
/// button's `HWND`. A post that outlives its preview (the focused control's
/// own destruction sends `WM_KILLFOCUS`, so replacing a preview posts a
/// cancel that is only pumped after the new one is showing) carries an old
/// generation and is dropped instead of closing the new preview.
const WM_CARD_PREVIEW_COMMAND: u32 = WM_USER + 0x78;

const CLASS_NAME: &str = "Wingman.Card.Window.7f3c1a9e";
/// Rule 9: the preview state's real-Win32 test creates an actual `HWND` with
/// real child controls, so it gets its own window class rather than sharing
/// the production one, the same way `single_instance`'s test uses its own
/// mutex/class names (commit `011f11a`).
#[cfg(any(test, debug_assertions))]
const TEST_CLASS_NAME: &str = "Wingman.Card.Window.7f3c1a9e.Test";

static CLASS_INIT: Once = Once::new();
static CLASS_OK: OnceLock<bool> = OnceLock::new();
#[cfg(any(test, debug_assertions))]
static TEST_CLASS_INIT: Once = Once::new();
#[cfg(any(test, debug_assertions))]
static TEST_CLASS_OK: OnceLock<bool> = OnceLock::new();

fn ensure_class_registered(instance: HINSTANCE) -> bool {
    CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance, CLASS_NAME) };
        let _ = CLASS_OK.set(ok);
    });
    CLASS_OK.get().copied().unwrap_or(false)
}

#[cfg(any(test, debug_assertions))]
fn ensure_test_class_registered(instance: HINSTANCE) -> bool {
    TEST_CLASS_INIT.call_once(|| {
        let ok = unsafe { register_class(instance, TEST_CLASS_NAME) };
        let _ = TEST_CLASS_OK.set(ok);
    });
    TEST_CLASS_OK.get().copied().unwrap_or(false)
}

unsafe fn register_class(instance: HINSTANCE, class_name_str: &str) -> bool {
    let class_name = wide_z(class_name_str);
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
        // We paint the entire client area ourselves (double-buffered) and
        // return 1 from WM_ERASEBKGND, so no background brush is needed.
        hbrBackground: HBRUSH(std::ptr::null_mut()),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: PCWSTR(class_name.as_ptr()),
        hIconSm: Default::default(),
    };
    RegisterClassExW(&wc) != 0
}

/// The window procedure for every card window. It is a thin shim: it stashes
/// the `CardInner` pointer passed via `CreateWindowExW`'s `lpparam` into
/// `GWLP_USERDATA` on `WM_NCCREATE`, then forwards every subsequent message to
/// [`CardInner::handle_message`], falling back to `DefWindowProcW` for
/// anything that method does not claim.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        let cs = &*(lparam.0 as *const CREATESTRUCTW);
        if !cs.lpCreateParams.is_null() {
            SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
        }
        return DefWindowProcW(hwnd, msg, wparam, lparam);
    }

    let ptr = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut CardInner;
    if !ptr.is_null() {
        let inner = &mut *ptr;
        if let Some(result) = inner.handle_message(msg, wparam, lparam) {
            return result;
        }
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

// ---------------------------------------------------------------------------
// Theme
// ---------------------------------------------------------------------------

struct Palette {
    bg: u32,
    border: u32,
    headline: u32,
    detail: u32,
    hint: u32,
    link: u32,
    danger: u32,
}

/// The card's colours for `theme`, from the shared tokens (#362). Light and
/// dark are fixed; high contrast takes the system colours in `sys`.
fn palette_for(theme: Theme, sys: &SysColors) -> Palette {
    let tokens = Tokens::for_theme(theme, sys);
    Palette {
        bg: tokens.surface,
        border: tokens.border,
        headline: tokens.text,
        detail: tokens.text_dim,
        hint: tokens.text_faint,
        link: tokens.link,
        danger: tokens.danger,
    }
}

/// Issue #350: the `(text, background)` colors a preview EDIT field is
/// painted with, so it follows the card's theme instead of the stock EDIT
/// control's fixed white-on-black. Pure so the theme -> color mapping is
/// unit-testable without a real window; the WM_CTLCOLOREDIT handler is the
/// only caller and does the GDI calls (SetTextColor/SetBkColor/brush) this
/// function has no business doing.
fn edit_field_colors(palette: &Palette, theme: Theme, highlight: u32) -> (u32, u32) {
    let look = field_look(palette, theme, highlight);
    (look.text, look.fill)
}

/// How an editable preview field is drawn (#811): a 1 px outlined, rounded
/// box on a slightly lifted fill, with an accent underline while it has focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FieldLook {
    fill: u32,
    border: u32,
    text: u32,
    /// The 2 px line along the bottom edge of the focused field.
    underline: u32,
}

/// The field colours for `theme`. Light lifts the card colour toward white
/// (70 %), dark by 6 %, the way Windows 11 text boxes sit on a surface; the
/// border is half way from the fill to the text colour, which holds 3:1
/// against the fill (WCAG 1.4.11). High contrast is the system colours only:
/// window fill, window-text border and underline.
fn field_look(palette: &Palette, theme: Theme, highlight: u32) -> FieldLook {
    use crate::ui::card_buttons::blend;
    match theme {
        Theme::HighContrast => FieldLook {
            fill: palette.bg,
            border: palette.border,
            text: palette.headline,
            // A 2 px ring in the system highlight colour, distinct from the
            // window-text border (see `paint_field_frame`).
            underline: highlight,
        },
        Theme::Light | Theme::Dark => {
            let lift = if theme == Theme::Light { 70 } else { 6 };
            let fill = blend(palette.bg, 0x00FF_FFFF, lift);
            FieldLook {
                fill,
                border: blend(fill, palette.headline, 50),
                text: palette.headline,
                underline: crate::ui::card_buttons::system_accent(theme, fill),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Difficulty badge colour
// ---------------------------------------------------------------------------

/// Maps a difficulty to the badge's fill colour, via `Difficulty::rank()`
/// (1..=11, Ultra == 11) rather than matching on the variant, per that
/// method's own doc comment. Ranks outside 1..=10 are clamped rather than
/// trusted for the gradient half, since this module must never panic
/// regardless of what upstream hands it.
fn difficulty_color(difficulty: Difficulty) -> u32 {
    let rank = difficulty.rank();
    if rank >= 11 {
        return rgb(BADGE_ULTRA.0, BADGE_ULTRA.1, BADGE_ULTRA.2);
    }
    let level = rank.clamp(1, 10) as f32;
    let (from, to, t) = if level <= 5.0 {
        (BADGE_LOW, BADGE_MID, (level - 1.0) / 4.0)
    } else {
        (BADGE_MID, BADGE_HIGH, (level - 5.0) / 5.0)
    };
    rgb(
        lerp_u8(from.0, to.0, t),
        lerp_u8(from.1, to.1, t),
        lerp_u8(from.2, to.2, t),
    )
}

/// The badge's `(fill, label ink)`. High contrast drops the rank gradient
/// for the system highlight pair (the label still carries the rank); every
/// other theme uses the gradient with whichever ink reads better on it.
fn badge_colors(difficulty: Difficulty, theme: Theme, sys: &SysColors) -> (u32, u32) {
    if theme == Theme::HighContrast {
        return (sys.highlight, sys.highlight_text);
    }
    let fill = difficulty_color(difficulty);
    (fill, badge_text_color(fill))
}

fn lerp_u8(a: u8, b: u8, t: f32) -> u8 {
    let t = t.clamp(0.0, 1.0);
    (a as f32 + (b as f32 - a as f32) * t).round() as u8
}

/// Picks whichever of near-black or white text gives the higher WCAG 2
/// contrast ratio (https://www.w3.org/TR/WCAG21/#dfn-contrast-ratio) against
/// `fill`, rather than thresholding a flat BT.601 luma as before. That flat
/// threshold (luma > 140) could pick the wrong ink: the issue that prompted
/// this rewrite measured white text on the red gradient stop at well under
/// WCAG AA's 4.5:1 for small text, a case the old threshold did not catch.
/// `badge_text_color_contrasts_with_its_fill` below checks every fill colour
/// the badge actually uses (every 1..=10 gradient rank plus Ultra).
fn badge_text_color(fill: u32) -> u32 {
    let fill_luminance = relative_luminance(fill);
    let dark_contrast = contrast_ratio(fill_luminance, relative_luminance(BADGE_INK_DARK));
    let light_contrast = contrast_ratio(fill_luminance, relative_luminance(BADGE_INK_LIGHT));
    if dark_contrast >= light_contrast {
        BADGE_INK_DARK
    } else {
        BADGE_INK_LIGHT
    }
}

// ---------------------------------------------------------------------------
// Fonts
// ---------------------------------------------------------------------------

struct Fonts {
    headline: HFONT,
    body: HFONT,
    /// Caption font: detail text, footer links, the status line and the badge.
    caption: HFONT,
}

impl Fonts {
    fn null() -> Self {
        Fonts {
            headline: HFONT(std::ptr::null_mut()),
            body: HFONT(std::ptr::null_mut()),
            caption: HFONT(std::ptr::null_mut()),
        }
    }

    fn delete(&self) {
        unsafe {
            if !self.headline.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.headline.0));
            }
            if !self.body.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.body.0));
            }
            if !self.caption.0.is_null() {
                let _ = DeleteObject(HGDIOBJ(self.caption.0));
            }
        }
    }
}

/// Builds the headline/body fonts for `dpi` and `text_scale`, deriving from
/// the shell's message font (`SPI_GETNONCLIENTMETRICS`) so the card matches
/// the system. Infallible: any Win32 failure degrades to a stock GUI font
/// rather than panicking or propagating an error.
fn build_fonts(dpi: u32, text_scale: f32) -> Fonts {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW {
            cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32,
            ..Default::default()
        };

        // `SystemParametersInfoForDpi` returns metrics ALREADY scaled for the
        // DPI you ask for. `SystemParametersInfoW` also returns pre-scaled
        // metrics -- for the system DPI, not this monitor's. Either way the
        // font that comes back is in real pixels, so multiplying it by
        // dpi/96 a second time double-scales it: at 250% the card rendered
        // ~2.5x too large, which is exactly what it looked like on screen.
        //
        // So: ask for this monitor's DPI, then apply only the design ratios
        // and the user's text_scale -- never the DPI factor again.
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

        let base = if got {
            ncm.lfMessageFont
        } else {
            // The fallback is expressed at 96 DPI, so this one does need the
            // monitor scale applied.
            let mut lf = fallback_logfont();
            lf.lfHeight = scaled_height(lf.lfHeight, dpi as f32 / 96.0);
            lf
        };

        // Face: Segoe UI Variable Text where installed, else the system
        // message font's own face. Sizes come from the type ramp in
        // `theme.rs`, in px at 96 DPI and scaled for this monitor and the
        // user's text scale (the DPI is applied once, in `type_height_px`).
        let mut face_lf = base;
        if preferred_face_installed() {
            set_face_name(&mut face_lf, PREFERRED_FONT_FACE);
        }
        let ramp = |px: i32, weight: i32| {
            let mut lf = face_lf;
            lf.lfWeight = weight;
            lf.lfHeight = type_height_px(px, dpi, text_scale);
            lf
        };

        // Headline: body strong (14 px semibold). Detail, links, status line
        // and badge: caption (12 px). Fields and buttons: body (14 px).
        let headline_lf = ramp(TYPE_BODY_PX, FW_SEMIBOLD.0 as i32);
        let body_lf = ramp(TYPE_BODY_PX, FW_NORMAL.0 as i32);
        let caption_lf = ramp(TYPE_CAPTION_PX, FW_NORMAL.0 as i32);

        Fonts {
            headline: font_or_stock(&headline_lf),
            body: font_or_stock(&body_lf),
            caption: font_or_stock(&caption_lf),
        }
    }
}

fn set_face_name(lf: &mut windows::Win32::Graphics::Gdi::LOGFONTW, face: &str) {
    lf.lfFaceName = [0; 32];
    // Leave the last unit as the NUL terminator.
    for (i, u) in face.encode_utf16().take(31).enumerate() {
        lf.lfFaceName[i] = u;
    }
}

/// Whether the preferred font family is installed. Asked once per process:
/// the answer does not change while the card is running.
fn preferred_face_installed() -> bool {
    use std::sync::OnceLock;
    static PREFERRED: OnceLock<bool> = OnceLock::new();
    *PREFERRED.get_or_init(|| unsafe {
        use windows::Win32::Graphics::Gdi::{
            EnumFontFamiliesExW, GetDC, ReleaseDC, DEFAULT_CHARSET, LOGFONTW, TEXTMETRICW,
        };
        unsafe extern "system" fn found(
            _lf: *const LOGFONTW,
            _tm: *const TEXTMETRICW,
            _font_type: u32,
            lparam: LPARAM,
        ) -> i32 {
            *(lparam.0 as *mut bool) = true;
            0 // stop at the first match
        }
        let mut lf = LOGFONTW {
            lfCharSet: DEFAULT_CHARSET,
            ..Default::default()
        };
        set_face_name(&mut lf, PREFERRED_FONT_FACE);
        let hdc = GetDC(None);
        let mut hit = false;
        EnumFontFamiliesExW(
            hdc,
            &lf,
            Some(found),
            LPARAM(&mut hit as *mut bool as isize),
            0,
        );
        ReleaseDC(None, hdc);
        hit
    })
}

fn scaled_height(base_height: i32, factor: f32) -> i32 {
    let v = (base_height as f32 * factor).round() as i32;
    if v == 0 {
        if base_height < 0 {
            -1
        } else {
            1
        }
    } else {
        v
    }
}

unsafe fn font_or_stock(lf: &windows::Win32::Graphics::Gdi::LOGFONTW) -> HFONT {
    let f = CreateFontIndirectW(lf);
    if f.0.is_null() {
        HFONT(GetStockObject(DEFAULT_GUI_FONT).0)
    } else {
        f
    }
}

fn fallback_logfont() -> windows::Win32::Graphics::Gdi::LOGFONTW {
    let mut lf = windows::Win32::Graphics::Gdi::LOGFONTW {
        lfHeight: -12,
        ..Default::default()
    };
    for (i, u) in FALLBACK_FONT_FACE.encode_utf16().enumerate() {
        if i < lf.lfFaceName.len() {
            lf.lfFaceName[i] = u;
        }
    }
    lf
}

// ---------------------------------------------------------------------------
// Layout constants (logical pixels at 96 DPI; scaled per-window via `scale`)
// ---------------------------------------------------------------------------

const CARD_WIDTH_DP: i32 = 384;
/// Collapsed headline cap. The prompt budgets a 90-character headline.
/// Content width is `CARD_WIDTH_DP - 2*PADDING_DP` = 384 - 32 = 352dp.
/// The headline is 14 px semibold Segoe UI: from the gallery renders an
/// average glyph is ~6.6dp once `DT_WORDBREAK`'s end-of-line slack is
/// priced in. With the chevron's 24dp reserved that is 328 / 6.6 ~= 50
/// characters per line (THEORY, unverified for wide text: MEASURED 2026-10-16
/// a news-style 50-character line fits one line but a wide one needs 371 px
/// of the 352 available and wraps, see
/// `a_fifty_character_headline_fits_one_collapsed_line`), so two lines already cover a 90-character
/// headline; three lines leave real headroom for wider glyphs and short
/// words that push wrapping earlier, so a full 90-character headline is
/// never silently `DT_END_ELLIPSIS`'d. (The card is as wide as it is so
/// the larger type keeps the 50-character budget the prompts promise.)
pub(crate) const HEADLINE_MAX_LINES: i32 = 3;
/// #885: the most lines of detail a setup card shows; the rest ends in an
/// ellipsis, so a long config path cannot make the card taller than the screen.
const SETUP_DETAIL_MAX_LINES: i32 = 6;
/// Characters that fit on one collapsed headline line once the chevron's
/// width is reserved: (352dp content - 24dp chevron) / 6.6dp per glyph = 50.
/// A budget for callers that must fit text in `HEADLINE_MAX_LINES`
/// (`actions::page_summary`), not a measurement.
pub(crate) const HEADLINE_CHARS_PER_LINE: usize = 50;
/// Side length of the spinner/glyph square inside the pending card (issue
/// #354: smaller than the old spinner-only card's full size, since the
/// pending card now also carries a status line below the glyph).
const PENDING_SIZE_DP: i32 = 40;
/// Full width of the pending card, once it carries status text -- matches
/// the Collapsed/Expanded card width so the pending card no longer looks
/// like a different surface from the states that immediately replace it.
const PENDING_WIDTH_DP: i32 = 280;
/// Vertical gap between the glyph and the status line beneath it.
const PENDING_TEXT_GAP_DP: i32 = 8;
const WHEEL_SCROLL_DP: i32 = 48;

// -- "there is more" cues (#353) ----------------------------------------

/// Width reserved at the right of a collapsed headline for the chevron that
/// says "click for the rest".
const CHEVRON_RESERVE_DP: i32 = 24;
/// Half the chevron's width, in dp.
const CHEVRON_HALF_DP: i32 = 5;
/// Extra height, beyond one body line, of the fade at the bottom of an
/// overflowing expanded card.
const FADE_EXTRA_DP: i32 = 10;

/// Whether a card in `state` with this `detail` has more to show on click:
/// the one rule behind the chevron, the hand cursor and the layout space
/// reserved for the chevron.
fn card_has_more_to_show(state: CardState, detail_is_empty: bool) -> bool {
    state == CardState::Collapsed && !detail_is_empty
}

/// How opaque the fade is on `row` of `rows`, in percent: from nearly clear
/// at the top to solid at the last row, so the text underneath dissolves.
fn fade_percent(row: i32, rows: i32) -> u32 {
    if rows <= 0 {
        return 100;
    }
    (((row + 1) * 100) / rows).clamp(0, 100) as u32
}

/// `from` moved `pct` percent of the way to `to`, per channel (`COLORREF`s).
fn blend_colors(from: u32, to: u32, pct: u32) -> u32 {
    let t = pct.min(100);
    let mix = |shift: u32| {
        let a = (from >> shift) & 0xFF;
        let b = (to >> shift) & 0xFF;
        ((a * (100 - t) + b * t + 50) / 100) << shift
    };
    mix(0) | mix(8) | mix(16)
}

/// Where an expanded card scrolls to for a key, or `None` when the key does
/// not scroll. Down/Up move a line, PageDown/PageUp a page, Home/End the
/// ends; the result is always inside `0..=max`.
/// `true` while the virtual key is physically down.
fn key_is_down(vk: u16) -> bool {
    unsafe { GetKeyState(vk as i32) < 0 }
}

fn scroll_target_for_key(vk: u16, offset: i32, max: i32, line: i32, page: i32) -> Option<i32> {
    let target = match vk {
        k if k == VK_DOWN.0 => offset + line,
        k if k == VK_UP.0 => offset - line,
        k if k == VK_NEXT.0 => offset + page,
        k if k == VK_PRIOR.0 => offset - page,
        k if k == VK_HOME.0 => 0,
        k if k == VK_END.0 => max,
        _ => return None,
    };
    Some(target.clamp(0, max.max(0)))
}

/// The footer's clickable links, in Tab order (#606).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FooterLink {
    CopyDetails,
    ReadAloud,
    Copy,
}

/// Where Tab (or Shift+Tab with `backwards`) moves the focus among `links`.
/// Wraps around; a `current` that is not in `links` starts from the end the
/// direction came from; no links means no focus.
fn next_footer_focus(
    links: &[FooterLink],
    current: Option<FooterLink>,
    backwards: bool,
) -> Option<FooterLink> {
    if links.is_empty() {
        return None;
    }
    let len = links.len();
    let at = current.and_then(|c| links.iter().position(|l| *l == c));
    let next = match (at, backwards) {
        (None, false) => 0,
        (None, true) => len - 1,
        (Some(i), false) => (i + 1) % len,
        (Some(i), true) => (i + len - 1) % len,
    };
    Some(links[next])
}

/// Draws a solid frame `thickness` px wide just inside `rect`, in `color`.
unsafe fn frame_rect(hdc: HDC, rect: &RECT, thickness: i32, color: u32) {
    let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(color));
    let t = thickness.max(1);
    let edges = [
        RECT {
            left: rect.left,
            top: rect.top,
            right: rect.right,
            bottom: rect.top + t,
        },
        RECT {
            left: rect.left,
            top: rect.bottom - t,
            right: rect.right,
            bottom: rect.bottom,
        },
        RECT {
            left: rect.left,
            top: rect.top + t,
            right: rect.left + t,
            bottom: rect.bottom - t,
        },
        RECT {
            left: rect.right - t,
            top: rect.top + t,
            right: rect.right,
            bottom: rect.bottom - t,
        },
    ];
    for edge in &edges {
        FillRect(hdc, edge, brush);
    }
    let _ = DeleteObject(HGDIOBJ(brush.0));
}

/// The message of a preview that rejected an edit. High contrast has one
/// ink for everything (#648), so the message says it is an error in words.
fn preview_error_title(message: &str, theme: Theme) -> String {
    if theme == Theme::HighContrast && !message.starts_with("Error") {
        format!("Error: {message}")
    } else {
        message.to_string()
    }
}

// -- Difficulty badge (Collapsed / Expanded only) -----------------------

/// Distance from the card's outer edge to the badge's outer edge. Smaller
/// than `PADDING_DP` on purpose: the badge nestles into the corner, mostly
/// inside the existing padding whitespace, rather than adding a second ring
/// of margin around it.
const BADGE_EDGE_MARGIN_DP: i32 = 8;
/// Horizontal/vertical text inset inside the badge shape.
const BADGE_PAD_X_DP: i32 = 5;
const BADGE_PAD_Y_DP: i32 = 3;
/// Floor on both badge dimensions so a single-digit label (e.g. "1") still
/// reads as a deliberate shape rather than a sliver -- this also makes
/// single-character badges come out as circles (width == height) while
/// two-character ones ("10") and "U" come out as pills.
const BADGE_MIN_DIAMETER_DP: i32 = 18;
/// Minimum clearance kept between the badge and any headline text next to
/// it, on top of the badge's own width.
const BADGE_TEXT_GAP_DP: i32 = 6;

const TIMER_ANIM: usize = 1;
const TIMER_DISMISS: usize = 2;
/// Issue #354: fires once, `STILL_WORKING_DELAY_MS` after the card enters
/// `PendingStage::AskingModel`, and moves the status line to "Still
/// working." Armed only for as long as the card is actually Pending and in
/// that sub-stage -- see `CardInner::set_pending_stage` and
/// `CardInner::kill_timers` (AGENTS.md rule 5: nothing runs while idle).
const TIMER_STILL_WORKING: usize = 3;
/// Issue #531: fires once, `COPIED_FEEDBACK_MS` after a successful "Copy"
/// click, and puts the footer label back. Armed only while the card is
/// showing and killed by `kill_timers` (AGENTS.md rule 5).
const TIMER_COPIED: usize = 4;
const COPIED_FEEDBACK_MS: u32 = 1500;
/// Gap between the footer's links, in dp.
const FOOTER_LINK_GAP_DP: i32 = 16;

/// Issue #531: the text "Copy" puts on the clipboard: the headline, a blank
/// line, then the detail, exactly what the card shows.
fn answer_clipboard_text(headline: &str, detail: &str) -> String {
    if detail.is_empty() {
        headline.to_string()
    } else {
        format!("{headline}\r\n\r\n{detail}")
    }
}

/// Issue #531: splits the pinned footer row into the "Read aloud" link (when
/// the card has one) and the "Copy" link beside it. Each link is as wide as
/// its measured label; whatever does not fit is clipped to the footer, never
/// pushed outside it.
fn footer_links_layout(
    footer: RECT,
    read_aloud_w: Option<i32>,
    copy_w: i32,
    gap: i32,
) -> (Option<RECT>, RECT) {
    let mut left = footer.left;
    let read_aloud = read_aloud_w.map(|w| {
        let r = RECT {
            right: (left + w.max(0)).min(footer.right),
            left,
            ..footer
        };
        left = (r.right + gap).min(footer.right);
        r
    });
    let copy = RECT {
        right: (left + copy_w.max(0)).min(footer.right),
        left,
        ..footer
    };
    (read_aloud, copy)
}

/// Issue #531: `C` (so also Ctrl+C) copies on an expanded answer card.
/// Not for auto-repeat (lparam bit 30), and not with Shift or Alt: plain C
/// and Ctrl+C only.
fn is_copy_key(vk: u16, lparam: isize, shift: bool, alt: bool) -> bool {
    vk == 0x43 && lparam & 0x4000_0000 == 0 && !shift && !alt
}

/// #376: Ctrl+Shift+C copies the card as an image. Not for auto-repeat, and
/// not with Alt. Plain C and Ctrl+C stay the text copy.
fn is_copy_image_key(vk: u16, lparam: isize, ctrl: bool, shift: bool, alt: bool) -> bool {
    vk == 0x43 && lparam & 0x4000_0000 == 0 && ctrl && shift && !alt
}

/// Gdi hands back BGRA with alpha left at zero. The clipboard image wants
/// RGBA, opaque: the card is a solid surface.
fn bgra_to_opaque_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bgra.len());
    for px in bgra.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    out
}

/// Issue #531: a link squeezed to nothing counts as not there.
fn rect_has_area(r: RECT) -> bool {
    r.right > r.left && r.bottom > r.top
}

/// What the "Copy" link says after a copy attempt.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CopyFeedback {
    None,
    Copied,
    Failed,
}
/// A rotation needs ~16-33ms/frame to read as smooth; the old text-dot
/// animation could get away with much slower ticks, but a spinner cannot.
const ANIM_INTERVAL_MS: u32 = 20;
/// A last resort if the worker never reports back. Derived from the provider
/// timeouts for a typical chain. A longer chain can outlast it, which is
/// recoverable: a press while busy shows the working card again (#723).
const PENDING_SAFETY_TIMEOUT_SECS: u32 =
    crate::provider::worst_case_run_secs(crate::provider::TYPICAL_CHAIN_LEN) as u32;
/// How long "Asking {model}..." stays up before the line changes to "Still
/// working." -- issue #354's "after a few seconds".
const STILL_WORKING_DELAY_MS: u32 = 4000;

/// Issue #354: the one-line Win32 read behind the reduce-motion decision.
/// Kept trivial on purpose (nothing here to get wrong) -- the actual
/// decision of whether to animate is the pure, unit-tested
/// `pending_status::pending_glyph_should_spin`. Falls back to `true`
/// (animate) on any Win32 failure, matching this module's existing degrade
/// pattern of "keep the pre-existing behaviour" rather than inventing a new
/// failure mode.
fn client_area_animation_enabled() -> bool {
    // SPI_GETCLIENTAREAANIMATION writes a Win32 BOOL (a plain 4-byte int, 0
    // or nonzero) through pvParam -- a raw i32 avoids depending on exactly
    // which module this `windows` crate version exports its BOOL type from.
    let mut enabled: i32 = 1;
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut enabled as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    enabled != 0
}

// -- Spinner (pending state) -------------------------------------------

/// Pen width for both the dim ring and the bright sweep, in logical pixels.
const SPINNER_STROKE_DP: i32 = 4;
/// Degrees the sweep advances per `TIMER_ANIM` tick. At `ANIM_INTERVAL_MS`
/// (20ms) this is 360 / 6 * 20ms = 1200ms per full revolution -- a typical
/// indeterminate-spinner cadence.
const SPINNER_DEGREES_PER_FRAME: f32 = 6.0;
/// Arc length of the bright sweep segment.
const SPINNER_SWEEP_DEG: f32 = 100.0;

// ---------------------------------------------------------------------------
// CardInner: the real state; addressed by raw pointer from GWLP_USERDATA
// ---------------------------------------------------------------------------

struct CardInner {
    hwnd: HWND,
    /// The module instance the window (and every preview child control) was
    /// created with. Stashed here (rather than re-fetched with
    /// `GetModuleHandleW`) so `create_preview_controls` creates its EDIT and
    /// BUTTON children against the exact same instance `CreateWindowExW`
    /// used for the card's own window.
    instance: HINSTANCE,
    state: CardState,
    headline: String,
    detail: String,
    /// `None` renders exactly as before this feature existed: no badge, no
    /// reserved layout space. Cleared by `show_error` and by `show_pending`
    /// so a stale badge can never linger onto a state that shouldn't have
    /// one.
    difficulty: Option<Difficulty>,
    dpi: u32,
    theme: Theme,
    /// The system colours high contrast paints with, read together with
    /// `theme` (and again on every theme message).
    sys: SysColors,
    /// Set by the UI gallery's `--theme`: `theme` and `sys` are simulated
    /// and no theme message may replace them.
    theme_forced: bool,
    /// Set by the UI gallery's `--dpi`: `dpi` is simulated and neither the
    /// monitor the card moves to nor `WM_DPICHANGED` may replace it.
    dpi_forced: bool,
    fonts: Fonts,
    /// User-tunable multiplier applied to every font size. Set via
    /// [`Card::set_text_scale`]; defaults to 1.0.
    text_scale: f32,
    /// Issue #109: where the card sits (`ui.card_corner`).
    corner: CardCorner,
    /// Issue #109: which monitor it appears on (`ui.card_monitor`).
    monitor: MonitorChoice,
    /// Issue #109: for `CardCorner::FollowMouse`, the pointer position the
    /// card is anchored to. Taken when Pending or Collapsed is laid out and
    /// reused by Expanded and Preview, so a card the user is reading does
    /// not chase the mouse.
    follow_anchor: POINT,
    /// Issue #109: set when a preview is shown from Hidden, so its layout
    /// reads the pointer and the pointer's monitor afresh instead of using
    /// the position an earlier card left behind. Consumed by `layout_preview`.
    place_from_cursor: bool,
    anim_frame: u32,
    scroll_offset: i32,
    scroll_max: i32,
    /// Height of the scrollable area in Expanded, for PageUp/PageDown (#353).
    scroll_page: i32,
    /// True while WS_EX_NOACTIVATE has been removed (i.e. while Expanded).
    ex_noactivate_removed: bool,
    /// Live state for `CardState::Preview`, `None` in every other state.
    /// `Some` for exactly as long as the preview's child controls exist --
    /// see `leave_preview_if_active`, the single choke point every path out
    /// of Preview goes through.
    preview: Option<PreviewUi>,
    /// The `Confirmed<Value>` from the most recent "Do it" / Enter, read
    /// (and cleared) by `Card::take_confirmed`. Deliberately NOT stored
    /// inside `PreviewUi`: `preview_do_it` closes the preview (via
    /// `close_preview` -> `hide` -> `leave_preview_if_active`) in the same
    /// call that produces the `Confirmed`, and `leave_preview_if_active`
    /// unconditionally drops `PreviewUi` -- a value stored there would be
    /// destroyed before any caller could ever read it back.
    last_confirmed: Option<crate::ui::confirm::Confirmed<serde_json::Value>>,
    /// Issue #39: the owner window [`Card::set_owner`] was told about, if
    /// any -- `preview_do_it`/`preview_cancel` `PostMessageW`
    /// [`WM_APP_PREVIEW_DECIDED`] here so `App::on_preview_decided` can
    /// call [`Card::take_confirmed`] and, for "Do it", actually run an
    /// executor. `None` until `set_owner` is called (every production
    /// caller does so once, right after `Card::new`); a preview shown with
    /// no owner set still works, it just has nowhere to notify -- the same
    /// degrade `Tray`'s own best-effort Win32 calls use elsewhere.
    owner: Option<HWND>,
    /// Issue #225: `true` from the moment a preview is shown until a
    /// decision has been reported for it. "Do it" and Cancel clear it and
    /// notify the owner themselves; anything else that tears the preview
    /// down (hiding the card, starting another action, opening Settings,
    /// pausing, or replacing this preview with a different one) leaves it
    /// set, and [`CardInner::leave_preview_if_active`] then reports the
    /// abandonment so the owner can drop whatever it was holding.
    ///
    /// Without this, a preview could be destroyed with no
    /// [`WM_APP_PREVIEW_DECIDED`] ever posted, `App`'s `pending_review` /
    /// `pending_form_fill` would stay `Some`, and a later "Do it" on a
    /// different preview could run the abandoned action instead of the one
    /// the user actually confirmed.
    /// Issue #347: `true` while the currently-showing Collapsed card is a
    /// "you need to configure something" prompt whose click should open
    /// Settings instead of expanding for more detail. Set only by
    /// [`Card::show_settings_needed`]; cleared by every other path into
    /// Collapsed (`show_collapsed`, used by both `show_answer` and
    /// `show_error`) so it can never linger onto an unrelated card.
    open_settings_on_click: bool,
    /// Issue #425: `true` while the currently-showing card is an error card
    /// (set only by [`Card::show_error`]), i.e. one that has a "Copy
    /// details" affordance to offer once Expanded. Cleared by every other
    /// path into Collapsed (`show_collapsed`, used by both `show_answer` and
    /// `show_error`) so an answer card can never inherit it.
    copy_details_available: bool,
    /// The "Copy details" affordance's hit-test rect while Expanded, in the
    /// same client-window coordinates `WM_LBUTTONDOWN`'s `lparam` arrives
    /// in -- a pinned footer row, so unlike the headline/detail text it is
    /// NOT affected by `scroll_offset`, and hit-testing it needs no scroll
    /// math. `None` whenever there is nothing to show (not an error card,
    /// or not Expanded yet); recomputed by every `layout_expanded` call.
    copy_details_rect: Option<RECT>,
    /// Issue #77: `true` while the card is an answer card (set only by
    /// [`Card::show_answer`]), i.e. one with a "Read aloud" footer to offer
    /// once Expanded. Mutually exclusive with `copy_details_available`:
    /// both share the one pinned footer row.
    read_aloud_available: bool,
    /// The "Read aloud" footer's hit-test rect while Expanded, same
    /// coordinates and same no-scroll-math reasoning as `copy_details_rect`.
    read_aloud_rect: Option<RECT>,
    /// Issue #531: `true` while the card is an answer card, i.e. one with a
    /// "Copy" link in the footer once Expanded.
    copy_available: bool,
    /// #606: the footer link Tab has moved to, drawn with a focus rectangle
    /// and run by Enter or Space. `None` until Tab is pressed.
    focus_link: Option<FooterLink>,
    /// The "Copy" link's hit-test rect while Expanded, same coordinates as
    /// `read_aloud_rect`.
    copy_rect: Option<RECT>,
    /// Issue #531: for `COPIED_FEEDBACK_MS` after a copy attempt the link
    /// reads "Copied" or "Couldn't copy".
    copy_feedback: CopyFeedback,
    /// The system voice, created on the first "Read aloud" click (so a card
    /// that is never read aloud never touches COM or SAPI). Lives on the
    /// main STA thread, like the card.
    speaker: Option<Speaker>,
    /// `true` from the click that starts the voice until the voice ends or
    /// is stopped; drives the footer label ("Read aloud" / "Stop reading").
    reading: bool,
    /// The voice's stream number for the utterance `reading` refers to, so
    /// the end of a replaced utterance is not taken for the end of this one.
    reading_stream: u32,
    preview_decision_pending: bool,
    /// Issue #225: incremented on every [`CardInner::show_preview`], and
    /// posted as `WM_APP_PREVIEW_DECIDED`'s `WPARAM` so the owner can tell
    /// WHICH preview a decision belongs to.
    ///
    /// The notification is a `PostMessageW`, so it is delivered after the
    /// call that triggered it has returned. Replacing a live preview posts
    /// the old one's abandonment and then arms the new one in the same turn;
    /// without a generation the owner would process that abandonment later
    /// and clear the state belonging to the preview now on screen.
    preview_generation: u32,
    /// Issue #350: the brush WM_CTLCOLOREDIT returns for preview EDIT
    /// fields, matching `self.theme`. Created lazily on first use and cached
    /// rather than created per-paint (WM_CTLCOLOREDIT fires on every
    /// keystroke and repaint), and deleted in WM_NCDESTROY, or earlier when
    /// a live theme change (`apply_theme`, #431) makes its color stale.
    edit_bg_brush: HBRUSH,
    /// Issue #354: which sub-stage the Pending state is in. Meaningless
    /// outside `CardState::Pending`; set to whatever stage the caller passed
    /// `show_pending` by every `show_pending` call, alongside every other
    /// pending-only field.
    pending_stage: PendingStage,
    /// Issue #354: whether Windows' "Animation effects" setting was on the
    /// last time `show_pending` queried it (`client_area_animation_enabled`,
    /// checked once per Pending session, not re-polled: its
    /// `WM_SETTINGCHANGE` is not handled, only the theme's is). With
    /// it `false`, `paint_pending` draws a static glyph and `show_pending`
    /// never arms `TIMER_ANIM`.
    animations_enabled: bool,
}

impl CardInner {
    fn scale(&self, dp: i32) -> i32 {
        (dp * self.dpi as i32 + 48) / 96
    }

    fn rebuild_fonts(&mut self) {
        let fresh = build_fonts(self.dpi, self.text_scale);
        let old = std::mem::replace(&mut self.fonts, fresh);
        old.delete();
    }

    /// Clamps and applies a new text-scale multiplier, then rebuilds fonts
    /// and re-runs the current state's layout so the change is visible
    /// immediately, even if the card is already on screen.
    fn set_text_scale(&mut self, scale: f32) {
        // Clamped to roughly half to double the default so a bad config
        // value cannot make the card unreadable (too small) or enormous
        // (too large).
        self.text_scale = scale.clamp(0.5, 2.0);
        self.rebuild_fonts();
        self.relayout_current_state();
    }

    // -- show/hide -----------------------------------------------------

    /// `stage` is the caller's own honest description of what is happening
    /// right as the card appears (issue #354 follow-up review, finding 1):
    /// there is no longer a generic "Capturing" placeholder shown here and
    /// then immediately overwritten -- see `PendingStage`'s doc comment for
    /// why that variant was removed rather than kept and fixed. Every
    /// caller passes the stage that is actually true at THIS call, e.g.
    /// `App::begin_model_action` already knows the mode-aware model label
    /// before capture even runs, so the three model-backed flows start
    /// directly at `AskingModel`.
    fn show_pending(&mut self, stage: PendingStage) {
        self.leave_preview_if_active();
        self.reset_activation_and_timers();
        // `headline`/`detail` stay unused by Pending (issue #354's status
        // line is driven by `pending_stage`/`pending_status_text`, painted
        // separately in `paint_pending`, never stored back into `headline`).
        self.headline.clear();
        self.detail.clear();
        self.difficulty = None; // pending never shows a badge; keep state tidy
        self.state = CardState::Pending;
        self.anim_frame = 0;
        self.scroll_offset = 0;
        self.scroll_max = 0;
        // Issue #354: queried once per Pending session, not on every frame
        // -- see the field's doc comment.
        self.animations_enabled = client_area_animation_enabled();

        unsafe {
            if pending_glyph_should_spin(self.animations_enabled) {
                let _ = SetTimer(Some(self.hwnd), TIMER_ANIM, ANIM_INTERVAL_MS, None);
            }
            let _ = SetTimer(
                Some(self.hwnd),
                TIMER_DISMISS,
                PENDING_SAFETY_TIMEOUT_SECS.saturating_mul(1000),
                None,
            );
        }

        // Applied BEFORE layout: `layout_pending` sizes the card off the
        // current status text, so `pending_stage` must already be `stage`
        // (not the previous Pending session's leftover value) by the time
        // it runs -- shares the "still working" timer arm-or-kill and
        // repaint logic with `set_pending_stage`, so starting straight at
        // `AskingModel` arms the same countdown a later transition into it
        // would.
        self.apply_pending_stage(stage);
        self.layout_pending();
        self.reveal();
    }

    /// Issue #354: moves the Pending card to a new sub-stage -- called by
    /// this module's own `TIMER_STILL_WORKING` handler (`StillWorking`) once
    /// the card is already showing. A no-op outside `CardState::Pending`: a
    /// stale call arriving after the card has already moved on (answer,
    /// error, hidden) must never resurrect a pending-only field or re-arm a
    /// pending-only timer.
    fn set_pending_stage(&mut self, stage: PendingStage) {
        if self.state != CardState::Pending {
            return;
        }
        self.apply_pending_stage(stage);
    }

    /// Shared by `show_pending` (the card's very first stage) and
    /// `set_pending_stage` (a later transition): stores `stage`, arms or
    /// kills the "still working" countdown to match, and repaints the
    /// status line. No `CardState::Pending` guard here -- both callers
    /// already enforce it themselves, one implicitly (it just set the state
    /// to `Pending`), one explicitly.
    fn apply_pending_stage(&mut self, stage: PendingStage) {
        self.pending_stage = stage;
        // Only `AskingModel` starts the "still working" countdown; killing
        // any previous one first means a caller that applies this twice in
        // a row (e.g. the router retrying a provider) never stacks timers.
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_STILL_WORKING);
        }
        if matches!(self.pending_stage, PendingStage::AskingModel { .. }) {
            unsafe {
                let _ = SetTimer(
                    Some(self.hwnd),
                    TIMER_STILL_WORKING,
                    STILL_WORKING_DELAY_MS,
                    None,
                );
            }
        }
        self.set_window_text_for_pending_stage();
        self.invalidate();
    }

    /// `SetWindowTextW` with the current pending status line, so basic
    /// assistive tech reading the window's own accessible name sees the
    /// same text a sighted user reads off the card (issue #354; full UIA
    /// provider wiring is #119, out of scope here).
    fn set_window_text_for_pending_stage(&self) {
        self.set_window_text(&pending_status_text(&self.pending_stage));
    }

    /// `SetWindowTextW` with `text`. Generalizes
    /// `set_window_text_for_pending_stage` (issue #354) to every other
    /// state the card can show (issue #356): the window's own accessible
    /// name should always match whatever a sighted user currently reads off
    /// the card, not stay the static "Wingman" set once at `Card::create`.
    /// No "Wingman:" prefix, same as the Pending precedent -- the app's own
    /// identity is a separate concern assistive tech already gets from the
    /// window/process, not something the window's own name needs to repeat.
    fn set_window_text(&self, text: &str) {
        let text = wide_z(text);
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                self.hwnd,
                PCWSTR(text.as_ptr()),
            );
        }
    }

    fn show_collapsed(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
    ) {
        self.show_collapsed_with(headline, detail, auto_dismiss_secs, difficulty, false);
    }

    /// `open_settings` must be known before the layout runs: a setup card
    /// shows its detail line (#885), so its height depends on it.
    fn show_collapsed_with(
        &mut self,
        headline: &str,
        detail: &str,
        auto_dismiss_secs: u32,
        difficulty: Option<Difficulty>,
        open_settings: bool,
    ) {
        self.leave_preview_if_active();
        self.reset_activation_and_timers();
        self.headline = headline.to_string();
        self.detail = detail.to_string();
        self.difficulty = difficulty;
        self.state = CardState::Collapsed;
        self.scroll_offset = 0;
        self.scroll_max = 0;
        // Issue #356: the window's accessible name follows the card, same
        // reasoning as `set_window_text_for_pending_stage` (#354) -- covers
        // show_answer, show_error, show_error_with_details and
        // show_settings_needed, every one of which funnels through here.
        // #885: a setup card's detail is on screen, so Narrator reads it too.
        if open_settings && !detail.is_empty() {
            self.set_window_text(&format!("{headline}. {detail}"));
        } else {
            self.set_window_text(headline);
        }
        // Issue #347: every ordinary show_answer/show_error call starts a
        // plain card, not a "click to open Settings" prompt -- only
        // `Card::show_settings_needed` sets this, right after this call
        // returns (through `show_collapsed_with`).
        self.open_settings_on_click = open_settings;
        // Issue #425: same reasoning -- only `Card::show_error` sets this,
        // right after this call returns, so an answer card never inherits a
        // "Copy details" affordance for an error it isn't.
        self.copy_details_available = false;
        self.copy_details_rect = None;
        // Issue #77: likewise, only `Card::show_answer` turns "Read aloud"
        // on, right after this call returns.
        self.read_aloud_available = false;
        self.read_aloud_rect = None;
        // Issue #531: same for "Copy"; `reset_activation_and_timers` above
        // already killed its timer.
        self.copy_available = false;
        self.copy_rect = None;
        self.copy_feedback = CopyFeedback::None;
        self.focus_link = None;

        unsafe {
            if auto_dismiss_secs > 0 {
                let _ = SetTimer(
                    Some(self.hwnd),
                    TIMER_DISMISS,
                    auto_dismiss_secs.saturating_mul(1000),
                    None,
                );
            }
        }

        self.layout_collapsed();
        self.reveal();
    }

    fn hide(&mut self) {
        self.leave_preview_if_active();
        self.stop_reading();
        self.kill_timers();
        self.copy_feedback = CopyFeedback::None;
        self.set_noactivate(true);
        self.state = CardState::Hidden;
        // Issue #729: nothing is showing, so nothing needs a click-away hook.
        // Every hide goes through here, the auto-dismiss timer included.
        crate::dismiss::disarm();
        // Issue #356: no headline is showing any more, so the accessible
        // name reverts to the plain app name set at `Card::create`, rather
        // than leaking the last-shown headline into a window nothing is
        // displaying.
        self.set_window_text("Wingman");
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
        }
    }

    fn reveal(&mut self) {
        unsafe {
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
        self.invalidate();
    }

    fn reset_activation_and_timers(&mut self) {
        // Issue #77: a new card replaces the old one, so its voice stops too.
        self.stop_reading();
        self.kill_timers();
        self.set_noactivate(true);
    }

    fn kill_timers(&self) {
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_ANIM);
            let _ = KillTimer(Some(self.hwnd), TIMER_DISMISS);
            // Issue #354: killed on every exit from Pending (this is called
            // from `reset_activation_and_timers`, which every `show_*`
            // entry point runs first, and from `hide()`), so the "Asking
            // ..." -> "Still working." countdown never outlives the card
            // state it belongs to.
            let _ = KillTimer(Some(self.hwnd), TIMER_STILL_WORKING);
            let _ = KillTimer(Some(self.hwnd), TIMER_COPIED);
        }
    }

    fn scroll_key_target(&self, vk: u16) -> Option<i32> {
        let line = self.line_height(self.fonts.caption).max(1);
        let page = (self.scroll_page - line).max(line);
        scroll_target_for_key(vk, self.scroll_offset, self.scroll_max, line, page)
    }

    /// Paints a preview button for `WM_DRAWITEM` (#351): "Do it" in the
    /// system accent, the rest as outlines, in the card's theme, or in the
    /// system colors under high contrast.
    fn draw_preview_button(&self, lparam: LPARAM) {
        use crate::ui::card_buttons as btn;
        let dis = lparam.0 as *const windows::Win32::UI::Controls::DRAWITEMSTRUCT;
        if dis.is_null() {
            return;
        }
        let dis = unsafe { &*dis };
        let palette = self.palette();
        let high_contrast = self.button_high_contrast();
        let surface = btn::Surface {
            bg: palette.bg,
            border: palette.border,
            text: palette.headline,
            hint: palette.hint,
        };
        let role = if dis.CtlID == ID_PREVIEW_DO_IT as u32 {
            btn::Role::Primary
        } else {
            btn::Role::Secondary
        };
        let hovered = button_is_hovered(dis.hwndItem);
        let look = btn::look(
            role,
            btn::state_of(dis, hovered),
            &surface,
            btn::system_accent(self.theme, palette.bg),
            high_contrast.as_ref(),
        );
        let text = window_text(dis.hwndItem);
        unsafe {
            btn::draw(
                dis,
                &text,
                self.fonts.body,
                &look,
                palette.bg,
                self.scale(CONTROL_RADIUS_DP),
                self.scale(1),
            );
        }
    }

    /// The card's colours in its current theme.
    fn palette(&self) -> Palette {
        palette_for(self.theme, &self.sys)
    }

    /// The colours for the preview buttons while high contrast is on, from
    /// the same system colours the card paints with (window and window text,
    /// not button face and text, so a custom scheme cannot split the two).
    /// The live card and the gallery's forced theme share this path.
    fn button_high_contrast(&self) -> Option<crate::ui::card_buttons::HighContrast> {
        (self.theme == Theme::HighContrast).then_some(crate::ui::card_buttons::HighContrast {
            face: self.sys.window,
            text: self.sys.window_text,
            highlight: self.sys.highlight,
            highlight_text: self.sys.highlight_text,
            gray_text: self.sys.gray_text,
        })
    }

    /// Re-reads the theme and the system colours, then repaints. Runs on
    /// every message that can change them. It repaints even when the theme
    /// is the same: switching between two high contrast schemes changes the
    /// colours and not the theme.
    fn refresh_theme(&mut self) {
        if self.theme_forced {
            return;
        }
        self.theme = detect_theme();
        self.sys = SysColors::read();
        self.repaint_for_theme();
    }

    /// Drops the cached EDIT brush (it holds the old theme's color;
    /// `WM_CTLCOLOREDIT` rebuilds it) and repaints the card and its preview
    /// fields.
    fn repaint_for_theme(&mut self) {
        if !self.edit_bg_brush.0.is_null() {
            unsafe {
                let _ = DeleteObject(HGDIOBJ(self.edit_bg_brush.0));
            }
            self.edit_bg_brush = HBRUSH(std::ptr::null_mut());
        }
        unsafe {
            use windows::Win32::Graphics::Gdi::{RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE};
            let _ = RedrawWindow(
                Some(self.hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }

    fn invalidate(&self) {
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
        }
    }

    /// Adds or removes `WS_EX_NOACTIVATE`.
    ///
    /// Tension noted in the spec: the card is created `WS_EX_NOACTIVATE` so
    /// showing it never steals focus (important for Pending/Collapsed, which
    /// pop up while the user is typing/working elsewhere). But once Expanded
    /// we need `SetForegroundWindow` to succeed so the window can later
    /// receive `WM_KILLFOCUS` and close itself — and `SetForegroundWindow`
    /// on a `WS_EX_NOACTIVATE` window will not actually give it the focus.
    /// So we strip the style right before expanding and restore it on every
    /// path back to Pending/Collapsed/Hidden.
    fn set_noactivate(&mut self, add: bool) {
        unsafe {
            let cur = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE);
            let mask = WS_EX_NOACTIVATE.0 as isize;
            let new = if add { cur | mask } else { cur & !mask };
            if new != cur {
                SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new);
                let _ = SetWindowPos(
                    self.hwnd,
                    None,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
                );
            }
            self.ex_noactivate_removed = !add;
        }
    }

    // -- click / expand --------------------------------------------------

    fn try_expand(&mut self) {
        if self.state != CardState::Collapsed || self.detail.is_empty() {
            return;
        }
        unsafe {
            let _ = KillTimer(Some(self.hwnd), TIMER_DISMISS); // expanded never auto-dismisses
        }
        self.set_noactivate(false);
        self.state = CardState::Expanded;
        self.focus_link = None;
        self.scroll_offset = 0;
        self.layout_expanded();
        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.invalidate();
    }

    /// Closes the card entirely. Used for WM_KILLFOCUS and Esc while
    /// Expanded, per spec ("close on WM_KILLFOCUS ... Esc also closes").
    fn close_expanded(&mut self) {
        if self.state == CardState::Expanded {
            self.hide();
        }
    }

    /// Issue #425: `true` when `(x, y)` (client-window coordinates, the same
    /// space `WM_LBUTTONDOWN`'s `lparam` arrives in) lands inside the
    /// pinned "Copy details" footer -- `false` whenever there is no footer
    /// at all (not an error card, or not Expanded, so `copy_details_rect`
    /// is `None`).
    fn point_hits_copy_details(&self, x: i32, y: i32) -> bool {
        let Some(r) = self.copy_details_rect else {
            return false;
        };
        (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
    }

    /// Issue #425: the click or Enter activation of "Copy details" on an
    /// expanded error card. `Card`/`CardInner` never holds the full raw
    /// error chain (only the already-humanized `detail` does -- see
    /// `WM_APP_CARD_COPY_DETAILS`'s doc comment), so this only notifies the
    /// owner; `App::copy_error_details` does the actual clipboard write from
    /// `App::last_error`. Deliberately does NOT hide the card first (unlike
    /// `show_settings_needed`'s click handling): the owner's handler
    /// replaces the card's content with its own confirmation
    /// (`show_answer("Details copied", ...)`) or error, so there is nothing
    /// for this to tear down itself.
    fn copy_details_activated(&mut self) {
        if let Some(owner) = self.owner {
            unsafe {
                let _ = PostMessageW(Some(owner), WM_APP_CARD_COPY_DETAILS, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// Issue #77: `true` when `(x, y)` (client-window coordinates) lands on
    /// the pinned "Read aloud" footer; `false` when there is no such footer.
    fn point_hits_read_aloud(&self, x: i32, y: i32) -> bool {
        let Some(r) = self.read_aloud_rect else {
            return false;
        };
        (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
    }

    /// Issue #77: the footer's label for the current state.
    fn read_aloud_label(&self) -> &'static str {
        if self.reading {
            "Stop reading"
        } else {
            "Read aloud"
        }
    }

    /// Issue #77: click or Enter on the "Read aloud" footer. Starts the voice
    /// on the headline and detail, or stops it if it is already reading. A
    /// voice that cannot be created or spoken to ends in an error card
    /// (AGENTS.md rule 7), never a silent no-op.
    fn read_aloud_activated(&mut self) {
        if self.reading {
            self.stop_reading();
            self.invalidate();
            return;
        }
        let text = spoken_text(&self.headline, &self.detail);
        if text.is_empty() {
            return;
        }
        if self.speaker.is_none() {
            match Speaker::new(self.hwnd, WM_CARD_SPEECH_END) {
                Ok(s) => self.speaker = Some(s),
                Err(e) => {
                    self.show_collapsed("Can't read aloud", &format!("{e:#}"), 0, None);
                    return;
                }
            }
        }
        let result = match &self.speaker {
            Some(s) => s.speak(&text),
            None => return,
        };
        match result {
            Ok(stream) => {
                self.reading = true;
                self.reading_stream = stream;
                self.invalidate();
            }
            Err(e) => self.show_collapsed("Can't read aloud", &format!("{e:#}"), 0, None),
        }
    }

    /// Issue #77: cuts off the voice, if it is reading. Called when the card
    /// hides or is replaced, so dismissing a card stops the speech.
    fn stop_reading(&mut self) {
        if self.reading {
            if let Some(s) = &self.speaker {
                s.stop();
            }
            self.reading = false;
        }
    }

    /// Issue #77: the voice posted its notification. Only the end of the
    /// utterance this card is reading clears `reading`; the end of one that
    /// was just replaced (it can arrive after the new one has started) is
    /// ignored.
    fn on_speech_end(&mut self) {
        let Some(speaker) = &self.speaker else {
            return;
        };
        let ended = speaker.ended_streams();
        if self.reading && ended.contains(&self.reading_stream) {
            self.reading = false;
            self.invalidate();
        }
    }

    /// The footer links the card shows now, in Tab order.
    fn footer_links(&self) -> Vec<FooterLink> {
        [
            (FooterLink::CopyDetails, self.copy_details_rect),
            (FooterLink::ReadAloud, self.read_aloud_rect),
            (FooterLink::Copy, self.copy_rect),
        ]
        .into_iter()
        .filter_map(|(link, rect)| rect.map(|_| link))
        .collect()
    }

    /// #606: Tab or Shift+Tab. The card never takes focus by itself; this
    /// only runs once Expanded, which already has the keyboard.
    fn move_footer_focus(&mut self, backwards: bool) {
        if self.state != CardState::Expanded {
            return;
        }
        self.focus_link = next_footer_focus(&self.footer_links(), self.focus_link, backwards);
        self.invalidate();
    }

    /// #606: Enter or Space on the focused link. `false` when none is focused.
    fn activate_focused_link(&mut self) -> bool {
        // Never run a link that is not on screen.
        let shown = self.footer_links();
        match self.focus_link.filter(|l| shown.contains(l)) {
            Some(FooterLink::CopyDetails) => self.copy_details_activated(),
            Some(FooterLink::ReadAloud) => self.read_aloud_activated(),
            Some(FooterLink::Copy) => self.copy_activated(),
            None => return false,
        }
        true
    }

    /// The pinned footer row's rect, whichever footer the card has.
    fn footer_rect(&self) -> Option<RECT> {
        self.copy_details_rect
            .or(self.read_aloud_rect)
            .or(self.copy_rect)
    }

    /// Issue #531: `true` when `(x, y)` lands on the "Copy" link.
    fn point_hits_copy(&self, x: i32, y: i32) -> bool {
        let Some(r) = self.copy_rect else {
            return false;
        };
        (r.left..r.right).contains(&x) && (r.top..r.bottom).contains(&y)
    }

    /// Issue #531: the link's label for the current state.
    fn copy_label(&self) -> &'static str {
        match self.copy_feedback {
            CopyFeedback::None => "Copy",
            CopyFeedback::Copied => "Copied",
            CopyFeedback::Failed => "Couldn't copy",
        }
    }

    /// Issue #531: `true` when `vk` should copy: the copy key while an
    /// answer card is Expanded and showing its "Copy" link.
    fn copy_key_applies(&self, vk: u16, lparam: isize, shift: bool, alt: bool) -> bool {
        is_copy_key(vk, lparam, shift, alt)
            && self.state == CardState::Expanded
            && self.copy_rect.is_some()
    }

    /// Issue #531: shows "Copied" and arms the one-shot timer that puts
    /// "Copy" back.
    fn mark_copied(&mut self) {
        self.set_copy_feedback(CopyFeedback::Copied);
    }

    /// Issue #531: a clipboard that could not be written. The answer stays;
    /// the link says so for the same one-shot timer.
    fn mark_copy_failed(&mut self) {
        self.set_copy_feedback(CopyFeedback::Failed);
    }

    fn set_copy_feedback(&mut self, feedback: CopyFeedback) {
        self.copy_feedback = feedback;
        unsafe {
            let _ = SetTimer(Some(self.hwnd), TIMER_COPIED, COPIED_FEEDBACK_MS, None);
        }
        self.invalidate();
    }

    /// Issue #531: click or the copy key on "Copy". Puts the headline and
    /// detail on the clipboard. A clipboard that cannot be written is said
    /// on the link itself (AGENTS.md rule 7), never a silent no-op, and the
    /// answer stays on screen.
    fn copy_activated(&mut self) {
        let text = answer_clipboard_text(&self.headline, &self.detail);
        // Decision (#716): the answer came from the user's screen, so it is
        // kept out of the cloud clipboard, but stays in local Win+V history.
        // If the exclusion cannot be written, fall back to a plain copy so
        // the card never says "failed" when the text is on the clipboard.
        use arboard::SetExtWindows;
        let copied = arboard::Clipboard::new()
            .and_then(|mut c| c.set().exclude_from_cloud().text(text.clone()))
            .is_ok()
            || arboard::Clipboard::new()
                .and_then(|mut c| c.set_text(text))
                .is_ok();
        if copied {
            self.mark_copied();
        } else {
            self.mark_copy_failed();
        }
    }

    /// #376: the card as it is drawn now, as opaque RGBA rows plus the size.
    /// Drawn with `paint_into` into a memory bitmap, never read from the
    /// screen, so a window over the card cannot leak in. The keyboard focus
    /// ring is left out; everything else is exactly what the card shows.
    fn render_rgba(&mut self) -> Option<(Vec<u8>, u32, u32)> {
        use windows::Win32::Graphics::Gdi::{
            CreateDIBSection, GdiFlush, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        let mut rc = RECT::default();
        unsafe { GetClientRect(self.hwnd, &mut rc).ok()? };
        let (w, h) = (rc.right - rc.left, rc.bottom - rc.top);
        if w <= 0 || h <= 0 {
            return None;
        }
        let focus = self.focus_link.take();
        let mut out = None;
        unsafe {
            let mem_dc = CreateCompatibleDC(None);
            if !mem_dc.0.is_null() {
                let bmi = BITMAPINFO {
                    bmiHeader: BITMAPINFOHEADER {
                        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                        biWidth: w,
                        biHeight: -h, // top-down
                        biPlanes: 1,
                        biBitCount: 32,
                        biCompression: BI_RGB.0,
                        ..Default::default()
                    },
                    ..Default::default()
                };
                let mut pixels: *mut c_void = std::ptr::null_mut();
                if let Ok(bmp) =
                    CreateDIBSection(Some(mem_dc), &bmi, DIB_RGB_COLORS, &mut pixels, None, 0)
                {
                    if !pixels.is_null() {
                        let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
                        self.paint_into(mem_dc, rc);
                        let _ = GdiFlush();
                        let len = (w as usize) * (h as usize) * 4;
                        let bgra = std::slice::from_raw_parts(pixels as *const u8, len);
                        out = Some((bgra_to_opaque_rgba(bgra), w as u32, h as u32));
                        SelectObject(mem_dc, old);
                    }
                    let _ = DeleteObject(HGDIOBJ(bmp.0));
                }
                let _ = DeleteDC(mem_dc);
            }
        }
        self.focus_link = focus;
        out
    }

    /// #376: Ctrl+Shift+C. The result is said on the Copy link, like the
    /// text copy, and never a silent no-op (AGENTS.md rule 7).
    fn copy_image_activated(&mut self) {
        let copied = self.render_rgba().is_some_and(|(rgba, w, h)| {
            crate::executors::image_clipboard::copy_rgba(&rgba, w, h).is_ok()
        });
        if copied {
            self.mark_copied();
        } else {
            self.mark_copy_failed();
        }
    }

    // -- layout ------------------------------------------------------------

    /// The work area the card is about to be laid out in, and the first
    /// step of every layout: it also switches the card to that monitor's DPI
    /// (rebuilding the fonts) so the sizes the layout then computes are
    /// right for the monitor the card is moving to, not the one it was last
    /// on.
    ///
    /// `from_cursor` (Pending and Collapsed) picks the monitor the setting
    /// names, else the one under the mouse, and records the pointer as the
    /// `FollowMouse` anchor. Without it (Expanded, Preview) the card stays on
    /// the monitor it is on, since the mouse may have moved on while the user
    /// reads, unless the setting pins a monitor.
    fn target_work_area(&mut self, from_cursor: bool) -> RECT {
        use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
        let base = unsafe {
            if from_cursor {
                let mut pt = POINT::default();
                let _ = GetCursorPos(&mut pt);
                self.follow_anchor = pt;
                MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
            } else {
                MonitorFromWindow(self.hwnd, MONITOR_DEFAULTTONEAREST)
            }
        };
        let target =
            if self.corner == CardCorner::FollowMouse || self.monitor == MonitorChoice::Cursor {
                base
            } else {
                let primary = unsafe {
                    MonitorFromPoint(
                        POINT { x: 0, y: 0 },
                        windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTOPRIMARY,
                    )
                };
                let all: Vec<_> = sorted_monitors().into_iter().map(|(_, h)| h).collect();
                resolve_monitor(self.monitor, base, primary, &all)
            };
        let (mut dx, mut dy) = (0u32, 0u32);
        let queried =
            unsafe { GetDpiForMonitor(target, MDT_EFFECTIVE_DPI, &mut dx, &mut dy) }.is_ok();
        if self.dpi_forced {
            // The gallery's simulated DPI stays whatever monitor this is.
        } else if let Some(dpi) = dpi_to_apply(self.dpi, queried.then_some(dx)) {
            self.dpi = dpi;
            self.rebuild_fonts();
        }
        work_area_for_monitor(target)
    }

    fn place_bottom_right(&self, work: RECT, width: i32, height: i32) {
        let margin = self.scale(MARGIN_DP);
        let (x, y) = card_origin(self.corner, work, margin, width, height, self.follow_anchor);
        unsafe {
            let _ = SetWindowPos(
                self.hwnd,
                Some(HWND_TOPMOST),
                x,
                y,
                width,
                height,
                SWP_NOACTIVATE,
            );
        }
    }

    fn layout_pending(&mut self) {
        let work = self.target_work_area(true);
        let padding = self.scale(PADDING_DP);
        let width = self.scale(PENDING_WIDTH_DP);
        let glyph_size = self.scale(PENDING_SIZE_DP);
        let gap = self.scale(PENDING_TEXT_GAP_DP);
        let text_line_h = self.line_height(self.fonts.caption).max(1);
        let height = padding * 2 + glyph_size + gap + text_line_h;
        self.place_bottom_right(work, width, height);
    }

    fn layout_collapsed(&mut self) {
        let work = self.target_work_area(true);
        let padding = self.scale(PADDING_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_width = self.headline_content_width(content_width);
        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        let measured = self.measure_wrapped(self.fonts.headline, &self.headline, headline_width);
        let headline_h = measured.min(max_headline_h).max(headline_line_h);

        // No affordance line: the card is just the headline. Clicking it
        // still expands when there is detail to show.
        let mut height = padding * 2 + headline_h;
        // #885: a setup card also shows its detail, measured the same way
        // `paint_collapsed` draws it.
        if self.shows_setup_detail() {
            let gap = self.scale(GAP_DP);
            height += gap + self.setup_detail_height(content_width);
        }

        self.place_bottom_right(work, width, height);
    }

    fn layout_expanded(&mut self) {
        let work = self.target_work_area(false);
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(CARD_WIDTH_DP);
        let content_width = (width - padding * 2).max(1);

        let headline_width = self.headline_content_width(content_width);
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_width)
            .max(self.line_height(self.fonts.headline));
        let detail_h = if self.detail.is_empty() {
            0
        } else {
            self.measure_wrapped(self.fonts.caption, &self.detail, content_width)
        };
        let content_h = headline_h + if detail_h > 0 { gap + detail_h } else { 0 };

        // Issue #425: "Copy details" is a pinned footer row below the
        // scrollable headline/detail area, not part of it -- so it stays
        // reachable regardless of scroll position and its hit-test rect
        // needs no scroll_offset math (see `copy_details_rect`'s doc
        // comment). Only an error card (`copy_details_available`) reserves
        // the space at all.
        let footer_h =
            if self.copy_details_available || self.read_aloud_available || self.copy_available {
                self.line_height(self.fonts.caption).max(1)
            } else {
                0
            };
        let footer_gap = if footer_h > 0 { gap } else { 0 };

        let work_h = (work.bottom - work.top).max(1);
        let max_window_h = ((work_h as f32) * 0.6) as i32;
        let min_window_h =
            padding * 2 + self.line_height(self.fonts.headline) + footer_gap + footer_h;

        let desired_window_h = padding * 2 + content_h + footer_gap + footer_h;
        let window_h = desired_window_h.min(max_window_h).max(min_window_h);
        let viewport_h = (window_h - padding * 2 - footer_gap - footer_h).max(1);

        self.scroll_max = (content_h - viewport_h).max(0);
        self.scroll_page = viewport_h;
        self.scroll_offset = self.scroll_offset.clamp(0, self.scroll_max);

        let footer = if footer_h > 0 {
            let top = window_h - padding - footer_h;
            Some(RECT {
                left: padding,
                top,
                right: padding + content_width,
                bottom: top + footer_h,
            })
        } else {
            None
        };
        // Issue #77: the one pinned footer row is "Copy details" on an error
        // card and "Read aloud" on an answer card, never both.
        self.copy_details_rect = footer.filter(|_| self.copy_details_available);
        // Issue #531: an answer card splits that row between "Read aloud" and
        // "Copy". Each link is sized for its widest label so the row does not
        // shift when "Read aloud" becomes "Stop reading" or "Copy" "Copied".
        self.read_aloud_rect = None;
        self.copy_rect = None;
        if let Some(f) = footer.filter(|_| self.copy_available || self.read_aloud_available) {
            let body = self.fonts.caption;
            let widest = |labels: &[&str]| {
                labels
                    .iter()
                    .map(|l| self.measure_label(body, l).0)
                    .max()
                    .unwrap_or(0)
            };
            let read_w = self
                .read_aloud_available
                .then(|| widest(&["Read aloud", "Stop reading"]));
            let copy_w = widest(&["Copy", "Copied", "Couldn't copy"]);
            let gap = self.scale(FOOTER_LINK_GAP_DP);
            let (read, copy) = footer_links_layout(f, read_w, copy_w, gap);
            self.read_aloud_rect = read;
            self.copy_rect = (self.copy_available && rect_has_area(copy)).then_some(copy);
        }
        // #606: a re-layout (a DPI change) can drop the focused link.
        if self
            .focus_link
            .is_some_and(|l| !self.footer_links().contains(&l))
        {
            self.focus_link = None;
        }

        self.place_bottom_right(work, width, window_h);
    }

    /// Called after a DPI change: rebuild fonts for the new DPI and re-run
    /// whichever layout matches the current state, so every metric is
    /// re-derived from the new monitor rather than left stale.
    fn relayout_current_state(&mut self) {
        match self.state {
            CardState::Hidden => {}
            CardState::Pending => self.layout_pending(),
            CardState::Collapsed => self.layout_collapsed(),
            CardState::Expanded => self.layout_expanded(),
            CardState::Preview => {
                self.layout_preview();
                self.reposition_preview_controls();
            }
        }
        self.invalidate();
    }

    // -- text measurement ----------------------------------------------

    fn line_height(&self, font: HFONT) -> i32 {
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                return self.scale(16);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut tm = TEXTMETRICW::default();
            let ok = GetTextMetricsW(hdc, &mut tm).as_bool();
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            if ok {
                tm.tmHeight + tm.tmExternalLeading
            } else {
                self.scale(16)
            }
        }
    }

    fn measure_wrapped(&self, font: HFONT, text: &str, width: i32) -> i32 {
        if text.is_empty() || width <= 0 {
            return 0;
        }
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                return self.line_height(font);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut buf = utf16(text);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                DT_CALCRECT | DT_WORDBREAK | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            (rect.bottom - rect.top).max(0)
        }
    }

    /// Natural (unwrapped) size of a single-line label, e.g. a badge's text.
    fn measure_label(&self, font: HFONT, text: &str) -> (i32, i32) {
        if text.is_empty() {
            let h = self.line_height(font);
            return (h, h);
        }
        unsafe {
            let hdc = GetDC(None);
            if hdc.0.is_null() {
                let h = self.line_height(font);
                return (h, h);
            }
            let old = SelectObject(hdc, HGDIOBJ(font.0));
            let mut buf = utf16(text);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: 0,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                &mut buf,
                &mut rect,
                DT_CALCRECT | DT_SINGLELINE | DT_NOPREFIX,
            );
            SelectObject(hdc, old);
            ReleaseDC(None, hdc);
            (
                (rect.right - rect.left).max(1),
                (rect.bottom - rect.top).max(1),
            )
        }
    }

    // -- difficulty badge --------------------------------------------------

    /// Outer width/height of the badge shape for `difficulty`, scaled for
    /// this card's DPI and text scale. A short label (e.g. "1") floors out at
    /// a circle; a wider one ("10", "U") grows into a pill.
    fn badge_size(&self, difficulty: Difficulty) -> (i32, i32) {
        let (tw, th) = self.measure_label(self.fonts.caption, difficulty.label());
        let pad_x = self.scale(BADGE_PAD_X_DP);
        let pad_y = self.scale(BADGE_PAD_Y_DP);
        let min_d = self.scale(BADGE_MIN_DIAMETER_DP);
        let w = (tw + pad_x * 2).max(min_d);
        let h = (th + pad_y * 2).max(min_d);
        (w, h)
    }

    /// How much narrower the headline's (and, in Expanded, the scroll hint's)
    /// available width must be so nothing is drawn underneath the badge.
    /// Returns 0 whenever there is no badge to avoid (i.e. `difficulty` is
    /// `None`), which is what keeps the `None` case pixel-identical to the
    /// pre-badge layout.
    fn badge_reserve(&self, difficulty: Option<Difficulty>) -> i32 {
        match difficulty {
            None => 0,
            Some(d) => {
                let (badge_w, _) = self.badge_size(d);
                let edge_margin = self.scale(BADGE_EDGE_MARGIN_DP);
                let gap = self.scale(BADGE_TEXT_GAP_DP);
                let padding = self.scale(PADDING_DP);
                // The content area's right edge already sits `padding` in
                // from the card edge; the badge sits `edge_margin` in from
                // the same edge. Only the amount by which the badge (plus a
                // little breathing room) extends past that existing padding
                // needs to be reserved on top of it.
                (edge_margin + badge_w + gap - padding).max(0)
            }
        }
    }

    /// Content width to wrap the headline into, narrowed to clear the
    /// difficulty badge when one is shown. Every headline layout/paint call
    /// site must use this (not the raw content width) so the reserved space
    /// stays in sync between measurement and drawing.
    fn headline_content_width(&self, content_w: i32) -> i32 {
        (content_w - self.badge_reserve(self.difficulty) - self.chevron_reserve()).max(1)
    }

    /// #885: a collapsed click-to-open-Settings card draws its detail line
    /// under the headline, because a click opens Settings, not the detail.
    fn shows_setup_detail(&self) -> bool {
        self.state == CardState::Collapsed && self.open_settings_on_click && !self.detail.is_empty()
    }

    /// Height of the setup card's detail: measured, capped at
    /// [`SETUP_DETAIL_MAX_LINES`] lines. Layout and paint both use it.
    fn setup_detail_height(&self, content_w: i32) -> i32 {
        let cap = self.line_height(self.fonts.caption).max(1) * SETUP_DETAIL_MAX_LINES;
        self.measure_wrapped(self.fonts.caption, &self.detail, content_w)
            .min(cap)
    }

    /// Space kept free at the right of a collapsed headline for the "click
    /// for more" chevron (#353); zero when there is nothing more to show.
    fn chevron_reserve(&self) -> i32 {
        // A setup card shows its detail itself and a click opens Settings,
        // so a "click for more" chevron would promise the wrong thing (#885).
        if card_has_more_to_show(self.state, self.detail.is_empty()) && !self.shows_setup_detail() {
            self.scale(CHEVRON_RESERVE_DP)
        } else {
            0
        }
    }

    /// Draws a down chevron centred on `(cx, cy)`.
    unsafe fn draw_chevron(&self, hdc: HDC, cx: i32, cy: i32, color: u32) {
        let half = self.scale(CHEVRON_HALF_DP);
        let pen = CreatePen(
            PS_SOLID,
            self.scale(2).max(1),
            windows::Win32::Foundation::COLORREF(color),
        );
        let old = SelectObject(hdc, HGDIOBJ(pen.0));
        let points = [
            POINT {
                x: cx - half,
                y: cy - half / 2,
            },
            POINT {
                x: cx,
                y: cy + half / 2,
            },
            POINT {
                x: cx + half,
                y: cy - half / 2,
            },
        ];
        let _ = windows::Win32::Graphics::Gdi::Polyline(hdc, &points);
        SelectObject(hdc, old);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }

    // -- painting --------------------------------------------------------

    fn on_paint(&self) {
        unsafe {
            let mut ps = windows::Win32::Graphics::Gdi::PAINTSTRUCT::default();
            let hdc = BeginPaint(self.hwnd, &mut ps);
            let mut rc = RECT::default();
            let _ = GetClientRect(self.hwnd, &mut rc);
            let w = rc.right - rc.left;
            let h = rc.bottom - rc.top;

            if w > 0 && h > 0 && !hdc.0.is_null() {
                let mem_dc = CreateCompatibleDC(Some(hdc));
                if !mem_dc.0.is_null() {
                    let bmp = CreateCompatibleBitmap(hdc, w, h);
                    if !bmp.0.is_null() {
                        let old_bmp = SelectObject(mem_dc, HGDIOBJ(bmp.0));
                        self.paint_into(mem_dc, rc);
                        let _ = BitBlt(hdc, 0, 0, w, h, Some(mem_dc), 0, 0, SRCCOPY);
                        SelectObject(mem_dc, old_bmp);
                        let _ = DeleteObject(HGDIOBJ(bmp.0));
                    }
                    let _ = DeleteDC(mem_dc);
                }
            }

            let _ = EndPaint(self.hwnd, &ps);
        }
    }

    fn paint_into(&self, hdc: HDC, rc: RECT) {
        let palette = self.palette();
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);

        unsafe {
            // Background fill. DWM clips the actual window to rounded
            // corners at composition time, so a plain rectangular fill here
            // ends up looking correctly rounded on screen.
            let bg_brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(palette.bg));
            FillRect(hdc, &rc, bg_brush);
            let _ = DeleteObject(HGDIOBJ(bg_brush.0));

            // Subtle 1px border.
            let border_brush =
                CreateSolidBrush(windows::Win32::Foundation::COLORREF(palette.border));
            FrameRect(hdc, &rc, border_brush);
            let _ = DeleteObject(HGDIOBJ(border_brush.0));

            SetBkMode(hdc, TRANSPARENT);

            match self.state {
                CardState::Hidden => {}
                CardState::Pending => self.paint_pending(hdc, rc, padding, &palette),
                CardState::Collapsed => self.paint_collapsed(hdc, rc, padding, &palette),
                CardState::Expanded => self.paint_expanded(hdc, rc, padding, gap, &palette),
                CardState::Preview => self.paint_preview(hdc, &palette),
            }
        }
    }

    /// Issue #354: the glyph (spinner, or a static ring with animations
    /// off) sits in a fixed-size square at the top of the card, with the
    /// status line (`pending_status_text`) drawn below it -- replacing the
    /// old glyph-only, text-only pending card.
    unsafe fn paint_pending(&self, hdc: HDC, rc: RECT, padding: i32, palette: &Palette) {
        let w = rc.right - rc.left;
        let glyph_size = self.scale(PENDING_SIZE_DP);
        let cx = rc.left + w / 2;
        let cy = rc.top + padding + glyph_size / 2;
        let radius = (glyph_size / 2 - self.scale(SPINNER_STROKE_DP)).max(1);
        let left = cx - radius;
        let top = cy - radius;
        let right = cx + radius;
        let bottom = cy + radius;
        let stroke = self.scale(SPINNER_STROKE_DP).max(2);

        self.paint_pending_glyph(
            hdc, left, top, right, bottom, cx, cy, radius, stroke, palette,
        );

        // Status line, centered under the glyph.
        let gap = self.scale(PENDING_TEXT_GAP_DP);
        let text_top = rc.top + padding + glyph_size + gap;
        let text_rect = RECT {
            left: rc.left + padding,
            top: text_top,
            right: rc.right - padding,
            bottom: rc.bottom - padding,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.caption.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
        let text = pending_status_text(&self.pending_stage);
        draw_text_line(
            hdc,
            &text,
            text_rect,
            DT_CENTER | DT_TOP | DT_SINGLELINE | DT_END_ELLIPSIS | DT_NOPREFIX,
        );
    }

    /// Draws either the spinning arc-over-ring (animations on) or a single
    /// static ring (animations off, issue #354: no motion when Windows'
    /// "Animation effects" setting is off). `anim_frame` is only consulted
    /// in the spinning branch, so a static glyph never depends on the timer
    /// that `show_pending` deliberately does not arm in that case.
    #[allow(clippy::too_many_arguments)]
    unsafe fn paint_pending_glyph(
        &self,
        hdc: HDC,
        left: i32,
        top: i32,
        right: i32,
        bottom: i32,
        cx: i32,
        cy: i32,
        radius: i32,
        stroke: i32,
        palette: &Palette,
    ) {
        // Arc()/Ellipse(outline) never fill -- select NULL_BRUSH so both the
        // dim ring and the static glyph render as outlines, not filled
        // discs.
        let old_brush = SelectObject(hdc, GetStockObject(NULL_BRUSH));

        // Dim full ring underneath (also the whole glyph when static).
        // High contrast has one ink, so the track and the arc would be the
        // same line: the track goes hairline so the thick arc still shows
        // progress (#817).
        let track_stroke = if self.theme == Theme::HighContrast
            && pending_glyph_should_spin(self.animations_enabled)
        {
            1
        } else {
            stroke
        };
        let ring_pen = CreatePen(
            PS_SOLID,
            track_stroke,
            windows::Win32::Foundation::COLORREF(palette.border),
        );
        if !ring_pen.0.is_null() {
            let old_pen = SelectObject(hdc, HGDIOBJ(ring_pen.0));
            let _ = Ellipse(hdc, left, top, right, bottom);
            SelectObject(hdc, old_pen);
        }
        let _ = DeleteObject(HGDIOBJ(ring_pen.0));

        if pending_glyph_should_spin(self.animations_enabled) {
            // Bright sweeping arc on top. Prefer a geometric pen with round
            // end caps for a clean look; fall back to a plain cosmetic pen
            // if that ever fails (e.g. exotic display driver).
            let brush = LOGBRUSH {
                lbStyle: BS_SOLID,
                lbColor: windows::Win32::Foundation::COLORREF(palette.headline),
                lbHatch: 0,
            };
            let mut arc_pen = ExtCreatePen(
                PS_GEOMETRIC | PS_SOLID | PS_ENDCAP_ROUND | PS_JOIN_ROUND,
                stroke as u32,
                &brush,
                None,
            );
            if arc_pen.0.is_null() {
                arc_pen = CreatePen(
                    PS_SOLID,
                    stroke,
                    windows::Win32::Foundation::COLORREF(palette.headline),
                );
            }
            if !arc_pen.0.is_null() {
                let start_deg = (self.anim_frame as f32 * SPINNER_DEGREES_PER_FRAME) % 360.0;
                let end_deg = start_deg + SPINNER_SWEEP_DEG;
                let (x1, y1) = ray_point(cx, cy, start_deg, radius);
                let (x2, y2) = ray_point(cx, cy, end_deg, radius);

                let old_pen = SelectObject(hdc, HGDIOBJ(arc_pen.0));
                let _ = Arc(hdc, left, top, right, bottom, x1, y1, x2, y2);
                SelectObject(hdc, old_pen);
            }
            let _ = DeleteObject(HGDIOBJ(arc_pen.0));
        } else {
            // Static glyph: a brighter, slightly smaller inner ring on top
            // of the dim outer one, so the state still reads as "in
            // progress" without any per-frame motion.
            let inner_radius = (radius * 3 / 5).max(1);
            let inner_pen = CreatePen(
                PS_SOLID,
                stroke,
                windows::Win32::Foundation::COLORREF(palette.headline),
            );
            if !inner_pen.0.is_null() {
                let old_pen = SelectObject(hdc, HGDIOBJ(inner_pen.0));
                let _ = Ellipse(
                    hdc,
                    cx - inner_radius,
                    cy - inner_radius,
                    cx + inner_radius,
                    cy + inner_radius,
                );
                SelectObject(hdc, old_pen);
            }
            let _ = DeleteObject(HGDIOBJ(inner_pen.0));
        }

        SelectObject(hdc, old_brush);
    }

    unsafe fn paint_collapsed(&self, hdc: HDC, rc: RECT, padding: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let headline_w = self.headline_content_width(content_w);
        let headline_line_h = self.line_height(self.fonts.headline).max(1);
        let max_headline_h = headline_line_h * HEADLINE_MAX_LINES;
        // Must match layout_collapsed's arithmetic exactly. Using the cap here
        // instead of the measured height pushes the hint below the window's
        // bottom edge whenever the headline wraps to fewer than the maximum
        // number of lines -- i.e. most of the time.
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_w)
            .min(max_headline_h)
            .max(headline_line_h);

        let headline_rect = RECT {
            left: rc.left + padding,
            top: rc.top + padding,
            right: rc.left + padding + headline_w,
            bottom: rc.top + padding + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &self.headline,
            headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
        );

        if self.chevron_reserve() > 0 {
            let cx = headline_rect.right + self.chevron_reserve() / 2;
            let cy = headline_rect.top + headline_line_h / 2;
            self.draw_chevron(hdc, cx, cy, palette.hint);
        }

        if self.shows_setup_detail() {
            let top = headline_rect.bottom + self.scale(GAP_DP);
            let detail_rect = RECT {
                left: rc.left + padding,
                top,
                right: rc.left + padding + content_w,
                bottom: top + self.setup_detail_height(content_w),
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.caption.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
            draw_text_line(
                hdc,
                &self.detail,
                detail_rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_END_ELLIPSIS | DT_NOPREFIX,
            );
        }

        self.paint_difficulty_badge(hdc, rc);
    }

    unsafe fn paint_expanded(&self, hdc: HDC, rc: RECT, padding: i32, gap: i32, palette: &Palette) {
        let content_w = (rc.right - rc.left - padding * 2).max(1);
        let content_left = rc.left + padding;
        let content_top = rc.top + padding;
        // Issue #425: the scrollable headline/detail area stops above the
        // pinned "Copy details" footer, not at the window's own bottom
        // padding, the same reservation `layout_expanded`'s `viewport_h`
        // makes -- otherwise a long detail could scroll text underneath (or
        // the clip region could cut into) the footer row.
        let footer_reserve = self
            .footer_rect()
            .map(|r| (rc.bottom - r.top) + gap - padding)
            .unwrap_or(0)
            .max(0);
        let content_bottom = rc.bottom - padding - footer_reserve;

        // Clip to the padded content area so scrolled text never bleeds into
        // the border/padding.
        IntersectClipRect(
            hdc,
            content_left,
            content_top,
            rc.right - padding,
            content_bottom,
        );

        let headline_w = self.headline_content_width(content_w);
        let headline_h = self
            .measure_wrapped(self.fonts.headline, &self.headline, headline_w)
            .max(self.line_height(self.fonts.headline));

        let y0 = content_top - self.scroll_offset;
        let headline_rect = RECT {
            left: content_left,
            top: y0,
            right: content_left + headline_w,
            bottom: y0 + headline_h,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &self.headline,
            headline_rect,
            DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
        );

        if !self.detail.is_empty() {
            let detail_h = self.measure_wrapped(self.fonts.caption, &self.detail, content_w);
            let y1 = y0 + headline_h + gap;
            let detail_rect = RECT {
                left: content_left,
                top: y1,
                right: content_left + content_w,
                bottom: y1 + detail_h,
            };
            SelectObject(hdc, HGDIOBJ(self.fonts.caption.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.detail));
            draw_text_line(
                hdc,
                &self.detail,
                detail_rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
            );
        }

        // The content is capped at 60% of the work area, so long answers get
        // clipped mid-line. While there is more below the fold the bottom
        // edge fades into the card colour and a chevron points down (#353),
        // so it reads as "scroll" rather than as a finished card.
        if self.scroll_offset < self.scroll_max {
            let line_h = self.line_height(self.fonts.caption).max(1);
            let fade_h = line_h + self.scale(FADE_EXTRA_DP);
            let top = (content_bottom - fade_h).max(content_top);
            let rows = content_bottom - top;
            for row in 0..rows {
                let pct = fade_percent(row, rows);
                for x in (rc.left + 1)..(rc.right - 1) {
                    let under = windows::Win32::Graphics::Gdi::GetPixel(hdc, x, top + row).0;
                    let blended = blend_colors(under, palette.bg, pct);
                    let _ = windows::Win32::Graphics::Gdi::SetPixel(
                        hdc,
                        x,
                        top + row,
                        windows::Win32::Foundation::COLORREF(blended),
                    );
                }
            }
            // Anchored off `content_bottom` (the scroll area's own bottom
            // edge, #425) and left of the badge reservation, so it never
            // lands on the "Copy details" footer or the difficulty badge.
            let cx = content_left + headline_w - self.scale(CHEVRON_HALF_DP);
            let cy = content_bottom - line_h / 2;
            self.draw_chevron(hdc, cx, cy, palette.detail);
        }

        self.paint_difficulty_badge(hdc, rc);

        // Issue #425: the pinned "Copy details" footer, painted outside the
        // clip region the scrollable content above used (it is set with
        // `IntersectClipRect`, which only ever narrows -- painting here,
        // after that clip has already applied to everything above, still
        // draws within it unless the clip is reset, so reset it first).
        let links = [
            (self.copy_details_rect, "Copy details"),
            (self.read_aloud_rect, self.read_aloud_label()),
            (self.copy_rect, self.copy_label()),
        ];
        let _ = SelectClipRgn(hdc, None);
        SelectObject(hdc, HGDIOBJ(self.fonts.caption.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.link));
        for (rect, label) in links {
            if let Some(rect) = rect {
                draw_text_line(
                    hdc,
                    label,
                    rect,
                    DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX,
                );
            }
        }
        // #606, #817: a 2 px solid ring round the link Tab has reached, in the
        // text ink with a thin ring of the surface inside it, so it shows on
        // the card in light, dark and high contrast alike.
        if let Some(rect) = self.focus_link.and_then(|l| self.link_rect(l)) {
            let ring = RECT {
                left: rect.left - 3,
                top: rect.top - 2,
                right: rect.right + 3,
                bottom: rect.bottom + 2,
            };
            let thickness = self.scale(2).max(2);
            frame_rect(hdc, &ring, thickness, palette.headline);
            let inner = RECT {
                left: ring.left + thickness,
                top: ring.top + thickness,
                right: ring.right - thickness,
                bottom: ring.bottom - thickness,
            };
            frame_rect(hdc, &inner, 1, palette.bg);
        }
    }

    fn link_rect(&self, link: FooterLink) -> Option<RECT> {
        match link {
            FooterLink::CopyDetails => self.copy_details_rect,
            FooterLink::ReadAloud => self.read_aloud_rect,
            FooterLink::Copy => self.copy_rect,
        }
    }

    /// Draws the difficulty badge in the card's bottom-right corner, if any
    /// (`self.difficulty` is `None` for Pending, for errors, and whenever the
    /// integrator disabled the feature -- in all of those cases this is a
    /// no-op, which is what keeps that path pixel-identical to the code
    /// before this feature existed).
    ///
    /// Called last, after the rest of `paint_collapsed`/`paint_expanded`, so
    /// the badge always sits on top. `paint_expanded` narrows its clip region
    /// to the padded content box for scrolling; the badge lives partly
    /// outside that box (in the corner's padding whitespace), so the clip is
    /// reset first -- otherwise it would be silently clipped away there.
    unsafe fn paint_difficulty_badge(&self, hdc: HDC, rc: RECT) {
        let Some(difficulty) = self.difficulty else {
            return;
        };

        let _ = SelectClipRgn(hdc, None);

        let (w, h) = self.badge_size(difficulty);
        let edge_margin = self.scale(BADGE_EDGE_MARGIN_DP);
        let right = rc.right - edge_margin;
        let bottom = rc.bottom - edge_margin;
        let left = (right - w).max(rc.left);
        let top = (bottom - h).max(rc.top);

        let (fill, text_color) = badge_colors(difficulty, self.theme, &self.sys);
        // Equal-radius rounding on both axes: when w == h (short labels like
        // "1") this comes out as a circle; when w > h ("10", "U") it comes
        // out as a pill.
        let round = h;

        let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(fill));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let old_pen = SelectObject(hdc, GetStockObject(NULL_PEN));
        let _ = RoundRect(hdc, left, top, right, bottom, round, round);
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(brush.0));

        let label_rect = RECT {
            left,
            top,
            right,
            bottom,
        };
        SelectObject(hdc, HGDIOBJ(self.fonts.caption.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(text_color));
        draw_text_line(
            hdc,
            difficulty.label(),
            label_rect,
            DT_CENTER | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX,
        );
    }

    // -- message handling --------------------------------------------------

    fn handle_message(&mut self, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_ERASEBKGND => Some(LRESULT(1)),
            // Issue #350: without this, preview EDIT fields keep the stock
            // white background and black text regardless of theme -- the
            // one control on the card where the user reads carefully before
            // confirming stays bright in dark mode. Only WM_CTLCOLOREDIT is
            // handled: every preview field the card creates is an editable
            // EDIT control (`create_preview_edit`, no ES_READONLY, no
            // STATIC), so WM_CTLCOLORSTATIC never fires for them and adding
            // a handler for it would be dead code.
            WM_CTLCOLOREDIT => {
                let hdc = HDC(wparam.0 as *mut _);
                let palette = self.palette();
                let (text, bg) = edit_field_colors(&palette, self.theme, self.sys.highlight);
                unsafe {
                    SetTextColor(hdc, windows::Win32::Foundation::COLORREF(text));
                    SetBkColor(hdc, windows::Win32::Foundation::COLORREF(bg));
                    if self.edit_bg_brush.0.is_null() {
                        self.edit_bg_brush =
                            CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg));
                    }
                }
                let _ = HWND(lparam.0 as *mut _); // the EDIT control; unused (every preview EDIT shares one theme)
                Some(LRESULT(self.edit_bg_brush.0 as isize))
            }
            WM_DRAWITEM => {
                self.draw_preview_button(lparam);
                Some(LRESULT(1))
            }
            WM_PAINT => {
                self.on_paint();
                Some(LRESULT(0))
            }
            WM_TIMER => {
                match wparam.0 {
                    TIMER_ANIM => {
                        self.anim_frame = self.anim_frame.wrapping_add(1);
                        self.invalidate();
                    }
                    // The preview state never auto-dismisses on a timer
                    // (rule 5 and safety: an action that writes something
                    // must never fire because nobody was there to see a
                    // countdown). `show_preview` never arms TIMER_DISMISS,
                    // but this guard makes that a hard invariant rather than
                    // "nothing currently posts one": a WM_TIMER already
                    // queued from a previous state (KillTimer stops future
                    // messages, not ones already in the queue) must still
                    // never close an open preview out from under the user
                    // -- the guard failing simply falls through to the `_`
                    // arm below, a deliberate no-op.
                    TIMER_DISMISS if self.state != CardState::Preview => {
                        self.hide();
                    }
                    // Issue #354: only acts while still genuinely Pending --
                    // `set_pending_stage`'s own guard would also catch a
                    // stale message, but checking here too means a message
                    // already queued from a state `kill_timers` could not
                    // reach in time never even calls into it.
                    TIMER_STILL_WORKING if self.state == CardState::Pending => {
                        self.set_pending_stage(PendingStage::StillWorking);
                    }
                    // Issue #531: one-shot; a stale message after the card
                    // moved on finds the feedback already cleared.
                    TIMER_COPIED => {
                        unsafe {
                            let _ = KillTimer(Some(self.hwnd), TIMER_COPIED);
                        }
                        if self.copy_feedback != CopyFeedback::None {
                            self.copy_feedback = CopyFeedback::None;
                            self.invalidate();
                        }
                    }
                    _ => {}
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN => {
                // Issue #347: a settings-needed card's click opens Settings
                // instead of expanding -- checked before try_expand() (whose
                // own guard would otherwise just expand it, since these
                // cards are always Collapsed with a non-empty detail).
                if self.state == CardState::Collapsed && self.open_settings_on_click {
                    self.open_settings_on_click = false;
                    if let Some(owner) = self.owner {
                        unsafe {
                            let _ = PostMessageW(
                                Some(owner),
                                WM_APP_CARD_OPEN_SETTINGS,
                                WPARAM(0),
                                LPARAM(0),
                            );
                        }
                    }
                    self.hide();
                } else if self.state == CardState::Expanded {
                    // Issue #425: checked before try_expand() -- whose own
                    // guard would no-op here anyway (Expanded, not
                    // Collapsed), but the point is that a click on the
                    // "Copy details" footer must activate it instead of
                    // falling through to nothing.
                    let (x, y) = crate::dismiss::unpack_point(lparam.0 as u32);
                    if self.point_hits_copy_details(x, y) {
                        self.copy_details_activated();
                    } else if self.point_hits_read_aloud(x, y) {
                        self.read_aloud_activated();
                    } else if self.point_hits_copy(x, y) {
                        self.copy_activated();
                    }
                } else {
                    self.try_expand();
                }
                Some(LRESULT(0))
            }
            // #353: a card with more to show is clickable, so say so.
            WM_SETCURSOR => {
                let hit_test = (lparam.0 & 0xFFFF) as u32;
                if hit_test == 1 /* HTCLIENT */
                    && card_has_more_to_show(self.state, self.detail.is_empty())
                {
                    unsafe {
                        use windows::Win32::UI::WindowsAndMessaging::{SetCursor, IDC_HAND};
                        if let Ok(cursor) = LoadCursorW(None, IDC_HAND) {
                            SetCursor(Some(cursor));
                        }
                    }
                    Some(LRESULT(1))
                } else {
                    None
                }
            }
            WM_MOUSEWHEEL => {
                if self.state == CardState::Expanded {
                    let raw = (wparam.0 & 0xFFFF_FFFF) as u32;
                    let delta = ((raw >> 16) as i16) as i32;
                    let step = self.scale(WHEEL_SCROLL_DP);
                    let notches = delta / 120;
                    self.scroll_offset =
                        (self.scroll_offset - notches * step).clamp(0, self.scroll_max);
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                if self.state == CardState::Expanded && wparam.0 as u16 == VK_ESCAPE.0 {
                    self.close_expanded();
                } else if self.state == CardState::Expanded && wparam.0 as u16 == VK_TAB.0 {
                    self.move_footer_focus(key_is_down(VK_SHIFT.0));
                } else if self.state == CardState::Expanded
                    && (wparam.0 as u16 == VK_RETURN.0 || wparam.0 as u16 == VK_SPACE.0)
                    && self.focus_link.is_some()
                {
                    self.activate_focused_link();
                } else if let Some(target) = self
                    .scroll_key_target(wparam.0 as u16)
                    .filter(|_| self.state == CardState::Expanded)
                {
                    // #353: an overflowing expanded card scrolls from the
                    // keyboard, not only with the mouse wheel.
                    if target != self.scroll_offset {
                        self.scroll_offset = target;
                        self.invalidate();
                    }
                } else if self.copy_key_applies(
                    wparam.0 as u16,
                    lparam.0,
                    key_is_down(VK_SHIFT.0),
                    key_is_down(VK_MENU.0),
                ) {
                    // Issue #531: C and Ctrl+C copy the answer.
                    self.copy_activated();
                } else if self.state == CardState::Expanded
                    && self.copy_rect.is_some()
                    && is_copy_image_key(
                        wparam.0 as u16,
                        lparam.0,
                        key_is_down(windows::Win32::UI::Input::KeyboardAndMouse::VK_CONTROL.0),
                        key_is_down(VK_SHIFT.0),
                        key_is_down(VK_MENU.0),
                    )
                {
                    // #376: Ctrl+Shift+C copies the card as an image.
                    self.copy_image_activated();
                } else if self.state == CardState::Expanded
                    && wparam.0 as u16 == VK_RETURN.0
                    && self.copy_details_rect.is_some()
                {
                    // Issue #425: "keyboard reachable too, not just mouse" --
                    // Enter activates "Copy details" while it is showing, the
                    // same way it already activates "Do it" in Preview.
                    self.copy_details_activated();
                } else if self.state == CardState::Expanded
                    && wparam.0 as u16 == VK_RETURN.0
                    && self.read_aloud_rect.is_some()
                {
                    // Issue #77: Enter toggles "Read aloud" the same way.
                    self.read_aloud_activated();
                } else if self.state == CardState::Preview {
                    let enter_confirms = self.preview.as_ref().map_or(true, |p| p.enter_confirms);
                    if let Some(command_id) = preview_key_command(wparam.0 as u16, enter_confirms) {
                        self.run_preview_command(command_id);
                    }
                }
                Some(LRESULT(0))
            }
            WM_COMMAND => {
                if self.state == CardState::Preview {
                    let id = (wparam.0 & 0xFFFF) as i32;
                    self.run_preview_command(id);
                }
                Some(LRESULT(0))
            }
            WM_CARD_PREVIEW_COMMAND => {
                if self.state == CardState::Preview
                    && preview_command_is_current(lparam.0 as u32, self.preview_generation)
                {
                    self.run_preview_command((wparam.0 & 0xFFFF) as i32);
                }
                Some(LRESULT(0))
            }
            WM_KILLFOCUS => {
                self.close_expanded();
                Some(LRESULT(0))
            }
            WM_CARD_SPEECH_END => {
                self.on_speech_end();
                Some(LRESULT(0))
            }
            // #431: the light/dark switch. Event-driven (no timer, rule 5):
            // Windows broadcasts this to every top-level window, so the card
            // hears it even while hidden.
            WM_SETTINGCHANGE => {
                if setting_change_affects_theme(wparam.0, &unsafe { setting_change_name(lparam) }) {
                    self.refresh_theme();
                }
                Some(LRESULT(0))
            }
            // The high contrast switch and a new contrast scheme: Windows
            // sends these to every top-level window as well.
            WM_THEMECHANGED | WM_SYSCOLORCHANGE => {
                self.refresh_theme();
                Some(LRESULT(0))
            }
            WM_DPICHANGED if self.dpi_forced => Some(LRESULT(0)),
            WM_DPICHANGED => {
                let new_dpi = (wparam.0 & 0xFFFF) as u32;
                self.dpi = new_dpi.max(1);
                self.rebuild_fonts();
                self.relayout_current_state();
                Some(LRESULT(0))
            }
            WM_NCDESTROY => {
                self.fonts.delete();
                self.fonts = Fonts::null();
                if !self.edit_bg_brush.0.is_null() {
                    unsafe {
                        let _ = DeleteObject(HGDIOBJ(self.edit_bg_brush.0));
                    }
                    self.edit_bg_brush = HBRUSH(std::ptr::null_mut());
                }
                self.hwnd = HWND(std::ptr::null_mut());
                None
            }
            WM_DESTROY => None,
            _ => None,
        }
    }

    // -- preview (confirmation) state (#26) -----------------------------

    /// Enters `CardState::Preview`. See [`Card::show_preview`] for the
    /// public contract (focus behaviour, the `main_window_exists` gate).
    fn show_preview(
        &mut self,
        title: &str,
        schema: &Value,
        proposal_value: &Value,
        main_window_exists: bool,
    ) {
        self.show_preview_labeled(
            title,
            schema,
            proposal_value,
            main_window_exists,
            "Do it",
            None,
        );
    }

    /// [`CardInner::show_preview`] with the confirm button's text chosen by
    /// the caller ("Send" for #105's send preview).
    fn show_preview_labeled(
        &mut self,
        title: &str,
        schema: &Value,
        proposal_value: &Value,
        main_window_exists: bool,
        confirm_label: &'static str,
        note: Option<&str>,
    ) {
        self.leave_preview_if_active();
        // Issue #77: the preview replaces whatever was showing, including an
        // answer being read aloud.
        self.stop_reading();
        // The preview never auto-dismisses on a timer (rule 5 and safety:
        // an action that writes something must never fire because nobody
        // was there to see a countdown) -- so, unlike show_pending/
        // show_collapsed, no TIMER_DISMISS is ever armed here.
        self.kill_timers();

        let previous_foreground = unsafe { GetForegroundWindow() };
        // Issue #109: from Hidden the card has no current position.
        self.place_from_cursor = self.state == CardState::Hidden;

        let model = PreviewModel::from_schema(schema, proposal_value);

        self.headline.clear();
        self.detail.clear();
        self.difficulty = None;
        self.state = CardState::Preview;
        self.scroll_offset = 0;
        self.scroll_max = 0;
        // Issue #356: same reasoning as `show_collapsed` -- the window's
        // accessible name follows whatever the card is showing, here the
        // preview's own title (e.g. "Add to calendar") standing in for a
        // headline.
        self.set_window_text(title);

        // Preview needs real keyboard focus (typing into an editable field,
        // Enter, Esc), unlike Pending/Collapsed -- see the "Focus" note on
        // `Card::show_preview`.
        self.set_noactivate(false);

        // Starting a fresh preview must never let an earlier, never-taken
        // confirmation leak into this one's lifetime.
        self.last_confirmed = None;

        self.preview = Some(PreviewUi {
            model,
            title: title.to_string(),
            edits: Vec::new(),
            do_it_btn: HWND(std::ptr::null_mut()),
            edit_btn: None,
            previous_foreground,
            main_window_exists,
            error: None,
            confirm_label,
            enter_confirms: label_confirms_with_enter(confirm_label),
            note: note.filter(|n| !n.is_empty()).map(str::to_string),
        });
        // #225: from here until a decision is reported, tearing this
        // preview down is an abandonment and the owner has to be told. Set
        // AFTER the `leave_preview_if_active` above, so replacing a live
        // preview reports the OLD one exactly once and then arms the new.
        self.preview_decision_pending = true;
        self.preview_generation = self.preview_generation.wrapping_add(1);

        self.layout_preview();
        self.create_preview_controls();
        self.reveal();

        unsafe {
            let _ = SetForegroundWindow(self.hwnd);
        }
        self.focus_initial_preview_control();
    }

    /// The one place every WM_COMMAND id and every preview keyboard mapping
    /// (from [`preview_key_command`]) is dispatched, so the card's own
    /// WM_KEYDOWN arm and the child-control subclass (which posts the same
    /// ids back to this window) always mean exactly the same thing.
    fn run_preview_command(&mut self, id: i32) {
        match id {
            ID_PREVIEW_DO_IT => self.preview_do_it(),
            ID_PREVIEW_CANCEL => self.preview_cancel(),
            ID_PREVIEW_EDIT => self.preview_edit_clicked(),
            _ => {}
        }
    }

    /// "Do it": reads every editable control's current text back into the
    /// model, refuses (leaving the card open) if a required field is now
    /// blank, then builds the `Confirmed<Value>` from exactly those values
    /// via `ui::confirm::confirm_preview` -- never from the original
    /// proposal -- and closes. This is #26's Done-when in code: what
    /// `take_confirmed` later hands an executor is provably what the card
    /// had on screen the moment "Do it" fired.
    fn preview_do_it(&mut self) {
        self.sync_edits_from_controls();
        let Some(preview) = self.preview.as_mut() else {
            return;
        };
        if !preview.model.is_valid() {
            // A required field is blank after editing: refuse silently and
            // leave the card open rather than build a Confirmed from an
            // invalid form. A card is never a dialog box (rule 7); an
            // inline validation message is left to a follow-up issue, not
            // invented here.
            return;
        }
        let type_errors = preview.model.type_error_fields();
        if !type_errors.is_empty() {
            // #284: an edit that cannot be read as its field's declared type
            // (a number field holding "abc") is never confirmed as a string.
            // The preview stays open with every other edit intact, the
            // problem is shown in place of the title, and the first bad
            // field takes focus.
            let message = type_errors
                .iter()
                .map(|(_, m)| m.as_str())
                .collect::<Vec<_>>()
                .join(". ");
            let first_bad = type_errors[0].0.clone();
            preview.error = Some(message);
            let target = preview
                .edits
                .iter()
                .find(|(name, _)| *name == first_bad)
                .map(|(_, hwnd)| *hwnd);
            if let Some(hwnd) = target {
                unsafe {
                    let _ = SetFocus(Some(hwnd));
                }
            }
            // The message takes a line of its own, so the card grows.
            self.relayout_current_state();
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(self.hwnd), None, false);
            }
            return;
        }
        preview.error = None;
        let shown = preview.model.to_value();
        let confirmed = crate::ui::confirm::confirm_preview(
            &preview.model,
            crate::ui::confirm::user_confirmed(),
        );
        debug_assert_eq!(
            *confirmed.value(),
            shown,
            "confirm_preview must hand back exactly what the model showed"
        );
        // Stored on `self`, not on `preview`: `close_preview` (below) drops
        // `PreviewUi` before returning -- see `last_confirmed`'s doc
        // comment on why this can't live there.
        self.last_confirmed = Some(confirmed);
        // #225: claim the decision before `close_preview` reaches
        // `leave_preview_if_active`, so it does not also report this as an
        // abandonment (and does not clear the `Confirmed` just stored).
        let generation = self.preview_generation;
        self.preview_decision_pending = false;
        self.close_preview();
        self.notify_owner_of_preview_decision(generation);
    }

    /// "Cancel" / Esc: produces nothing (no `Confirmed` is ever built) and
    /// closes. Also clears any earlier `last_confirmed` a caller never took,
    /// so `take_confirmed` reliably returns `None` after a Cancel rather
    /// than silently handing back a stale confirmation from an unrelated,
    /// earlier preview.
    fn preview_cancel(&mut self) {
        self.last_confirmed = None;
        // #225: same as `preview_do_it` -- this path reports its own
        // decision, so it is not an abandonment.
        let generation = self.preview_generation;
        self.preview_decision_pending = false;
        self.close_preview();
        self.notify_owner_of_preview_decision(generation);
    }

    /// Issue #39: the one way `CardInner` (a plain Win32 window with no
    /// reference back to `App`, and must stay that way) tells the owner
    /// window a preview just closed with a decision. Carries no payload
    /// (`wparam`/`lparam` are both `0`) -- see [`WM_APP_PREVIEW_DECIDED`]'s
    /// doc comment for why that is safe even if this message is dropped
    /// while Settings is open. A no-op if [`Card::set_owner`] was never
    /// called.
    /// #225: `WPARAM` is the generation of the preview this decision is
    /// about, so a late-delivered abandonment cannot be mistaken for a
    /// decision on whatever preview is on screen by the time it arrives.
    fn notify_owner_of_preview_decision(&self, generation: u32) {
        if let Some(owner) = self.owner {
            unsafe {
                let _ = PostMessageW(
                    Some(owner),
                    WM_APP_PREVIEW_DECIDED,
                    WPARAM(generation as usize),
                    LPARAM(0),
                );
            }
        }
    }

    /// "Edit": only reachable when `main_window_exists` was `true` at
    /// `show_preview` time -- the button (and thus `ID_PREVIEW_EDIT`) is not
    /// even created otherwise (#352), so Windows never delivers a click (or
    /// this synthetic command) for it. No caller passes `true` yet (Wingman
    /// has no main window today), so this body is an intentional no-op
    /// placeholder for the issue that adds one, the same "inert until its
    /// caller exists" status `Action::hotkey` has -- not a control silently
    /// doing nothing where a real signal was expected, since that control
    /// does not exist in production yet.
    fn preview_edit_clicked(&mut self) {}

    /// Reads every editable control's `GetWindowTextW` back into the model.
    /// Called once, right before "Do it" decides whether the form is valid
    /// -- never continuously (no `EN_CHANGE` tracking), which keeps "what
    /// was shown" meaning exactly what was on screen at the moment of the
    /// decision, not some earlier keystroke.
    fn sync_edits_from_controls(&mut self) {
        let edits: Vec<(String, HWND)> = match &self.preview {
            Some(p) => p.edits.clone(),
            None => return,
        };
        for (name, hwnd) in edits {
            let text = window_text(hwnd);
            if let Some(preview) = self.preview.as_mut() {
                preview.model.set_value(&name, text);
            }
        }
    }

    /// Tears down the preview's child controls, restores focus to whatever
    /// window had it before `show_preview`, and hides the card. Used by
    /// both `preview_do_it` and `preview_cancel` -- the only two ways out of
    /// Preview that go through here rather than a later `show_*`/`hide`
    /// call (both of which also tear the preview down, via
    /// `leave_preview_if_active`).
    fn close_preview(&mut self) {
        let previous_foreground = self.preview.as_ref().map(|p| p.previous_foreground);
        self.hide();
        if let Some(prev) = previous_foreground {
            if !prev.0.is_null() && unsafe { IsWindow(Some(prev)) }.as_bool() {
                unsafe {
                    let _ = SetForegroundWindow(prev);
                }
            }
        }
    }

    /// The single choke point that guarantees "`self.preview` is `Some`
    /// iff `state == Preview`": called at the top of every state-entering
    /// method (`show_pending`, `show_collapsed`, `show_preview` itself, and
    /// `hide`) so a preview's child controls can never survive a jump to a
    /// different state, regardless of which path got there.
    /// The single choke point every path out of `CardState::Preview` goes
    /// through. Issue #225: it is also where an ABANDONED preview is
    /// reported. "Do it" and Cancel clear `preview_decision_pending` before
    /// they get here and post their own notification, so they do not double
    /// post (`preview_do_it_posts_exactly_one_wm_app_preview_decided`
    /// guards that). Every other way out of Preview leaves the flag set,
    /// and the owner is told here.
    fn leave_preview_if_active(&mut self) {
        if self.preview.is_some() {
            self.destroy_preview_controls();
            self.preview = None;
        }
        if self.preview_decision_pending {
            self.preview_decision_pending = false;
            // No `Confirmed` is produced, and any stale one is dropped: an
            // abandoned preview must never look like a confirmation.
            self.last_confirmed = None;
            self.notify_owner_of_preview_decision(self.preview_generation);
        }
    }

    fn destroy_preview_controls(&mut self) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        unsafe {
            for (_, hwnd) in &preview.edits {
                if !hwnd.0.is_null() {
                    let _ = DestroyWindow(*hwnd);
                }
            }
            let mut btns = vec![preview.do_it_btn];
            if let Some(edit_btn) = preview.edit_btn {
                btns.push(edit_btn);
            }
            for hwnd in btns {
                if !hwnd.0.is_null() {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
    }

    fn focus_initial_preview_control(&self) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        // #105: the send preview must not put focus on its Send button, or a
        // stray space bar typed for another window would press it. Focus the
        // card itself; Esc still reaches it.
        let target = if preview.enter_confirms {
            preview
                .edits
                .first()
                .map(|(_, hwnd)| *hwnd)
                .unwrap_or(preview.do_it_btn)
        } else {
            self.hwnd
        };
        if !target.0.is_null() {
            unsafe {
                let _ = SetFocus(Some(target));
            }
        }
    }

    /// Creates one EDIT control per editable field and the three buttons,
    /// all parented to the card's own window and positioned from
    /// [`CardInner::compute_preview_layout`]. Every child control is
    /// subclassed with [`preview_control_subclass`] so Enter/Esc work
    /// regardless of which control has focus (see that function's doc
    /// comment for why this cannot rely on the integrating app's message
    /// loop).
    fn create_preview_controls(&mut self) {
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        let instance = self.instance;
        let parent = self.hwnd;
        let font = self.fonts.body;

        let mut edits = Vec::new();
        for (field, row) in fields.iter().zip(metrics.rows.iter()) {
            if !field.editable {
                continue;
            }
            let rect = field_edit_rect(
                &row.value_rect,
                self.line_height(font),
                field.multiline,
                self.scale(FIELD_PAD_X_DP),
                self.scale(FIELD_PAD_Y_DP),
            );
            if let Some(hwnd) = create_preview_edit(parent, instance, font, &field.value, &rect) {
                subclass_preview_control(hwnd, parent);
                edits.push((field.name.clone(), hwnd));
            }
        }

        let confirm_label = self
            .preview
            .as_ref()
            .map(|p| p.confirm_label)
            .unwrap_or("Do it");
        let do_it_btn = create_preview_button(
            parent,
            instance,
            font,
            confirm_label,
            &metrics.buttons.do_it,
            ID_PREVIEW_DO_IT,
            true,
        );
        let edit_btn = metrics.buttons.edit.map(|rect| {
            create_preview_button(parent, instance, font, "Edit", &rect, ID_PREVIEW_EDIT, true)
        });
        let mut btns = vec![do_it_btn];
        if let Some(edit_btn) = edit_btn {
            btns.push(edit_btn);
        }
        for hwnd in btns {
            if !hwnd.0.is_null() {
                subclass_preview_control(hwnd, parent);
            }
        }

        if let Some(preview) = self.preview.as_mut() {
            preview.edits = edits;
            preview.do_it_btn = do_it_btn;
            preview.edit_btn = edit_btn;
        }
    }

    /// Re-runs preview layout and moves every existing child control (and
    /// re-applies the possibly-rebuilt font) to match -- the DPI-change
    /// counterpart of `create_preview_controls`, which only ever runs once
    /// per `show_preview` call.
    fn reposition_preview_controls(&mut self) {
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        let font = self.fonts.body;

        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        for (name, hwnd) in &preview.edits {
            if let Some(idx) = fields.iter().position(|f| &f.name == name) {
                let cell = metrics.rows[idx].value_rect;
                let r = &field_edit_rect(
                    &cell,
                    self.line_height(font),
                    fields[idx].multiline,
                    self.scale(FIELD_PAD_X_DP),
                    self.scale(FIELD_PAD_Y_DP),
                );
                unsafe {
                    let _ = SetWindowPos(
                        *hwnd,
                        None,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER,
                    );
                    SendMessageW(
                        *hwnd,
                        WM_SETFONT,
                        Some(WPARAM(font.0 as usize)),
                        Some(LPARAM(1)),
                    );
                }
            }
        }
        let mut positioned = vec![(preview.do_it_btn, metrics.buttons.do_it)];
        if let (Some(edit_btn), Some(edit_rect)) = (preview.edit_btn, metrics.buttons.edit) {
            positioned.push((edit_btn, edit_rect));
        }
        for (hwnd, rect) in positioned {
            if hwnd.0.is_null() {
                continue;
            }
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER,
                );
                SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
        }
    }

    fn layout_preview(&mut self) {
        // A preview shown from Hidden has no position of its own yet: read
        // the pointer afresh (see `place_from_cursor`).
        let from_cursor = std::mem::take(&mut self.place_from_cursor);
        let work = self.target_work_area(from_cursor);
        let fields = match &self.preview {
            Some(p) => p.model.fields().to_vec(),
            None => return,
        };
        let main_window_exists = self
            .preview
            .as_ref()
            .map(|p| p.main_window_exists)
            .unwrap_or(false);
        let metrics = self.compute_preview_layout(&fields, main_window_exists);
        // A preview that replaces a visible card does not track the cursor
        // the way Pending/Collapsed do: once a form is up, the user's mouse
        // is likely to move away while they read or type, and having the
        // card hop monitors mid-edit would be actively hostile. Expanded
        // makes the same choice for the same reason.
        self.place_bottom_right(work, metrics.width, metrics.window_h);
    }

    /// The preview form's full geometry: the title line, one label/value
    /// row per field (in `fields`' order, which is schema order -- see
    /// `PreviewModel::from_schema`), and the button row. Pure geometry:
    /// used both to position real child controls and to paint the
    /// non-editable rows' labels/values, so the two can never drift apart.
    ///
    /// `show_edit` mirrors `main_window_exists` (#352): when `false`, no
    /// "Edit" rect is produced at all (`buttons.edit` is `None`) and "Do it"
    /// / "Cancel" are spread across the freed width instead of leaving a
    /// blank column where "Edit" used to sit.
    fn compute_preview_layout(&self, fields: &[Field], show_edit: bool) -> PreviewLayoutMetrics {
        let padding = self.scale(PADDING_DP);
        let gap = self.scale(GAP_DP);
        let width = self.scale(PREVIEW_WIDTH_DP);
        let content_left = padding;
        let content_right = width - padding;
        let content_width = (content_right - content_left).max(1);

        let title_h = self.line_height(self.fonts.headline).max(1);
        let mut y = padding;
        let title_rect = RECT {
            left: content_left,
            top: y,
            right: content_left + content_width,
            bottom: y + title_h,
        };
        y += title_h + gap;

        // #817: a rejected edit keeps the title and shows its message on a
        // line of its own under it, instead of replacing the title.
        let error_rect = self
            .preview
            .as_ref()
            .and_then(|p| p.error.as_deref())
            .map(|message| {
                let text = preview_error_title(message, self.theme);
                let h = self
                    .measure_wrapped(self.fonts.body, &text, content_width)
                    .max(self.line_height(self.fonts.body).max(1));
                let rect = RECT {
                    left: content_left,
                    top: y,
                    right: content_right,
                    bottom: y + h,
                };
                y += h + gap;
                rect
            });

        // The row is also an editable field's box: at least the design
        // height, and always the text line plus the padding above and below
        // (a larger text scale must not squeeze the padding away, #811).
        let row_h = self
            .scale(PREVIEW_ROW_H_DP)
            .max(self.line_height(self.fonts.body) + 2 * self.scale(FIELD_PAD_Y_DP));
        let row_gap = self.scale(PREVIEW_ROW_GAP_DP);
        // #355: the label column used to be a fixed 84dp, truncating a long
        // label (e.g. "Date of birth") to "Date of bi..." exactly when the
        // user is checking what is about to be written. Size it from the
        // widest label actually shown instead, clamped between the old
        // 84dp floor and 45% of the content width so one very long label
        // cannot squeeze the value column away.
        let label_min_w = self.scale(PREVIEW_LABEL_W_DP).min(content_width / 2).max(1);
        let label_max_w = ((content_width as f32) * PREVIEW_LABEL_MAX_FRACTION) as i32;
        let full_label_w: Vec<i32> = fields
            .iter()
            .map(|f| self.measure_label(self.fonts.body, &f.label).0)
            .collect();
        // A multi-line field, or a label too long for even the widest
        // column, stacks (label above value) and so must not widen the
        // label column of the other rows (#813).
        let measured_label_w: Vec<i32> = fields
            .iter()
            .zip(full_label_w.iter())
            .map(|(f, &w)| if f.multiline || w > label_max_w { 0 } else { w })
            .collect();
        let label_w = label_column_width(&measured_label_w, label_min_w, label_max_w);
        let value_x = content_left + label_w + self.scale(6);
        let value_w = (content_right - value_x).max(1);
        // The text column of a read-only value: the same inset the paint
        // applies, so "fits" here means "fits on screen".
        let value_text_w = (value_w - self.scale(FIELD_PAD_X_DP)).max(1);
        let mut rows = Vec::with_capacity(fields.len());
        for (field, &full_w) in fields.iter().zip(full_label_w.iter()) {
            let stacked = row_is_stacked(
                field.multiline,
                full_w,
                label_max_w,
                (!field.editable).then(|| self.measure_label(self.fonts.body, &field.value).0),
                value_text_w,
            );
            if stacked {
                // #105, #813: the label on its own line (wrapped when it is
                // wider than the card), then the value across the full
                // width. A multi-line or read-only value wraps, capped so a
                // long OCR dump cannot push the buttons off the screen (the
                // last visible line ends in an ellipsis when the text is
                // longer); a one-line editable field keeps its one-line box.
                let line_h = self.line_height(self.fonts.body).max(1);
                let label_wrapped = full_w > content_width;
                let label_h = if label_wrapped {
                    // Top-aligned lines get the air a centred single line has.
                    self.measure_wrapped(self.fonts.body, &field.label, content_width)
                        + (row_h - line_h).max(0) / 2
                } else {
                    row_h
                };
                let wrapped_h = self.measure_wrapped(
                    self.fonts.body,
                    &field.value,
                    content_width - self.scale(FIELD_PAD_X_DP) * 2,
                );
                let text_h = wrapped_h.clamp(line_h, line_h * PREVIEW_MULTILINE_MAX_LINES);
                // An editable field is a framed box, so its text needs room
                // inside the frame (#811).
                let value_h = if field.editable && !field.multiline {
                    row_h
                } else if field.editable {
                    text_h + 2 * self.scale(FIELD_PAD_Y_DP)
                } else {
                    text_h
                };
                let label_rect = RECT {
                    left: content_left,
                    top: y,
                    right: content_right,
                    bottom: y + label_h,
                };
                let value_rect = RECT {
                    left: content_left,
                    top: y + label_h,
                    right: content_right,
                    bottom: y + label_h + value_h,
                };
                rows.push(PreviewRowMetrics {
                    label_rect,
                    value_rect,
                    label_wrapped,
                    value_stacked: true,
                });
                y += label_h + value_h + row_gap;
                continue;
            }
            let measured_w = full_w;
            // A label that still does not fit the (clamped) column wraps to
            // a second line rather than ellipsizing, per #355's "Done
            // when": the whole label must be visible, not just wider.
            let label_wrapped = measured_w > label_w;
            let row_content_h = if label_wrapped {
                self.measure_wrapped(self.fonts.body, &field.label, label_w)
                    .max(row_h)
            } else {
                row_h
            };
            let label_rect = RECT {
                left: content_left,
                top: y,
                right: content_left + label_w,
                bottom: y + row_content_h,
            };
            // When the label wraps to two lines, the value stays a single
            // line top-aligned with the label's *first* line rather than
            // vertically centred in the now-taller row (review nit on
            // #355): `row_h` here, not `row_content_h`.
            let value_rect = RECT {
                left: value_x,
                top: y,
                right: value_x + value_w,
                bottom: y + row_h,
            };
            rows.push(PreviewRowMetrics {
                label_rect,
                value_rect,
                label_wrapped,
                value_stacked: false,
            });
            y += row_content_h + row_gap;
        }
        y = if fields.is_empty() {
            y + gap
        } else {
            y - row_gap + gap
        };

        let note_rect = self
            .preview
            .as_ref()
            .and_then(|p| p.note.as_deref())
            .map(|text| {
                let h = self
                    .measure_wrapped(self.fonts.body, text, content_width)
                    .max(self.line_height(self.fonts.body).max(1));
                let rect = RECT {
                    left: content_left,
                    top: y,
                    right: content_right,
                    bottom: y + h,
                };
                y += h + gap;
                rect
            });

        let btn_h = self.scale(PREVIEW_BUTTON_H_DP);
        let btn_w = self.scale(PREVIEW_BUTTON_W_DP);
        let do_it = RECT {
            left: content_right - btn_w,
            top: y,
            right: content_right,
            bottom: y + btn_h,
        };
        // #392: Cancel is gone (nothing has run yet, so there is nothing to
        // undo). "Edit", when present, keeps its spot at the content's left
        // edge; "Do it" stays right-aligned. With no "Edit" either, the left
        // side is simply empty rather than filled by a button that no
        // longer exists.
        let edit = if show_edit {
            Some(RECT {
                left: content_left,
                top: y,
                right: content_left + btn_w,
                bottom: y + btn_h,
            })
        } else {
            None
        };
        y += btn_h;

        let window_h = y + padding;

        PreviewLayoutMetrics {
            width,
            window_h,
            title_rect,
            error_rect,
            note_rect,
            rows,
            buttons: PreviewButtonMetrics { do_it, edit },
        }
    }

    /// The frame of an editable field (#811): rounded 1 px border on the
    /// lifted fill, and the accent underline while the field has focus.
    unsafe fn paint_field_frame(&self, hdc: HDC, frame: &RECT, palette: &Palette, focused: bool) {
        use windows::Win32::Foundation::COLORREF;
        let look = field_look(palette, self.theme, self.sys.highlight);
        let radius = self.scale(CONTROL_RADIUS_DP);
        let brush = CreateSolidBrush(COLORREF(look.fill));
        let pen = CreatePen(PS_SOLID, 1, COLORREF(look.border));
        let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
        let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));
        let _ = RoundRect(
            hdc,
            frame.left,
            frame.top,
            frame.right,
            frame.bottom,
            radius * 2,
            radius * 2,
        );
        if focused && self.theme == Theme::HighContrast {
            // High contrast: a 2 px ring all round in the system highlight
            // colour, so focus is not the same ink as the border.
            let ring_pen = CreatePen(
                PS_SOLID,
                self.scale(FIELD_UNDERLINE_DP).max(2),
                COLORREF(look.underline),
            );
            let prev = SelectObject(hdc, HGDIOBJ(ring_pen.0));
            let inset = 1;
            let _ = RoundRect(
                hdc,
                frame.left + inset,
                frame.top + inset,
                frame.right - inset,
                frame.bottom - inset,
                radius * 2,
                radius * 2,
            );
            SelectObject(hdc, prev);
            let _ = DeleteObject(HGDIOBJ(ring_pen.0));
        } else if focused {
            let thickness = self.scale(FIELD_UNDERLINE_DP).max(2);
            let line = RECT {
                left: frame.left + radius,
                top: frame.bottom - thickness,
                right: frame.right - radius,
                bottom: frame.bottom,
            };
            let accent = CreateSolidBrush(COLORREF(look.underline));
            FillRect(hdc, &line, accent);
            let _ = DeleteObject(HGDIOBJ(accent.0));
        }
        SelectObject(hdc, old_pen);
        SelectObject(hdc, old_brush);
        let _ = DeleteObject(HGDIOBJ(pen.0));
        let _ = DeleteObject(HGDIOBJ(brush.0));
    }

    /// Paints the title line, every field's label, and every *non-editable*
    /// field's value (an editable field's value is shown by its live EDIT
    /// control instead, drawn by Windows on top of this same client area).
    unsafe fn paint_preview(&self, hdc: HDC, palette: &Palette) {
        let Some(preview) = self.preview.as_ref() else {
            return;
        };
        let fields = preview.model.fields();
        let metrics = self.compute_preview_layout(fields, preview.main_window_exists);

        let title_rect = metrics.title_rect;
        SelectObject(hdc, HGDIOBJ(self.fonts.headline.0));
        SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
        draw_text_line(
            hdc,
            &preview.title,
            title_rect,
            DT_LEFT | DT_TOP | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS,
        );
        if let (Some(rect), Some(message)) = (metrics.error_rect, preview.error.as_deref()) {
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.danger));
            draw_text_line(
                hdc,
                &preview_error_title(message, self.theme),
                rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX,
            );
        }

        for (field, row) in fields.iter().zip(metrics.rows.iter()) {
            let label_rect = row.label_rect;
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            // #355: a label that did not fit the clamped column even at its
            // widest wraps to a second line instead of ellipsizing, so the
            // whole label stays readable.
            let label_format = if row.label_wrapped {
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX
            } else {
                DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS
            };
            draw_text_line(hdc, &field.label, label_rect, label_format);

            if field.editable {
                let focused = preview
                    .edits
                    .iter()
                    .any(|(name, h)| *name == field.name && GetFocus() == *h);
                self.paint_field_frame(hdc, &row.value_rect, palette, focused);
            } else {
                // The text lines up with the text inside the framed fields.
                let mut value_rect = row.value_rect;
                value_rect.left += self.scale(FIELD_PAD_X_DP);
                SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
                SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.headline));
                draw_text_line(
                    hdc,
                    &field.value,
                    value_rect,
                    value_draw_format(row.value_stacked),
                );
            }
        }

        if let (Some(rect), Some(note)) = (metrics.note_rect, preview.note.as_deref()) {
            SelectObject(hdc, HGDIOBJ(self.fonts.body.0));
            SetTextColor(hdc, windows::Win32::Foundation::COLORREF(palette.hint));
            draw_text_line(
                hdc,
                note,
                rect,
                DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Preview (confirmation) state -- layout constants, child-control helpers,
// keyboard mapping, and the shared control subclass (#26)
// ---------------------------------------------------------------------------

const PREVIEW_WIDTH_DP: i32 = 320;
/// Height of one preview row, which is also the box of an editable field.
const PREVIEW_ROW_H_DP: i32 = 28;
const PREVIEW_ROW_GAP_DP: i32 = 8;
/// Space between an editable field's border and its text, left and right.
const FIELD_PAD_X_DP: i32 = 8;
/// The same, above and below, for a multi-line editable field.
const FIELD_PAD_Y_DP: i32 = 4;
/// Thickness of the focus underline on an editable field.
const FIELD_UNDERLINE_DP: i32 = 2;
const PREVIEW_LABEL_W_DP: i32 = 84;
const PREVIEW_BUTTON_H_DP: i32 = 26;
const PREVIEW_BUTTON_W_DP: i32 = 84;
/// Upper bound on the label column as a fraction of the content width
/// (#355): even the widest label never pushes the value column below 55%
/// of the available space.
const PREVIEW_LABEL_MAX_FRACTION: f32 = 0.45;
/// Most lines a multi-line preview value (#105, the send preview's prompt and
/// text) may take before it ends in an ellipsis.
const PREVIEW_MULTILINE_MAX_LINES: i32 = 10;

/// How a read-only value is drawn (#813): a stacked value wraps from the top
/// of its box (the box is as tall as the wrapped text), a beside-the-label
/// value is one centred line.
fn value_draw_format(stacked: bool) -> windows::Win32::Graphics::Gdi::DRAW_TEXT_FORMAT {
    if stacked {
        DT_LEFT | DT_TOP | DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS
    } else {
        DT_LEFT | DT_VCENTER | DT_SINGLELINE | DT_NOPREFIX | DT_END_ELLIPSIS
    }
}

/// Whether a preview row puts its label above its value instead of beside
/// it (#813): a multi-line field always does, and so does a label too wide
/// for even the widest label column, or a read-only one-line value
/// (`value_w`, `None` for an editable field) wider than the value column,
/// so the note the user needs to see ("(from profile)") is never cut off.
fn row_is_stacked(
    multiline: bool,
    label_w: i32,
    label_max_w: i32,
    value_w: Option<i32>,
    value_col_w: i32,
) -> bool {
    multiline || label_w > label_max_w || value_w.is_some_and(|w| w > value_col_w)
}
/// Sizes the preview label column from the widest measured label (#355),
/// clamped between `min_w` (the old fixed 84dp floor) and `max_w` (45% of
/// the content width). Pure and DPI-agnostic: callers scale `measured`,
/// `min_w` and `max_w` to pixels first, so this same function is exercised
/// at every DPI by `label_column_width_is_clamped_at_several_dpis` below --
/// it never needs to know what DPI produced its inputs.
fn label_column_width(measured: &[i32], min_w: i32, max_w: i32) -> i32 {
    let max_w = max_w.max(min_w);
    let widest = measured.iter().copied().max().unwrap_or(min_w);
    widest.clamp(min_w, max_w)
}

const ID_PREVIEW_DO_IT: i32 = 3900;
const ID_PREVIEW_EDIT: i32 = 3901;
const ID_PREVIEW_CANCEL: i32 = 3902;

/// Subclass id passed to `SetWindowSubclass`/`RemoveWindowSubclass`. A
/// single constant is enough: every preview child control uses the same
/// subclass procedure, so nothing here ever needs to tell two subclasses on
/// the same control apart.
const PREVIEW_SUBCLASS_ID: usize = 1;

fn button_is_hovered(hwnd: HWND) -> bool {
    use windows::Win32::UI::WindowsAndMessaging::GetPropW;
    let name = wide_z(BUTTON_HOVER_PROP);
    !unsafe { GetPropW(hwnd, PCWSTR(name.as_ptr())) }.0.is_null()
}

fn set_button_hover(hwnd: HWND, hovered: bool) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Graphics::Gdi::InvalidateRect;
    use windows::Win32::UI::WindowsAndMessaging::{RemovePropW, SetPropW};
    if button_is_hovered(hwnd) == hovered {
        return;
    }
    let name = wide_z(BUTTON_HOVER_PROP);
    unsafe {
        if hovered {
            let _ = SetPropW(hwnd, PCWSTR(name.as_ptr()), Some(HANDLE(1 as *mut c_void)));
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut track);
        } else {
            let _ = RemovePropW(hwnd, PCWSTR(name.as_ptr()));
        }
        let _ = InvalidateRect(Some(hwnd), None, false);
    }
}

fn is_owner_drawn_button(hwnd: HWND) -> bool {
    let style =
        unsafe { GetWindowLongPtrW(hwnd, windows::Win32::UI::WindowsAndMessaging::GWL_STYLE) };
    crate::ui::card_buttons::is_owner_drawn(style as u32)
}

const WC_EDIT: &str = "EDIT";
const WC_BUTTON: &str = "BUTTON";
const ES_AUTOHSCROLL: u32 = 0x0080;
/// `WM_MOUSELEAVE`'s stable, documented value (winuser.h).
const WM_MOUSELEAVE: u32 = 0x02A3;
/// Window property set on an owner-drawn preview button while the mouse is
/// over it (#351), read by the `WM_DRAWITEM` handler. No timer: the subclass
/// sets it on `WM_MOUSEMOVE` and clears it on `WM_MOUSELEAVE`.
const BUTTON_HOVER_PROP: &str = "Wingman.Card.ButtonHover";

/// Live state for `CardState::Preview`. Owned by `CardInner.preview`; exists
/// for exactly as long as the preview's child controls do (see
/// `leave_preview_if_active`).
struct PreviewUi {
    model: PreviewModel,
    /// The heading line drawn above the fields -- the action's name (e.g.
    /// "Add to calendar"), not a schema field.
    title: String,
    /// One `(schema field name, EDIT HWND)` pair per *editable* field, in
    /// the same order `PreviewModel::fields()` returns them. Non-editable
    /// fields have no entry here; their value is painted, not typed into.
    edits: Vec<(String, HWND)>,
    do_it_btn: HWND,
    /// `None` when `main_window_exists` was `false` at `show_preview` time
    /// (#352): the button is not created at all, not just disabled.
    edit_btn: Option<HWND>,
    /// `GetForegroundWindow()` at the moment `show_preview` was called.
    /// `close_preview` hands the foreground back to this window (if it
    /// still exists) so the preview's interruption is temporary -- see
    /// `Card::show_preview`'s "Focus" note.
    previous_foreground: HWND,
    /// Whether Wingman's main window exists yet -- gates whether the "Edit"
    /// button is created enabled. See `Card::show_preview`'s doc comment.
    main_window_exists: bool,
    /// An edit that cannot be read as its field's type (#284), shown in place
    /// of the title until "Do it" is pressed again with every edit valid.
    error: Option<String>,
    /// The confirm button's text: "Do it" for an action preview, "Send" for
    /// the send preview (#105).
    confirm_label: &'static str,
    /// Whether Enter presses the confirm button; `false` for the send
    /// preview (#105), which is click-only.
    enter_confirms: bool,
    /// #228: a small display-only line under the fields, naming the model
    /// that answered when a fallback did. Not a field: never confirmed.
    note: Option<String>,
}

struct PreviewRowMetrics {
    label_rect: RECT,
    value_rect: RECT,
    /// Whether this row's label is too wide for the (clamped) label column
    /// even after `label_column_width` picked the widest label it could,
    /// so it must be drawn wrapped (`DT_WORDBREAK`) instead of ellipsized
    /// on one line (#355).
    label_wrapped: bool,
    /// The value sits under its label on the full width (#813), and is
    /// drawn wrapped from the top, not on one centred line.
    value_stacked: bool,
}

struct PreviewButtonMetrics {
    do_it: RECT,
    /// `None` when the layout was computed with `show_edit: false` (#352).
    edit: Option<RECT>,
}

struct PreviewLayoutMetrics {
    width: i32,
    window_h: i32,
    title_rect: RECT,
    /// #817: where a rejected edit's message goes, `None` when there is none.
    error_rect: Option<RECT>,
    /// #228: where the footer note goes, `None` when there is none.
    note_rect: Option<RECT>,
    /// Same length and order as the `fields` slice `compute_preview_layout`
    /// was called with.
    rows: Vec<PreviewRowMetrics>,
    buttons: PreviewButtonMetrics,
}

/// Pure keyboard mapping for the preview state (#26): Enter means "Do it",
/// Esc means "Cancel", everything else is not a preview command. Shared by
/// the card's own `WM_KEYDOWN` arm and [`preview_control_subclass`] (every
/// EDIT/BUTTON child forwards through the same mapping), so both agree by
/// construction rather than by two hand-kept switch statements. Pure and
/// unit-tested without a live window -- see the `tests` module below.
///
/// #105: `enter_confirms` is `false` for the send preview, where Enter does
/// nothing. A key press meant for another window can land on the card the
/// instant it takes focus, and that must never send a screenshot: Send is
/// click-only, Esc still cancels.
fn preview_key_command(vk: u16, enter_confirms: bool) -> Option<i32> {
    if vk == VK_RETURN.0 && enter_confirms {
        Some(ID_PREVIEW_DO_IT)
    } else if vk == VK_ESCAPE.0 {
        Some(ID_PREVIEW_CANCEL)
    } else {
        None
    }
}

/// The confirm button's text on the send preview (#105).
const SEND_CONFIRM_LABEL: &str = "Send";

/// Whether Enter may press the confirm button of a preview whose button reads
/// `confirm_label`. Only the send preview refuses it (see
/// [`preview_key_command`]).
fn label_confirms_with_enter(confirm_label: &str) -> bool {
    confirm_label != SEND_CONFIRM_LABEL
}

/// Whether Enter confirms the preview showing in `card`. Read-only, same
/// reasoning as [`live_preview_generation`].
unsafe fn live_preview_enter_confirms(card: HWND) -> bool {
    let ptr = GetWindowLongPtrW(card, GWLP_USERDATA) as *const CardInner;
    if ptr.is_null() {
        return true;
    }
    match &*std::ptr::addr_of!((*ptr).preview) {
        Some(p) => p.enter_confirms,
        None => true,
    }
}

/// Whether a command posted for `posted_generation` still belongs to the
/// preview showing now (#255).
fn preview_command_is_current(posted_generation: u32, live_generation: u32) -> bool {
    posted_generation == live_generation
}

/// The generation of the preview currently showing in `card`, read off the
/// window's own `CardInner`. Read-only and on the card's own thread, so it
/// cannot race. Goes through `addr_of!` because `CardInner` may already be
/// mutably borrowed further up this same call stack.
unsafe fn live_preview_generation(card: HWND) -> u32 {
    let ptr = GetWindowLongPtrW(card, GWLP_USERDATA) as *const CardInner;
    if ptr.is_null() {
        0
    } else {
        std::ptr::addr_of!((*ptr).preview_generation).read()
    }
}

/// Issue #207: pure decision for a preview child control's `WM_KILLFOCUS`
/// -- whether the new focus target means "click away" (cancel the preview,
/// same as Esc) or staying within the card's own window group (do nothing:
/// Tab moving between two preview fields, or focus landing on the Do
/// it/Edit button itself right before its own click fires).
/// `new_focus_is_card_or_descendant` is
/// `new_focus_hwnd == card_hwnd || IsChild(card_hwnd, new_focus_hwnd)`,
/// computed by the caller (both need a live `HWND` comparison/`IsChild`
/// call, so cannot be done here). `WM_KILLFOCUS`'s own "nothing is gaining
/// focus" case (`wparam == 0`, e.g. the whole app losing the foreground to
/// nothing in particular) needs no separate case: a null `HWND` is never
/// the card's own and `IsChild` is never true for it, so it already
/// computes `new_focus_is_card_or_descendant == false` the same as any
/// other outside window.
fn preview_focus_left_the_card(new_focus_is_card_or_descendant: bool) -> bool {
    !new_focus_is_card_or_descendant
}

/// Subclass installed on every preview EDIT/BUTTON child control so Enter
/// and Esc work regardless of which control currently has keyboard focus.
///
/// This cannot be left to the integrating app's message loop: this module's
/// own doc comment promises callers a plain `GetMessageW`/`DispatchMessageW`
/// loop is enough to run the card (no `IsDialogMessageW` translation, unlike
/// `ui::settings`'s modal loop). A child control's `WM_KEYDOWN` is delivered
/// to the CHILD's own window procedure, never to the parent card's, so
/// without this subclass Enter/Esc would only work while the card's own
/// `HWND` itself happened to have focus (which it never does once a child
/// control does) -- exactly the "wired to nothing" shape this crate's own
/// skill warns about for a low-level hook with no message loop backing it.
///
/// `dwrefdata` carries the parent card's `HWND` (as a `usize`, set at
/// subclass time by [`subclass_preview_control`]) so a mapped key can
/// `PostMessageW` a [`WM_CARD_PREVIEW_COMMAND`] back to it, tagged with the
/// preview generation so a late delivery cannot hit a newer preview (#255).
///
/// Issue #207: the same `dwrefdata` is what makes click-away cancellation
/// possible here too. `WM_KILLFOCUS` on the card's own `HWND` (handled in
/// `CardInner::handle_message`) never fires while a preview is showing,
/// because Preview's fields are real EDIT/BUTTON children -- focus moves
/// between THEM, not off the card window itself, until it leaves the whole
/// group. So this subclass, installed on every one of those children,
/// checks each `WM_KILLFOCUS`'s own new-focus target (`wparam`) against the
/// card window group via [`preview_focus_left_the_card`] and posts
/// `ID_PREVIEW_CANCEL` the same way Enter/Esc already do when it has.
unsafe extern "system" fn preview_control_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _subclass_id: usize,
    ref_data: usize,
) -> LRESULT {
    let parent = HWND(ref_data as *mut c_void);
    if msg == WM_KEYDOWN {
        if let Some(command_id) =
            preview_key_command(wparam.0 as u16, live_preview_enter_confirms(parent))
        {
            let _ = PostMessageW(
                Some(parent),
                WM_CARD_PREVIEW_COMMAND,
                WPARAM(command_id as usize),
                LPARAM(live_preview_generation(parent) as isize),
            );
            return LRESULT(0);
        }
    }
    if msg == WM_KILLFOCUS {
        let new_focus = HWND(wparam.0 as *mut c_void);
        let new_focus_is_card_or_descendant =
            new_focus == parent || unsafe { IsChild(parent, new_focus) }.as_bool();
        if preview_focus_left_the_card(new_focus_is_card_or_descendant) {
            let _ = PostMessageW(
                Some(parent),
                WM_CARD_PREVIEW_COMMAND,
                WPARAM(ID_PREVIEW_CANCEL as usize),
                LPARAM(live_preview_generation(parent) as isize),
            );
        }
    }
    if msg == WM_MOUSEMOVE && is_owner_drawn_button(hwnd) {
        set_button_hover(hwnd, true);
    }
    if msg == WM_MOUSELEAVE && is_owner_drawn_button(hwnd) {
        set_button_hover(hwnd, false);
    }
    if msg == WM_NCDESTROY {
        set_button_hover(hwnd, false);
        let _ = RemoveWindowSubclass(hwnd, Some(preview_control_subclass), PREVIEW_SUBCLASS_ID);
    }
    let result = DefSubclassProc(hwnd, msg, wparam, lparam);
    // #811: the focus cue is painted by the card, so it must repaint when
    // focus moves between fields. Each field sends its own message, so only
    // that field's frame is invalidated (the control is redrawn with it,
    // since the card's paint covers it).
    if (msg == WM_SETFOCUS || msg == WM_KILLFOCUS) && !is_owner_drawn_button(hwnd) {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_ok() {
            let mut corners = [
                POINT {
                    x: r.left,
                    y: r.top,
                },
                POINT {
                    x: r.right,
                    y: r.bottom,
                },
            ];
            MapWindowPoints(None, Some(parent), &mut corners);
            let dpi = GetDpiForWindow(parent).max(96) as i32;
            let dirty = field_frame_dirty_rect(
                &RECT {
                    left: corners[0].x,
                    top: corners[0].y,
                    right: corners[1].x,
                    bottom: corners[1].y,
                },
                dpi,
            );
            let _ = RedrawWindow(
                Some(parent),
                Some(&dirty),
                None,
                RDW_INVALIDATE | RDW_ALLCHILDREN,
            );
        }
    }
    result
}

fn subclass_preview_control(hwnd: HWND, parent: HWND) {
    unsafe {
        let _ = SetWindowSubclass(
            hwnd,
            Some(preview_control_subclass),
            PREVIEW_SUBCLASS_ID,
            parent.0 as usize,
        );
    }
}

/// #743: where a preview's EDIT control goes inside its row's value cell. A
/// plain value is drawn vertically centred in the cell, but an EDIT draws its
/// text from the top of its own box, so a full-height box put the text above
/// the label and the plain values. One text line high, centred, lines up.
/// The part of the card to repaint when `edit` (client coordinates) gains or
/// loses focus: its frame, i.e. the edit grown by the field padding, and by
/// the row's vertical slack above and below (the underline sits on the
/// frame's bottom edge).
fn field_frame_dirty_rect(edit: &RECT, dpi: i32) -> RECT {
    let px = |dp: i32| (dp * dpi + 48) / 96;
    let (x, y) = (px(FIELD_PAD_X_DP), px(PREVIEW_ROW_H_DP) / 2);
    RECT {
        left: edit.left - x,
        top: edit.top - y,
        right: edit.right + x,
        bottom: edit.bottom + y,
    }
}

fn field_edit_rect(frame: &RECT, line_h: i32, multiline: bool, pad_x: i32, pad_y: i32) -> RECT {
    let inner = if multiline {
        RECT {
            left: frame.left,
            top: frame.top + pad_y,
            right: frame.right,
            bottom: (frame.bottom - pad_y).max(frame.top + pad_y + 1),
        }
    } else {
        centered_edit_rect(frame, line_h)
    };
    RECT {
        left: inner.left + pad_x,
        right: (inner.right - pad_x).max(inner.left + pad_x + 1),
        ..inner
    }
}

fn centered_edit_rect(cell: &RECT, line_h: i32) -> RECT {
    let line_h = line_h.clamp(1, (cell.bottom - cell.top).max(1));
    let top = cell.top + ((cell.bottom - cell.top) - line_h) / 2;
    RECT {
        left: cell.left,
        top,
        right: cell.right,
        bottom: top + line_h,
    }
}

/// `EM_SETMARGINS` with `EC_LEFTMARGIN | EC_RIGHTMARGIN` and no margin, so
/// an edit's text starts where a plain value's does (#743).
fn clear_edit_margins(edit: HWND) {
    const EM_SETMARGINS: u32 = 0x00D3;
    const EC_LEFTMARGIN_AND_RIGHT: usize = 0x0001 | 0x0002;
    unsafe {
        SendMessageW(
            edit,
            EM_SETMARGINS,
            Some(WPARAM(EC_LEFTMARGIN_AND_RIGHT)),
            Some(LPARAM(0)),
        );
    }
}

fn create_preview_edit(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    initial_text: &str,
    rect: &RECT,
) -> Option<HWND> {
    let class_w = wide_z(WC_EDIT);
    let text_w = wide_z(initial_text);
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | ES_AUTOHSCROLL,
            ),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(parent),
            None,
            Some(instance),
            None,
        )
    };
    let hwnd = hwnd.ok()?;
    unsafe {
        SendMessageW(
            hwnd,
            WM_SETFONT,
            Some(WPARAM(font.0 as usize)),
            Some(LPARAM(1)),
        );
    }
    clear_edit_margins(hwnd);
    Some(hwnd)
}

/// A preview button. Owner-drawn (#351) so it follows the card's theme and
/// the system accent; Enter, Space and Tab behave as for any push button.
fn create_preview_button(
    parent: HWND,
    instance: HINSTANCE,
    font: HFONT,
    text: &str,
    rect: &RECT,
    id: i32,
    enabled: bool,
) -> HWND {
    let class_w = wide_z(WC_BUTTON);
    let text_w = wide_z(text);
    let style = WS_CHILD.0
        | WS_VISIBLE.0
        | WS_TABSTOP.0
        | crate::ui::card_buttons::BS_OWNERDRAW
        | if enabled { 0 } else { WS_DISABLED.0 };
    let hwnd = unsafe {
        CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR(class_w.as_ptr()),
            PCWSTR(text_w.as_ptr()),
            windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(style),
            rect.left,
            rect.top,
            rect.right - rect.left,
            rect.bottom - rect.top,
            Some(parent),
            Some(HMENU(id as *mut c_void)),
            Some(instance),
            None,
        )
    };
    match hwnd {
        Ok(hwnd) => {
            unsafe {
                SendMessageW(
                    hwnd,
                    WM_SETFONT,
                    Some(WPARAM(font.0 as usize)),
                    Some(LPARAM(1)),
                );
            }
            hwnd
        }
        Err(_) => HWND(std::ptr::null_mut()),
    }
}

/// `GetWindowTextW` into an owned `String`, `""` for a null/invalid handle
/// rather than a panic -- mirrors `ui::settings::get_text`.
fn window_text(hwnd: HWND) -> String {
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

/// A point at distance `len` from `(cx, cy)` along the ray at `angle_deg`
/// (standard math convention: 0 deg is +x, increasing counterclockwise).
/// Used to give `Arc` the two boundary points that define where the
/// spinner's sweep begins and ends.
fn ray_point(cx: i32, cy: i32, angle_deg: f32, len: i32) -> (i32, i32) {
    let rad = angle_deg.to_radians();
    let x = cx + (len as f32 * rad.cos()).round() as i32;
    let y = cy - (len as f32 * rad.sin()).round() as i32;
    (x, y)
}

/// Issue #109: every monitor, numbered left to right (what
/// `ui.card_monitor = "2"` indexes into; see `sort_left_to_right`).
fn sorted_monitors() -> Vec<(RECT, windows::Win32::Graphics::Gdi::HMONITOR)> {
    use windows::Win32::Graphics::Gdi::HMONITOR;
    unsafe extern "system" fn collect(
        hmon: HMONITOR,
        _hdc: HDC,
        rc: *mut RECT,
        lparam: LPARAM,
    ) -> windows::core::BOOL {
        let out = &mut *(lparam.0 as *mut Vec<(RECT, HMONITOR)>);
        out.push((*rc, hmon));
        true.into()
    }
    let mut out: Vec<(RECT, HMONITOR)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut out as *mut Vec<(RECT, HMONITOR)> as isize),
        );
    }
    sort_left_to_right(&mut out);
    out
}

/// Issue #498: how many monitors are connected, for the Settings list of
/// numbered displays. Counted the same way `sorted_monitors` numbers them.
pub(crate) fn monitor_count() -> usize {
    sorted_monitors().len()
}

fn work_area_for_monitor(hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> RECT {
    unsafe {
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return mi.rcWork;
        }
        // Fall back to the primary monitor's work area.
        let mut rc = RECT::default();
        let _ = SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rc as *mut _ as *mut c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
        rc
    }
}

// ---------------------------------------------------------------------------
// Small string helpers
// ---------------------------------------------------------------------------

/// UTF-16, null-terminated — for Win32 APIs that expect a `PCWSTR`.
fn wide_z(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// UTF-16, *not* null-terminated — for `DrawTextW`, which takes an explicit
/// slice length rather than scanning for a terminator.
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

// ---------------------------------------------------------------------------
// Smoke test — exercises real Win32 window creation/destruction, not just
// compilation. `cargo test` runs this headless; no window is ever shown
// (`show_*` only calls `ShowWindow(SW_SHOWNOACTIVATE)`, which does not steal
// focus or require an interactive desktop message pump to succeed).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme;

    /// #723: the pending card must outlive the slowest honest request: every
    /// provider in the chain at its own wall clock.
    #[test]
    fn pending_safety_timeout_outlasts_a_full_provider_chain() {
        let timeout = u64::from(std::hint::black_box(PENDING_SAFETY_TIMEOUT_SECS));
        assert!(timeout >= 4 * crate::provider::common::MAX_TOTAL_WALL_CLOCK_SECS);
        assert!(timeout >= 4 * crate::provider::ollama::TOTAL_TIMEOUT_SECS);
    }

    use windows::Win32::System::LibraryLoader::GetModuleHandleW;

    fn instance() -> HINSTANCE {
        let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
        HINSTANCE(h.0)
    }

    #[test]
    fn create_show_hide_drop_smoke_test() {
        let mut card = Card::new(instance()).expect("Card::new");
        assert!(!card.hwnd().0.is_null());
        assert_eq!(card.state(), CardState::Hidden);

        card.show_pending(PendingStage::Working);
        assert_eq!(card.state(), CardState::Pending);

        card.show_answer(
            "2 + 2 = 4",
            "You carried correctly.",
            5,
            Some(Difficulty::Level(1)),
        );
        assert_eq!(card.state(), CardState::Collapsed);

        // Simulate the click that expands a collapsed card with detail.
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);

        // Esc while expanded should close the card entirely.
        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_ESCAPE.0 as usize), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Hidden);

        card.show_error("Couldn't capture the screen", "detail text");
        assert_eq!(card.state(), CardState::Collapsed);
        // show_error must clear any badge left over from the previous
        // show_answer call above -- it must never linger on an error card.
        assert_eq!(card.inner.difficulty, None);

        card.hide();
        assert_eq!(card.state(), CardState::Hidden);
        // Dropping must not panic (this exercises DestroyWindow + WM_NCDESTROY).
    }

    /// True when `id` is a live timer on the card's window. `KillTimer`
    /// fails for a timer that does not exist, so killing is the oracle (it
    /// also removes the timer, so call it once per id per check).
    fn timer_was_live(card: &Card, id: usize) -> bool {
        unsafe { KillTimer(Some(card.inner.hwnd), id) }.is_ok()
    }

    #[test]
    fn working_timers_run_only_while_pending_and_die_on_every_exit() {
        // Issue #354, AGENTS.md rule 5: nothing runs while idle.
        let model = || PendingStage::AskingModel {
            model_name: "test-model".to_string(),
        };

        // Pending: the safety timer and the "still working" countdown are
        // armed; the spinner timer only when animations are on.
        let mut card = Card::new(instance()).expect("Card::new");
        card.show_pending(model());
        assert!(timer_was_live(&card, TIMER_DISMISS));
        assert!(timer_was_live(&card, TIMER_STILL_WORKING));
        assert_eq!(
            timer_was_live(&card, TIMER_ANIM),
            card.inner.animations_enabled
        );

        // Every way out of Pending leaves no countdown or spinner behind.
        type Exit = fn(&mut Card);
        let exits: [(&str, Exit); 3] = [
            ("answer", |c| c.show_answer("a", "b", 5, None)),
            ("error", |c| c.show_error("a", "b")),
            ("hide", |c| c.hide()),
        ];
        for (name, exit) in exits {
            card.show_pending(model());
            exit(&mut card);
            assert!(
                !timer_was_live(&card, TIMER_STILL_WORKING),
                "{name}: countdown left running"
            );
            assert!(
                !timer_was_live(&card, TIMER_ANIM),
                "{name}: spinner timer left running"
            );
        }

        // A stage with no countdown (and a fresh Pending after a countdown
        // stage) must not inherit the old countdown.
        card.show_pending(model());
        card.show_pending(PendingStage::Working);
        assert!(!timer_was_live(&card, TIMER_STILL_WORKING));

        // The countdown fires once: moving to StillWorking kills it.
        card.show_pending(model());
        card.set_pending_stage(PendingStage::StillWorking);
        assert!(!timer_was_live(&card, TIMER_STILL_WORKING));
        card.hide();
    }

    #[test]
    fn a_settings_needed_card_is_taller_for_its_detail_and_has_no_chevron() {
        // #885: the detail of a click-to-open-Settings card is drawn on the
        // collapsed card, so the window must have room for it.
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let height = |card: &Card| {
            let mut r = RECT::default();
            unsafe {
                let _ = GetWindowRect(card.hwnd(), &mut r);
            }
            r.bottom - r.top
        };
        let detail = "Click here to open Settings and paste an API key, or install Ollama to run models on this PC for free.";
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_settings_needed("No AI model set up yet", "");
        let headline_only = height(&card);
        assert_eq!(card.inner.chevron_reserve(), 0);
        card.show_settings_needed("No AI model set up yet", detail);
        assert!(height(&card) > headline_only, "detail must add height");
        assert!(card.inner.shows_setup_detail());
        assert_eq!(
            card.inner.chevron_reserve(),
            0,
            "a click opens Settings, so no expand chevron"
        );
        // Narrator reads the detail too (the window text is the accessible name).
        assert!(window_text(card.hwnd()).contains("paste an API key"));
        // A very long detail is capped, not taller than a few lines.
        let normal = height(&card);
        card.show_settings_needed("No AI model set up yet", &"word ".repeat(200));
        let long = height(&card);
        let line = card.inner.line_height(card.inner.fonts.caption);
        assert!(long > normal);
        assert!(
            long <= normal + line * SETUP_DETAIL_MAX_LINES,
            "{long} vs {normal}"
        );
        // An ordinary answer card with the same text is unchanged: it keeps
        // the chevron and shows no detail until expanded.
        card.show_answer("No AI model set up yet", detail, 0, None);
        assert!(!card.inner.shows_setup_detail());
        assert!(card.inner.chevron_reserve() > 0);
    }

    #[test]
    fn clicking_a_settings_needed_card_opens_settings_instead_of_expanding() {
        // Issue #347: the old "No API key: open Edit settings" card named a
        // menu item that no longer exists. The fix is that the card's own
        // click opens Settings directly, so this proves the click posts
        // WM_APP_CARD_OPEN_SETTINGS to the owner (same self-notify idiom the
        // preview tests use) and does not just expand the card.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_settings_needed("No AI model set up yet", "Click here to open Settings.");
        assert_eq!(card.state(), CardState::Collapsed);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());

        assert_eq!(
            card.state(),
            CardState::Hidden,
            "clicking a settings-needed card must hide it, not expand it"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "clicking a settings-needed card must post WM_APP_CARD_OPEN_SETTINGS to the owner"
        );
        assert_eq!(msg.message, WM_APP_CARD_OPEN_SETTINGS);
    }

    #[test]
    fn clicking_an_ordinary_error_card_still_expands() {
        // Neighbouring case: show_error (not show_settings_needed) must keep
        // the pre-#347 expand-on-click behaviour.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error("Couldn't capture the screen", "detail text");
        assert_eq!(card.state(), CardState::Collapsed);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Expanded);
    }

    // -- "Copy details" affordance on an expanded error card (#425) --------

    #[test]
    fn expanded_error_card_reserves_a_copy_details_footer() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(
            card.inner.copy_details_rect.is_some(),
            "an expanded, tracked error card must reserve a \"Copy details\" footer"
        );
    }

    #[test]
    fn plain_show_error_never_gets_a_copy_details_footer() {
        // Cold review of #425/PR #451: `show_error` alone (untracked -- no
        // `App::track_error`/`record_last_error` call behind it) must never
        // offer "Copy details", because `App::last_error` may hold a chain
        // from an unrelated, earlier failure. Only `show_error_with_details`
        // -- reserved for callers that just recorded the matching chain --
        // grows the footer.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error("Hotkeys unavailable", "The keyboard hook is not installed.");
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(
            card.inner.copy_details_rect.is_none(),
            "an untracked error card must not show \"Copy details\""
        );
    }

    #[test]
    fn untracked_error_card_after_a_tracked_one_has_no_copy_details_footer() {
        // The exact regression PR #451's cold review caught: a tracked
        // error card's footer must not linger (or, worse, keep pointing at
        // the OLDER chain) once a later, untracked error card replaces it.
        // `show_collapsed` (both `show_answer` and plain `show_error` route
        // through it) unconditionally clears `copy_details_available`, so
        // only the call that most recently used `show_error_with_details`
        // can have left it set -- and a later plain `show_error` call
        // always clears it again.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());

        card.show_error_with_details("Couldn't add the event", "tracked detail");
        card.inner.try_expand();
        assert!(
            card.inner.copy_details_rect.is_some(),
            "the tracked error must show the footer"
        );

        card.show_error("Couldn't restore the tray icon", "untracked detail");
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(
            card.inner.copy_details_rect.is_none(),
            "an untracked error card must never inherit the previous card's \"Copy details\" \
             footer, which would copy a stale, unrelated chain"
        );
    }

    #[test]
    fn expanded_answer_card_has_no_copy_details_footer() {
        // Neighbour: show_answer (not show_error) must never grow the
        // affordance -- there is no raw error chain behind an answer.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(
            card.inner.copy_details_rect.is_none(),
            "an answer card must not show \"Copy details\""
        );
    }

    #[test]
    fn card_lands_in_the_configured_corner_of_the_cursor_monitor() {
        // Issue #109, the wired-to-nothing check: the real window's position
        // follows `set_placement`, not just the pure math.
        use windows::Win32::UI::WindowsAndMessaging::GetWindowRect;
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let work = card.inner.target_work_area(true);
        let margin = card.inner.scale(MARGIN_DP);
        let rect_of = |card: &Card| {
            let mut rc = RECT::default();
            unsafe {
                let _ = GetWindowRect(card.hwnd(), &mut rc);
            }
            rc
        };

        card.set_placement(CardCorner::TopLeft, "cursor");
        card.show_answer("Right.", "Details.", 0, None);
        let rc = rect_of(&card);
        assert_eq!((rc.left, rc.top), (work.left + margin, work.top + margin));

        card.set_placement(CardCorner::BottomRight, "cursor");
        card.show_answer("Right.", "Details.", 0, None);
        let rc = rect_of(&card);
        assert_eq!(
            (rc.right, rc.bottom),
            (work.right - margin, work.bottom - margin)
        );
    }

    #[test]
    fn preview_from_hidden_anchors_follow_mouse_to_the_current_pointer() {
        // Issue #109: a preview shown from Hidden used to place itself with
        // whatever pointer position an earlier card had recorded.
        // #514: pin the pointer first, so a moving mouse cannot change what
        // the card should record. Read it back (the system may clamp the
        // point) and put the user's pointer back afterwards.
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let mut original = POINT::default();
        let mut pinned = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut original);
            let _ = SetCursorPos(200, 200);
            let _ = GetCursorPos(&mut pinned);
        }
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.set_placement(CardCorner::FollowMouse, "cursor");
        card.inner.follow_anchor = POINT { x: -5000, y: -5000 };
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        let anchor = (card.inner.follow_anchor.x, card.inner.follow_anchor.y);
        unsafe {
            let _ = SetCursorPos(original.x, original.y);
        }
        assert_eq!(anchor, (pinned.x, pinned.y));
    }

    #[test]
    fn expanded_answer_card_reserves_a_read_aloud_footer() {
        // Issue #77.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(
            card.inner.read_aloud_rect.is_some(),
            "answer card needs \"Read aloud\""
        );
        assert!(card.inner.copy_details_rect.is_none());
        assert_eq!(card.inner.read_aloud_label(), "Read aloud");
    }

    #[test]
    fn expanded_error_card_has_no_read_aloud_footer() {
        // Issue #77 neighbour: the one footer row belongs to "Copy details"
        // on an error card, and an error is not read aloud.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "tracked detail");
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(card.inner.read_aloud_rect.is_none());
        assert!(card.inner.copy_details_rect.is_some());
    }

    #[test]
    fn error_card_after_an_answer_card_drops_the_read_aloud_footer() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("Right.", "Details.", 0, None);
        card.show_error("Oops", "detail");
        card.inner.try_expand();
        assert!(card.inner.read_aloud_rect.is_none());
    }

    #[test]
    fn answer_clipboard_text_is_headline_blank_line_detail() {
        // Issue #531: what the user sees, nothing else.
        assert_eq!(
            answer_clipboard_text("2 + 2 = 4", "You carried."),
            "2 + 2 = 4\r\n\r\nYou carried."
        );
        assert_eq!(answer_clipboard_text("Done", ""), "Done");
    }

    #[test]
    fn footer_links_sit_side_by_side_inside_the_footer() {
        let footer = RECT {
            left: 10,
            top: 100,
            right: 300,
            bottom: 120,
        };
        let (read, copy) = footer_links_layout(footer, Some(80), 50, 16);
        let read = read.expect("read aloud rect");
        assert_eq!((read.left, read.right), (10, 90));
        assert_eq!((copy.left, copy.right), (106, 156));
        for r in [read, copy] {
            assert_eq!((r.top, r.bottom), (100, 120));
        }
        // Copy only: it takes the left edge.
        let (read, copy) = footer_links_layout(footer, None, 50, 16);
        assert!(read.is_none());
        assert_eq!((copy.left, copy.right), (10, 60));
        // A narrow footer clips the link instead of overflowing it.
        let (_, copy) = footer_links_layout(footer, Some(250), 50, 16);
        assert_eq!(copy.right, 300);
        assert!(copy.left <= copy.right);
    }

    #[test]
    fn expanded_answer_card_has_a_copy_link_beside_read_aloud() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        card.inner.try_expand();
        let read = card.inner.read_aloud_rect.expect("read aloud");
        let copy = card.inner.copy_rect.expect("copy");
        assert!(read.right <= copy.left, "links must not overlap");
        assert!(card.inner.point_hits_copy(copy.left, copy.top));
        assert!(!card.inner.point_hits_copy(read.left, read.top));
        assert!(!card.inner.point_hits_read_aloud(copy.left, copy.top));
        assert_eq!(card.inner.copy_label(), "Copy");
    }

    #[test]
    fn only_answer_cards_get_a_copy_link() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "tracked detail");
        card.inner.try_expand();
        assert!(card.inner.copy_rect.is_none());
        card.show_error("Oops", "detail");
        card.inner.try_expand();
        assert!(card.inner.copy_rect.is_none());
        // A status or confirmation card (plain show_answer) has none either.
        card.show_answer("Details copied", "Paste them.", 0, None);
        card.inner.try_expand();
        assert!(card.inner.copy_rect.is_none());
        assert!(card.inner.read_aloud_rect.is_some());
        card.show_model_answer("Right.", "Details.", 0, None);
        card.show_error("Oops", "detail");
        card.inner.try_expand();
        assert!(card.inner.copy_rect.is_none());
    }

    #[test]
    fn copied_label_uses_a_one_shot_timer_that_dies_with_the_card() {
        // Issue #531, AGENTS.md rule 5.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("Right.", "Details.", 0, None);
        card.inner.try_expand();
        assert!(!timer_was_live(&card, TIMER_COPIED), "idle: no timer");
        card.inner.mark_copied();
        assert_eq!(card.inner.copy_label(), "Copied");
        let _ = card.handle_message(WM_TIMER, WPARAM(TIMER_COPIED), LPARAM(0));
        assert_eq!(card.inner.copy_label(), "Copy");
        assert!(
            !timer_was_live(&card, TIMER_COPIED),
            "fired once, then gone"
        );
        // Hiding cancels a pending one and resets the label.
        card.inner.mark_copied();
        card.hide();
        assert!(!timer_was_live(&card, TIMER_COPIED));
        assert_eq!(card.inner.copy_label(), "Copy");
        // A replacement card never inherits "Copied".
        card.show_model_answer("A", "B", 0, None);
        card.inner.try_expand();
        card.inner.mark_copied();
        card.show_model_answer("C", "D", 0, None);
        assert_eq!(card.inner.copy_label(), "Copy");
        assert!(!timer_was_live(&card, TIMER_COPIED));
    }

    #[test]
    fn copy_key_ignores_repeat_and_shift_alt() {
        assert!(is_copy_key(0x43, 0, false, false));
        assert!(!is_copy_key(VK_RETURN.0, 0, false, false));
        assert!(!is_copy_key(0x43, 0x4000_0001, false, false), "auto-repeat");
        assert!(!is_copy_key(0x43, 0, true, false), "Shift+C");
        assert!(!is_copy_key(0x43, 0, false, true), "Alt+C");
    }

    #[test]
    fn rect_with_no_area_counts_as_unavailable() {
        let r = RECT {
            left: 5,
            top: 1,
            right: 5,
            bottom: 9,
        };
        assert!(!rect_has_area(r));
        assert!(rect_has_area(RECT { right: 6, ..r }));
    }

    #[test]
    fn copy_key_applies_only_on_an_expanded_model_answer_card() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error("Oops", "detail");
        card.inner.try_expand();
        assert!(!card.inner.copy_key_applies(0x43, 0, false, false));
        card.show_answer("Details copied", "x", 0, None);
        card.inner.try_expand();
        assert!(
            !card.inner.copy_key_applies(0x43, 0, false, false),
            "status card"
        );
        card.show_model_answer("Right.", "Details.", 0, None);
        assert!(
            !card.inner.copy_key_applies(0x43, 0, false, false),
            "collapsed"
        );
        card.inner.try_expand();
        assert!(card.inner.copy_key_applies(0x43, 0, false, false));
        assert!(!card.inner.copy_key_applies(VK_RETURN.0, 0, false, false));
    }

    #[test]
    fn only_ctrl_shift_c_copies_the_card_as_an_image() {
        assert!(is_copy_image_key(0x43, 0, true, true, false));
        assert!(!is_copy_image_key(0x43, 0, true, false, false));
        assert!(!is_copy_image_key(0x43, 0, false, true, false));
        assert!(!is_copy_image_key(0x43, 0, true, true, true));
        assert!(!is_copy_image_key(0x43, 0x4000_0000, true, true, false));
        assert!(!is_copy_image_key(0x56, 0, true, true, false));
        // The text copy keys must not become the image copy.
        assert!(!is_copy_key(0x43, 0, true, false));
    }

    #[test]
    fn gdi_pixels_become_opaque_rgba() {
        assert_eq!(
            bgra_to_opaque_rgba(&[1, 2, 3, 0, 10, 20, 30, 7]),
            vec![3, 2, 1, 255, 30, 20, 10, 255]
        );
    }

    #[test]
    fn the_card_renders_to_a_bitmap_of_its_size_with_its_text_in_it() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("Right.", "Details.", 0, None);
        card.inner.try_expand();
        card.inner.focus_link = Some(FooterLink::Copy);
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(card.inner.hwnd, &mut rc);
        }
        let (rgba, w, h) = card.inner.render_rgba().expect("a visible card renders");
        assert_eq!(
            (w as i32, h as i32),
            (rc.right - rc.left, rc.bottom - rc.top)
        );
        assert_eq!(rgba.len(), (w * h * 4) as usize);
        assert!(rgba.chunks_exact(4).all(|p| p[3] == 255));
        // The text is in it: inside the border, many pixels differ from the
        // fill (read from the padding, a few pixels in from the corner).
        let at = |x: u32, y: u32| ((y * w + x) * 4) as usize;
        let bg = rgba[at(3, 3)..at(3, 3) + 4].to_vec();
        let mut inked = 0;
        for y in 2..h - 2 {
            for x in 2..w - 2 {
                if rgba[at(x, y)..at(x, y) + 4] != bg[..] {
                    inked += 1;
                }
            }
        }
        assert!(inked > 100, "only {inked} pixels differ from the fill");
        assert_eq!(
            card.inner.focus_link,
            Some(FooterLink::Copy),
            "rendering must give the focus back"
        );
    }

    #[test]
    fn failed_copy_keeps_the_answer_and_relabels_the_link() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("Right.", "Details.", 0, None);
        card.inner.try_expand();
        card.inner.mark_copy_failed();
        assert_eq!(card.state(), CardState::Expanded);
        assert_eq!(card.inner.headline, "Right.");
        assert_eq!(card.inner.copy_label(), "Couldn't copy");
        assert!(timer_was_live(&card, TIMER_COPIED));
        card.inner.mark_copy_failed();
        let _ = card.handle_message(WM_TIMER, WPARAM(TIMER_COPIED), LPARAM(0));
        assert_eq!(card.inner.copy_label(), "Copy");
    }

    /// Issue #531 live check, run manually (it overwrites the clipboard):
    /// `cargo test copy_live -- --ignored --nocapture`. A real click on the
    /// "Copy" link must put the headline and detail on the clipboard.
    #[test]
    #[ignore = "writes the real clipboard; run by hand"]
    fn copy_live_click_puts_the_answer_on_the_clipboard() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("2 + 2 = 4", "You carried.", 0, None);
        card.inner.try_expand();
        let r = card.inner.copy_rect.expect("copy link");
        let lparam = LPARAM(((r.top + 1) << 16 | ((r.left + 1) & 0xFFFF)) as isize);
        let _ = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), lparam);
        assert_eq!(card.inner.copy_label(), "Copied");
        let text = arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .expect("clipboard text");
        assert_eq!(text.replace("\r\n", "\n"), "2 + 2 = 4\n\nYou carried.");
    }

    #[test]
    fn clicking_off_the_read_aloud_footer_does_not_start_the_voice() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("Right.", "Details.", 0, None);
        card.inner.try_expand();
        let r = card.inner.read_aloud_rect.expect("footer");
        assert!(!card.inner.point_hits_read_aloud(r.left, r.top - 1));
        let lparam = LPARAM(((r.top - 1) << 16 | (r.left & 0xFFFF)) as isize);
        let _ = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), lparam);
        assert!(!card.inner.reading);
        assert!(
            card.inner.speaker.is_none(),
            "no click on the footer, no COM voice"
        );
    }

    /// Pumps this thread's messages for `ms`, dispatching each. SAPI's
    /// notify window lives on the thread that created the voice and only
    /// forwards the end-of-speech message to the card while the thread is
    /// pumped, as the app's main loop does.
    fn pump_messages(ms: u64) {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE,
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(ms);
        while std::time::Instant::now() < deadline {
            let mut msg = MSG::default();
            while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
                unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Issue #77, needs a working system voice, so `#[ignore]`d. Run
    /// `cargo test read_aloud_live -- --ignored --nocapture`. Clicking the
    /// footer starts the voice (label flips), hiding the card stops it.
    #[test]
    #[ignore]
    fn read_aloud_live_click_speaks_and_hide_stops() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer(
            "Right.",
            "This answer is long enough that the voice is still talking when we look. \
             Keep going keep going keep going keep going keep going keep going.",
            0,
            None,
        );
        card.inner.try_expand();
        let r = card.inner.read_aloud_rect.expect("footer");
        let (x, y) = (r.left + 2, r.top + 2);
        let lparam = LPARAM(((y << 16) | (x & 0xFFFF)) as isize);
        let _ = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), lparam);
        assert!(card.inner.reading, "click must start the voice");
        assert_eq!(card.inner.read_aloud_label(), "Stop reading");
        // The first utterance can take a moment to start (voice engine load).
        let speaker = card.inner.speaker.as_ref().unwrap();
        let started = (0..60).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            speaker.is_speaking()
        });
        assert!(started, "the voice must start speaking");
        card.hide();
        assert!(!card.inner.reading);
        let speaker = card.inner.speaker.as_ref().unwrap();
        let stopped = (0..60).any(|_| {
            std::thread::sleep(std::time::Duration::from_millis(50));
            !speaker.is_speaking()
        });
        assert!(stopped, "hide must stop the voice");
    }

    /// Issue #77, needs a working system voice: a short utterance that ends
    /// by itself notifies the card (`WM_CARD_SPEECH_END`), which clears
    /// `reading`.
    #[test]
    #[ignore]
    fn read_aloud_live_natural_end_posts_notification() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("Hi.", "Ok.", 0, None);
        card.inner.try_expand();
        card.inner.read_aloud_activated();
        assert!(card.inner.reading);
        // No manual on_speech_end call: only the voice's own notification,
        // delivered to the card's wndproc, may clear `reading`.
        pump_messages(3000);
        assert!(
            !card.inner.reading,
            "label must flip back after the voice ends"
        );
    }

    #[test]
    fn clicking_copy_details_posts_wm_app_card_copy_details() {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        let rect = card
            .inner
            .copy_details_rect
            .expect("error card must have a Copy details footer once expanded");
        let x = (rect.left + rect.right) / 2;
        let y = (rect.top + rect.bottom) / 2;
        let lparam = LPARAM(crate::dismiss::pack_point(x, y) as isize);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), lparam);
        assert!(handled.is_some());
        // Unlike the settings-needed click, activating "Copy details" does
        // not hide the card itself -- the owner's handler replaces its
        // content with its own confirmation/error.
        assert_eq!(card.state(), CardState::Expanded);

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "clicking \"Copy details\" must post WM_APP_CARD_COPY_DETAILS to the owner"
        );
        assert_eq!(msg.message, WM_APP_CARD_COPY_DETAILS);
    }

    #[test]
    fn pressing_enter_on_an_expanded_error_card_also_posts_copy_details() {
        // "Keyboard reachable too, not just mouse" (#425).
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();

        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), LPARAM(0));
        assert!(handled.is_some());

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "Enter on an expanded error card must post WM_APP_CARD_COPY_DETAILS"
        );
        assert_eq!(msg.message, WM_APP_CARD_COPY_DETAILS);
    }

    /// `true` if `wanted` is queued for `hwnd`. Unlike
    /// `PeekMessageW(&mut msg, Some(hwnd), 0, 0, PM_REMOVE)` (the
    /// unfiltered range the positive-assertion tests above use, where the
    /// first message found IS the one under test), this passes `wanted` as
    /// both `wMsgFilterMin` and `wMsgFilterMax` so it only ever looks at --
    /// and only ever removes -- that exact message. A real window can have
    /// unrelated messages pending even with no interactive desktop pump
    /// running (WM_WINDOWPOSCHANGED, WM_ACTIVATE, a paint); WM_PAINT in
    /// particular is regenerated by Windows for as long as its update
    /// region stays non-empty, which an unfiltered drain loop would never
    /// clear (`BeginPaint`/`DispatchMessageW` would; a bare `PeekMessageW`
    /// does neither) -- so this must never widen to "drain everything and
    /// see what turns up".
    fn queue_contains(hwnd: HWND, wanted: u32) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};
        let mut msg = MSG::default();
        unsafe { PeekMessageW(&mut msg, Some(hwnd), wanted, wanted, PM_REMOVE) }.as_bool()
    }

    #[test]
    fn clicking_outside_the_footer_on_an_expanded_error_card_does_not_copy() {
        // Neighbour: a click elsewhere in the expanded card (e.g. on the
        // headline/detail text) must not trigger the affordance -- only the
        // footer's own rect does.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        let rect = card.inner.copy_details_rect.expect("footer must exist");
        // Just above the footer -- still inside the card, but not on it.
        let lparam = LPARAM(crate::dismiss::pack_point(rect.left, rect.top - 4) as isize);

        let handled = card.handle_message(WM_LBUTTONDOWN, WPARAM(0), lparam);
        assert!(handled.is_some());

        assert!(
            !queue_contains(card.hwnd(), WM_APP_CARD_COPY_DETAILS),
            "a click above the footer must not post WM_APP_CARD_COPY_DETAILS"
        );
    }

    #[test]
    fn pressing_enter_on_an_expanded_answer_card_does_nothing() {
        // Neighbour: no footer, so Enter must not post anything either.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        card.inner.try_expand();

        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), LPARAM(0));
        assert!(handled.is_some());

        assert!(
            !queue_contains(card.hwnd(), WM_APP_CARD_COPY_DETAILS),
            "an answer card has no Copy details to activate"
        );
    }

    // -- footer link focus (#606) --------------------------------------------

    #[test]
    fn tab_cycles_through_the_links_and_shift_tab_goes_back() {
        use FooterLink::*;
        let links = [ReadAloud, Copy];
        assert_eq!(next_footer_focus(&links, None, false), Some(ReadAloud));
        assert_eq!(
            next_footer_focus(&links, Some(ReadAloud), false),
            Some(Copy)
        );
        assert_eq!(
            next_footer_focus(&links, Some(Copy), false),
            Some(ReadAloud)
        );
        assert_eq!(next_footer_focus(&links, None, true), Some(Copy));
        assert_eq!(next_footer_focus(&links, Some(ReadAloud), true), Some(Copy));
        assert_eq!(next_footer_focus(&links, Some(Copy), true), Some(ReadAloud));
        // A focused link that is gone starts over; no links means no focus.
        assert_eq!(
            next_footer_focus(&[Copy], Some(ReadAloud), false),
            Some(Copy)
        );
        assert_eq!(next_footer_focus(&[], Some(Copy), false), None);
    }

    fn expanded_model_answer() -> Card {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_model_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        card.inner.try_expand();
        card
    }

    #[test]
    fn tab_on_an_expanded_answer_card_moves_focus_between_its_links() {
        let mut card = expanded_model_answer();
        assert_eq!(card.inner.focus_link, None);
        card.handle_message(WM_KEYDOWN, WPARAM(VK_TAB.0 as usize), LPARAM(0));
        assert_eq!(card.inner.focus_link, Some(FooterLink::ReadAloud));
        card.handle_message(WM_KEYDOWN, WPARAM(VK_TAB.0 as usize), LPARAM(0));
        assert_eq!(card.inner.focus_link, Some(FooterLink::Copy));
        card.inner.move_footer_focus(true);
        assert_eq!(card.inner.focus_link, Some(FooterLink::ReadAloud));
    }

    #[test]
    fn the_focused_link_is_drawn_with_a_focus_rectangle() {
        // The observable: the card's pixels differ once a link has focus.
        let mut card = expanded_model_answer();
        let before = render(&card);
        card.handle_message(WM_KEYDOWN, WPARAM(VK_TAB.0 as usize), LPARAM(0));
        let after = render(&card);
        assert_eq!(before.0, after.0);
        assert_ne!(before.2, after.2, "no focus rectangle was painted");
    }

    #[test]
    fn space_activates_the_focused_link() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        card.handle_message(WM_KEYDOWN, WPARAM(VK_TAB.0 as usize), LPARAM(0));
        assert_eq!(card.inner.focus_link, Some(FooterLink::CopyDetails));
        card.handle_message(WM_KEYDOWN, WPARAM(VK_SPACE.0 as usize), LPARAM(0));
        // Peek last: peeking can run a WM_KILLFOCUS another test's window
        // caused, which would close this card.
        assert!(queue_contains(card.hwnd(), WM_APP_CARD_COPY_DETAILS));
    }

    #[test]
    fn space_with_no_focused_link_does_nothing() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        assert!(!card.inner.activate_focused_link());
        card.handle_message(WM_KEYDOWN, WPARAM(VK_SPACE.0 as usize), LPARAM(0));
        assert!(!queue_contains(card.hwnd(), WM_APP_CARD_COPY_DETAILS));
    }

    #[test]
    fn a_focused_link_that_is_no_longer_shown_is_not_activated() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_error_with_details("Couldn't add the event", "detail text");
        card.inner.try_expand();
        card.inner.focus_link = Some(FooterLink::CopyDetails);
        card.inner.copy_details_rect = None;
        card.inner.copy_details_available = false;
        assert!(!card.inner.activate_focused_link());
        // The next layout drops the stale focus too.
        card.inner.layout_expanded();
        assert_eq!(card.inner.focus_link, None);
    }

    #[test]
    fn a_new_card_starts_with_no_focused_link() {
        let mut card = expanded_model_answer();
        card.inner.move_footer_focus(false);
        assert!(card.inner.focus_link.is_some());
        card.show_model_answer("next", "detail", 0, None);
        assert_eq!(card.inner.focus_link, None);
    }

    #[test]
    fn a_rejected_edit_keeps_the_title_and_adds_a_message_line_below_it() {
        // #817: the message used to replace the title.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let fields = card.inner.preview.as_ref().unwrap().model.fields().to_vec();
        let plain = card.inner.compute_preview_layout(&fields, false);
        assert!(plain.error_rect.is_none());

        card.set_preview_error_for_gallery("Start must be a time, like 09:00.");
        let with_error = card.inner.compute_preview_layout(&fields, false);
        let err = with_error.error_rect.expect("an error row");
        assert_eq!(with_error.title_rect.top, plain.title_rect.top);
        assert!(err.top >= with_error.title_rect.bottom, "under the title");
        assert!(
            with_error.rows[0].label_rect.top >= err.bottom,
            "above the fields"
        );
        assert!(with_error.window_h > plain.window_h, "the card grows");
        assert_eq!(
            card.inner.preview.as_ref().unwrap().title,
            "Add to calendar",
            "the title text is untouched"
        );
    }

    #[test]
    fn a_high_contrast_error_already_saying_error_is_not_prefixed_twice() {
        assert_eq!(
            preview_error_title("Error: bad time", Theme::HighContrast),
            "Error: bad time"
        );
    }

    #[test]
    fn a_high_contrast_preview_error_says_error_in_words() {
        assert_eq!(
            preview_error_title("Start must be a time.", Theme::HighContrast),
            "Error: Start must be a time."
        );
        assert_eq!(
            preview_error_title("Start must be a time.", Theme::Dark),
            "Start must be a time."
        );
    }

    // -- owner-drawn preview buttons (#351) ---------------------------------

    /// Sends a real `WM_DRAWITEM` for `button` to the card, drawing into an
    /// off-screen bitmap, and returns the colour at `(x, y)` inside it.
    fn drawn_pixel(card: &mut Card, button: HWND, id: i32, item_state: u32, x: i32, y: i32) -> u32 {
        use windows::Win32::Graphics::Gdi::{DeleteDC, GetPixel};
        use windows::Win32::UI::Controls::DRAWITEMSTRUCT;
        let mut rc = RECT::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(button, &mut rc);
        }
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, rc.right.max(1), rc.bottom.max(1));
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            let dis = DRAWITEMSTRUCT {
                CtlType: windows::Win32::UI::Controls::DRAWITEMSTRUCT_CTL_TYPE(4),
                CtlID: id as u32,
                itemID: 0,
                itemAction: windows::Win32::UI::Controls::ODA_FLAGS(1),
                itemState: windows::Win32::UI::Controls::ODS_FLAGS(item_state),
                hwndItem: button,
                hDC: dc,
                rcItem: rc,
                itemData: 0,
            };
            let handled = card.handle_message(
                WM_DRAWITEM,
                WPARAM(id as usize),
                LPARAM(&dis as *const _ as isize),
            );
            assert_eq!(handled, Some(LRESULT(1)));
            let c = GetPixel(dc, x, y).0;
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
            c
        }
    }

    fn preview_card_for_buttons(theme: Theme) -> Card {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.inner.theme = theme;
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, true);
        card
    }

    #[test]
    fn do_it_is_painted_in_the_system_accent_and_cancel_style_is_not() {
        use crate::ui::card_buttons::{system_accent, system_high_contrast};
        if system_high_contrast().is_some() {
            return;
        }
        for theme in [Theme::Light, Theme::Dark] {
            let mut card = preview_card_for_buttons(theme);
            let preview = card.inner.preview.as_ref().unwrap();
            let (do_it, edit) = (preview.do_it_btn, preview.edit_btn.unwrap());
            let do_px = drawn_pixel(&mut card, do_it, ID_PREVIEW_DO_IT, 0, 8, 12);
            let bg = palette_for(theme, &SysColors::read()).bg;
            assert_eq!(
                do_px,
                system_accent(theme, bg),
                "Do it fills with the accent"
            );
            let edit_px = drawn_pixel(&mut card, edit, ID_PREVIEW_EDIT, 0, 8, 12);
            assert_eq!(
                edit_px,
                palette_for(theme, &SysColors::read()).bg,
                "Edit is an outline on the card background"
            );
        }
    }

    #[test]
    fn a_pressed_button_is_painted_differently_from_a_resting_one() {
        let mut card = preview_card_for_buttons(Theme::Light);
        let do_it = card.inner.preview.as_ref().unwrap().do_it_btn;
        let rest = drawn_pixel(&mut card, do_it, ID_PREVIEW_DO_IT, 0, 8, 12);
        let pressed = drawn_pixel(&mut card, do_it, ID_PREVIEW_DO_IT, 0x1, 8, 12);
        if crate::ui::card_buttons::system_high_contrast().is_none() {
            assert_ne!(rest, pressed);
        }
    }

    #[test]
    fn a_focused_button_gets_a_focus_ring() {
        let mut card = preview_card_for_buttons(Theme::Light);
        let edit = card.inner.preview.as_ref().unwrap().edit_btn.unwrap();
        // The ring is drawn a few pixels inside the edge, on the vertical
        // midline of the left side.
        let ring_visible = (2..6).any(|x| {
            drawn_pixel(&mut card, edit, ID_PREVIEW_EDIT, 0, x, 12)
                != drawn_pixel(&mut card, edit, ID_PREVIEW_EDIT, 0x10, x, 12)
        });
        assert!(ring_visible, "focus must be visible");
    }

    #[test]
    fn preview_buttons_are_owner_drawn_and_hover_follows_the_mouse() {
        let card = preview_card_for_buttons(Theme::Light);
        let do_it = card.inner.preview.as_ref().unwrap().do_it_btn;
        assert!(is_owner_drawn_button(do_it));
        assert!(!button_is_hovered(do_it));
        let card_hwnd = card.hwnd();
        unsafe {
            let _ = preview_control_subclass(
                do_it,
                WM_MOUSEMOVE,
                WPARAM(0),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }
        assert!(button_is_hovered(do_it));
        unsafe {
            let _ = preview_control_subclass(
                do_it,
                WM_MOUSELEAVE,
                WPARAM(0),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }
        assert!(!button_is_hovered(do_it));
    }

    // -- "there is more" cues (#353) ----------------------------------------

    /// Paints the card into an off-screen bitmap and returns its size and
    /// every pixel, row by row.
    fn render(card: &Card) -> (i32, i32, Vec<u32>) {
        use windows::Win32::Graphics::Gdi::{DeleteDC, GetPixel};
        let mut rc = RECT::default();
        unsafe {
            let _ = GetClientRect(card.hwnd(), &mut rc);
        }
        let (w, h) = (rc.right, rc.bottom);
        let mut pixels = Vec::with_capacity((w * h) as usize);
        unsafe {
            let screen = GetDC(None);
            let dc = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, w.max(1), h.max(1));
            let old = SelectObject(dc, HGDIOBJ(bmp.0));
            card.inner.paint_into(dc, rc);
            for y in 0..h {
                for x in 0..w {
                    pixels.push(GetPixel(dc, x, y).0);
                }
            }
            SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(dc);
            ReleaseDC(None, screen);
        }
        (w, h, pixels)
    }

    #[test]
    fn only_a_collapsed_card_with_detail_has_more_to_show() {
        assert!(card_has_more_to_show(CardState::Collapsed, false));
        assert!(!card_has_more_to_show(CardState::Collapsed, true));
        assert!(!card_has_more_to_show(CardState::Expanded, false));
        assert!(!card_has_more_to_show(CardState::Preview, false));
    }

    #[test]
    fn the_fade_grows_to_solid_at_the_last_row() {
        assert_eq!(fade_percent(0, 4), 25);
        assert_eq!(fade_percent(3, 4), 100);
        assert_eq!(fade_percent(0, 0), 100);
        let ramp: Vec<u32> = (0..10).map(|r| fade_percent(r, 10)).collect();
        assert!(ramp.windows(2).all(|w| w[0] < w[1]), "{ramp:?}");
    }

    #[test]
    fn colors_blend_per_channel() {
        assert_eq!(blend_colors(0x0000_0000, 0x00FF_FFFF, 0), 0x0000_0000);
        assert_eq!(blend_colors(0x0000_0000, 0x00FF_FFFF, 100), 0x00FF_FFFF);
        assert_eq!(blend_colors(0x0000_0000, 0x00FE_FEFE, 50), 0x007F_7F7F);
    }

    #[test]
    fn scroll_keys_move_by_line_page_and_ends_inside_the_range() {
        let t = |vk: u16, off: i32| scroll_target_for_key(vk, off, 100, 10, 40);
        assert_eq!(t(VK_DOWN.0, 0), Some(10));
        assert_eq!(t(VK_UP.0, 5), Some(0));
        assert_eq!(t(VK_NEXT.0, 0), Some(40));
        assert_eq!(t(VK_NEXT.0, 90), Some(100));
        assert_eq!(t(VK_PRIOR.0, 30), Some(0));
        assert_eq!(t(VK_HOME.0, 70), Some(0));
        assert_eq!(t(VK_END.0, 0), Some(100));
        assert_eq!(t(VK_RETURN.0, 0), None);
    }

    #[test]
    fn a_card_with_detail_visibly_differs_from_one_without() {
        let mut with = Card::new_for_test(instance()).expect("Card::new_for_test");
        with.show_answer("Same headline", "More to read.", 0, None);
        let mut without = Card::new_for_test(instance()).expect("Card::new_for_test");
        without.show_answer("Same headline", "", 0, None);
        assert!(with.inner.chevron_reserve() > 0);
        assert_eq!(without.inner.chevron_reserve(), 0);
        let (w, h, a) = render(&with);
        let (w2, h2, b) = render(&without);
        assert_eq!((w, h), (w2, h2), "same headline, same size");
        // The chevron sits in the strip reserved at the right of the headline.
        let padding = with.inner.scale(PADDING_DP);
        let strip_left = w - padding - with.inner.chevron_reserve();
        let strip = |px: &[u32]| {
            (0..h)
                .flat_map(|y| (strip_left..w - padding).map(move |x| px[(y * w + x) as usize]))
                .collect::<Vec<u32>>()
        };
        assert_ne!(strip(&a), strip(&b), "the chevron must be painted");
    }

    #[test]
    fn a_clickable_card_asks_for_the_hand_cursor() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_answer("Headline", "Detail.", 0, None);
        let over_client = LPARAM(1); // HTCLIENT
        assert_eq!(
            card.handle_message(WM_SETCURSOR, WPARAM(0), over_client),
            Some(LRESULT(1))
        );
        card.show_answer("Headline", "", 0, None);
        assert_eq!(
            card.handle_message(WM_SETCURSOR, WPARAM(0), over_client),
            None
        );
    }

    fn long_expanded_card() -> Card {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let detail = "A line of the working.\n".repeat(80);
        card.show_answer("Headline", &detail, 0, None);
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        assert!(card.inner.scroll_max > 0, "the detail must overflow");
        card
    }

    #[test]
    fn page_down_and_end_scroll_an_overflowing_expanded_card() {
        let mut card = long_expanded_card();
        card.handle_message(WM_KEYDOWN, WPARAM(VK_NEXT.0 as usize), LPARAM(0));
        assert!(card.inner.scroll_offset > 0, "PageDown must scroll");
        card.handle_message(WM_KEYDOWN, WPARAM(VK_END.0 as usize), LPARAM(0));
        assert_eq!(card.inner.scroll_offset, card.inner.scroll_max);
        card.handle_message(WM_KEYDOWN, WPARAM(VK_HOME.0 as usize), LPARAM(0));
        assert_eq!(card.inner.scroll_offset, 0);
        assert_eq!(card.state(), CardState::Expanded, "scrolling never closes");
    }

    #[test]
    fn the_chevron_shows_only_while_there_is_more_below() {
        let mut card = long_expanded_card();
        let (w, h, above) = render(&card);
        card.handle_message(WM_KEYDOWN, WPARAM(VK_END.0 as usize), LPARAM(0));
        let (_, _, at_end) = render(&card);
        let palette = card.inner.palette();
        let padding = card.inner.scale(PADDING_DP);
        // The short text lines leave the right side of the scroll area
        // empty, so the only thing that can be there is the cue.
        let cue_count = |px: &[u32]| {
            (padding..h - padding)
                .flat_map(|y| (w / 2..w - padding).map(move |x| px[(y * w + x) as usize]))
                .filter(|&c| c == palette.detail)
                .count()
        };
        assert!(cue_count(&above) > 0, "chevron while more is below");
        assert_eq!(cue_count(&at_end), 0, "no chevron at the end");
    }

    #[test]
    fn the_bottom_edge_fades_text_only_while_there_is_more_below() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let detail = "mmmm mmmm mmmm mmmm mmmm mmmm mmmm mmmm mmmm mmmm
"
        .repeat(80);
        card.show_answer("Headline", &detail, 0, None);
        card.inner.try_expand();
        assert!(card.inner.scroll_max > 0);
        let padding = card.inner.scale(PADDING_DP);
        let gap = card.inner.scale(GAP_DP);
        let line = card.inner.line_height(card.inner.fonts.caption).max(1);
        let fade_h = line + card.inner.scale(FADE_EXTRA_DP);
        let ink = card.inner.palette().detail;
        // Pixels in the fade band, left half (the chevron is on the right),
        // that are full-strength text colour: a fade blends every one of them.
        let solid_ink = |card: &Card| {
            let (w, h, px) = render(card);
            let bottom = card
                .inner
                .footer_rect()
                .map(|r| r.top - gap)
                .unwrap_or(h - padding);
            (bottom - fade_h..bottom)
                .flat_map(|y| (padding..w / 2).map(move |x| (x, y)))
                .filter(|&(x, y)| px[(y * w + x) as usize] == ink)
                .count()
        };
        assert_eq!(solid_ink(&card), 0, "text under the fade is blended away");
        card.handle_message(WM_KEYDOWN, WPARAM(VK_END.0 as usize), LPARAM(0));
        assert!(
            solid_ink(&card) > 0,
            "without more below the same band is plain text"
        );
    }

    // -- live theme change (#431) ------------------------------------------

    fn opposite(theme: Theme) -> Theme {
        match theme {
            Theme::Light => Theme::Dark,
            Theme::Dark | Theme::HighContrast => Theme::Light,
        }
    }

    #[test]
    fn a_theme_change_message_makes_the_card_re_read_the_theme() {
        // The high contrast switch (Win+Left Alt+PrtScn) and a new contrast
        // scheme both send WM_THEMECHANGED and WM_SYSCOLORCHANGE.
        for msg in [WM_THEMECHANGED, WM_SYSCOLORCHANGE] {
            let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
            let detected = detect_theme();
            card.inner.theme = opposite(detected);
            card.inner.sys = SysColors {
                window: 1,
                window_text: 2,
                gray_text: 3,
                hotlight: 6,
                highlight: 4,
                highlight_text: 5,
            };

            let handled = card.handle_message(msg, WPARAM(0), LPARAM(0));

            assert!(handled.is_some());
            assert_eq!(card.inner.theme, detected, "message {msg:#x}");
            assert_eq!(card.inner.sys, SysColors::read(), "message {msg:#x}");
        }
    }

    #[test]
    fn a_theme_message_sent_to_the_real_window_reaches_the_card() {
        // Through the window procedure, not `handle_message`: a card whose
        // class never routed the message would pass the tests above. All three
        // messages (#658), each on a fresh card with stale values.
        let name = wide_z("ImmersiveColorSet");
        for msg in [WM_SETTINGCHANGE, WM_THEMECHANGED, WM_SYSCOLORCHANGE] {
            let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
            let detected = detect_theme();
            card.inner.theme = opposite(detected);
            card.inner.sys = SysColors {
                window: 1,
                window_text: 2,
                gray_text: 3,
                hotlight: 6,
                highlight: 4,
                highlight_text: 5,
            };
            let hwnd = card.inner.hwnd;
            let lparam = if msg == WM_SETTINGCHANGE {
                Some(LPARAM(name.as_ptr() as isize))
            } else {
                None
            };
            unsafe {
                SendMessageW(hwnd, msg, None, lparam);
            }
            assert_eq!(card.inner.theme, detected, "message {msg:#x}");
            assert_eq!(card.inner.sys, SysColors::read(), "message {msg:#x}");
        }
    }
    #[test]
    fn the_high_contrast_setting_change_makes_the_card_re_read_the_theme() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let detected = detect_theme();
        card.inner.theme = opposite(detected);
        let empty = wide_z("");

        card.handle_message(
            WM_SETTINGCHANGE,
            WPARAM(windows::Win32::UI::WindowsAndMessaging::SPI_SETHIGHCONTRAST.0 as usize),
            LPARAM(empty.as_ptr() as isize),
        );

        assert_eq!(card.inner.theme, detected);
    }

    #[test]
    fn a_forced_theme_ignores_every_theme_message() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.force_theme_for_gallery(Theme::HighContrast);
        let name = wide_z("ImmersiveColorSet");

        card.handle_message(WM_SETTINGCHANGE, WPARAM(0), LPARAM(name.as_ptr() as isize));
        card.handle_message(WM_THEMECHANGED, WPARAM(0), LPARAM(0));
        card.handle_message(WM_SYSCOLORCHANGE, WPARAM(0), LPARAM(0));

        assert_eq!(card.inner.theme, Theme::HighContrast);
        assert_eq!(card.inner.sys, theme::HC_BLACK);
    }

    #[test]
    fn the_high_contrast_palette_is_the_system_colours() {
        let sys = theme::HC_BLACK;
        let p = palette_for(Theme::HighContrast, &sys);
        assert_eq!(p.bg, sys.window);
        assert_eq!(p.headline, sys.window_text);
        assert_eq!(p.detail, sys.window_text);
        assert_eq!(
            p.hint, sys.window_text,
            "live labels are never GrayText (#816)"
        );
        assert_eq!(p.danger, sys.window_text);
        // Dark ignores the system colours.
        assert_eq!(palette_for(Theme::Dark, &sys).bg, theme::DARK.surface);
    }

    #[test]
    fn live_high_contrast_buttons_use_the_same_colours_as_the_card() {
        // The card paints with window/window text; the buttons must not read
        // button face/text, or a custom scheme splits the two.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.inner.theme = Theme::HighContrast;
        card.inner.sys = theme::HC_BLACK;
        let hc = card.inner.button_high_contrast().expect("high contrast");
        assert_eq!(
            (hc.face, hc.text),
            (card.inner.palette().bg, card.inner.palette().headline)
        );
    }

    #[test]
    fn high_contrast_badges_use_the_highlight_colours() {
        let sys = theme::HC_BLACK;
        for d in [
            Difficulty::Level(1),
            Difficulty::Level(10),
            Difficulty::Ultra,
        ] {
            assert_eq!(
                badge_colors(d, Theme::HighContrast, &sys),
                (sys.highlight, sys.highlight_text)
            );
        }
        let (fill, ink) = badge_colors(Difficulty::Level(5), Theme::Dark, &sys);
        assert_eq!(fill, difficulty_color(Difficulty::Level(5)));
        assert_eq!(ink, badge_text_color(fill));
    }

    #[test]
    fn footer_links_use_the_link_colour_not_the_hint() {
        let p = palette_for(Theme::HighContrast, &theme::HC_BLACK);
        assert_eq!(p.link, theme::HC_BLACK.hotlight);
        assert_ne!(p.link, p.hint);
        let l = palette_for(Theme::Light, &theme::HC_BLACK);
        assert_eq!(l.link, l.hint);
    }

    #[test]
    fn a_forced_high_contrast_card_paints_its_buttons_from_the_simulated_colours() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.force_theme_for_gallery(Theme::HighContrast);
        let hc = card.inner.button_high_contrast().expect("high contrast");
        assert_eq!(hc.face, theme::HC_BLACK.window);
        assert_eq!(hc.highlight, theme::HC_BLACK.highlight);
        assert_eq!(hc.text, theme::HC_BLACK.window_text);
        card.force_theme_for_gallery(Theme::Dark);
        assert!(card.inner.button_high_contrast().is_none());
    }

    #[test]
    fn an_edit_is_one_line_high_and_centred_in_its_value_cell() {
        let cell = RECT {
            left: 100,
            top: 40,
            right: 300,
            bottom: 70,
        };
        let r = centered_edit_rect(&cell, 20);
        assert_eq!((r.left, r.right), (100, 300));
        assert_eq!((r.top, r.bottom), (45, 65));
        // The same gap above and below, like a vertically centred plain value.
        assert_eq!(r.top - cell.top, cell.bottom - r.bottom);
        // A line taller than the cell never grows it, and never goes below 1.
        let tall = centered_edit_rect(&cell, 99);
        assert_eq!((tall.top, tall.bottom), (40, 70));
        assert!(centered_edit_rect(&cell, 0).bottom > 40);
    }

    #[test]
    fn a_live_preview_edit_sits_inside_its_row_and_one_line_high() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        let preview = card.inner.preview.as_ref().expect("preview");
        assert!(!preview.edits.is_empty());
        let line_h = card.inner.line_height(card.inner.fonts.body);
        for (_, hwnd) in &preview.edits {
            let mut r = RECT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(*hwnd, &mut r);
            }
            assert_eq!(r.bottom - r.top, line_h);
        }
    }

    fn card_width(card: &Card) -> i32 {
        let mut r = RECT::default();
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(card.hwnd(), &mut r);
        }
        r.right - r.left
    }

    #[test]
    fn a_forced_dpi_lays_the_card_out_at_that_dpi_and_survives_dpi_messages() {
        let mut small = Card::new_for_test(instance()).expect("Card::new_for_test");
        small.force_dpi_for_gallery(96);
        small.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        let mut big = Card::new_for_test(instance()).expect("Card::new_for_test");
        big.force_dpi_for_gallery(240);
        big.show_answer("2 + 2 = 4", "You carried correctly.", 0, None);
        assert_eq!(small.inner.dpi, 96);
        assert_eq!(big.inner.dpi, 240, "a layout must not reset the DPI");
        // 250%: the card is 2.5 times as wide (give or take rounding).
        let (s, b) = (card_width(&small), card_width(&big));
        assert!(s > 0 && (b * 100 / s) >= 240, "width {s} -> {b}");
        big.handle_message(WM_DPICHANGED, WPARAM(96), LPARAM(0));
        assert_eq!(big.inner.dpi, 240, "a DPI message must not replace it");
    }

    #[test]
    fn a_preview_error_replaces_the_title() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        card.set_preview_error_for_gallery("Start must be a time.");
        let preview = card.inner.preview.as_ref().unwrap();
        assert_eq!(preview.error.as_deref(), Some("Start must be a time."));
    }

    #[test]
    fn a_theme_setting_change_makes_the_card_re_read_the_theme() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let detected = detect_theme();
        card.inner.theme = opposite(detected);
        let name = wide_z("ImmersiveColorSet");

        let handled =
            card.handle_message(WM_SETTINGCHANGE, WPARAM(0), LPARAM(name.as_ptr() as isize));

        assert!(handled.is_some());
        assert_eq!(card.inner.theme, detected);
    }

    #[test]
    fn an_unrelated_setting_change_leaves_the_theme_alone() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let forced = opposite(detect_theme());
        card.inner.theme = forced;
        let name = wide_z("intl");

        card.handle_message(WM_SETTINGCHANGE, WPARAM(0), LPARAM(name.as_ptr() as isize));
        card.handle_message(WM_SETTINGCHANGE, WPARAM(0), LPARAM(0));

        assert_eq!(card.inner.theme, forced);
    }

    #[test]
    fn a_new_theme_drops_the_cached_edit_brush() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let detected = detect_theme();
        card.inner.theme = opposite(detected);
        card.inner.edit_bg_brush =
            unsafe { CreateSolidBrush(windows::Win32::Foundation::COLORREF(0x0012_3456)) };
        let name = wide_z("ImmersiveColorSet");

        card.handle_message(WM_SETTINGCHANGE, WPARAM(0), LPARAM(name.as_ptr() as isize));

        assert!(
            card.inner.edit_bg_brush.0.is_null(),
            "the cached brush is the old theme's color and must be rebuilt"
        );
    }

    #[test]
    fn card_palettes_keep_the_colours_they_always_had() {
        // #362 slice 1 moved the values into `theme`; the card must paint
        // exactly what it painted before. Literal numbers on purpose.
        let d = palette_for(Theme::Dark, &SysColors::read());
        assert_eq!(
            (d.bg, d.border, d.headline, d.detail, d.hint),
            (
                0x0020_2020,
                0x0040_4040,
                0x00F5_F5F5,
                0x00BE_BEBE,
                0x008C_8C8C
            )
        );
        let l = palette_for(Theme::Light, &SysColors::read());
        assert_eq!(
            (l.bg, l.border, l.headline, l.detail, l.hint),
            (
                0x00FA_FAFA,
                0x00D6_D6D6,
                0x0017_1717,
                0x005A_5A5A,
                0x0070_7070
            )
        );
        // The preview error colour was rgb(224, 82, 82) in both themes
        // until #578 moved each one to 4.5:1 on its own surface.
        assert_eq!(d.danger, 0x0064_64F0);
        assert_eq!(l.danger, 0x0032_32C8);
    }

    #[test]
    fn edit_field_colors_follow_the_card_theme() {
        // Issue #350: preview EDIT fields kept the stock white-on-black
        // regardless of theme. The colors WM_CTLCOLOREDIT hands back must
        // come from the same Palette the rest of the card paints with, not
        // a hardcoded pair -- otherwise dark mode gets a bright rectangle in
        // the one place the user reads carefully before confirming.
        let dark = palette_for(Theme::Dark, &SysColors::read());
        let (dark_text, dark_bg) = edit_field_colors(&dark, Theme::Dark, 0);
        assert_eq!(dark_bg, field_look(&dark, Theme::Dark, 0).fill);
        assert_eq!(dark_text, dark.headline);

        let light = palette_for(Theme::Light, &SysColors::read());
        let (light_text, light_bg) = edit_field_colors(&light, Theme::Light, 0);
        assert_eq!(light_bg, field_look(&light, Theme::Light, 0).fill);
        assert_eq!(light_text, light.headline);

        // The two themes must not collapse onto the same colors -- that
        // would be the "wired to nothing" failure mode: a function that
        // compiles and returns *a* color pair but ignores the theme it was
        // given.
        assert_ne!(dark_bg, light_bg);
        assert_ne!(dark_text, light_text);
    }

    /// MEASURED in the card's own headline font: a 50-character first line
    /// (`HEADLINE_CHARS_PER_LINE`, what the prompts promise) fits one
    /// collapsed headline line, with the chevron's room reserved.
    #[test]
    fn a_fifty_character_headline_fits_one_collapsed_line() {
        let card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let content_w = card.inner.scale(CARD_WIDTH_DP) - 2 * card.inner.scale(PADDING_DP);
        let avail = card.inner.headline_content_width(content_w);
        let samples = [
            "Fed holds rates at 5.25%, signals 2 or 3 cuts now.",
            "Summary of the quarterly results and what changed.",
            "WWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWWW",
            "iiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiiii",
        ];
        for text in samples {
            assert_eq!(text.chars().count(), HEADLINE_CHARS_PER_LINE, "{text}");
        }
        // MEASURED 2026-10-16: a news-style line of 50 characters fits one
        // line; a wide one needs 371 px of the 352 px available, so it
        // wraps to a second line. Wrapping is fine (the card allows
        // `HEADLINE_MAX_LINES`), clipping is not: both must stay within it.
        let fits = card
            .inner
            .measure_label(card.inner.fonts.headline, samples[0])
            .0;
        assert!(
            fits <= avail,
            "{:?} needs {fits}px of {avail}px",
            samples[0]
        );
        for text in &samples[1..3] {
            let w = card.inner.measure_label(card.inner.fonts.headline, text).0;
            assert!(w <= avail * 2, "{text:?} needs {w}px of {avail}px per line");
        }
    }

    #[test]
    fn field_boxes_stand_out_and_keep_their_text_readable_in_every_theme() {
        // #811: the box must be visible (3:1 border against its fill and
        // against the card), the text readable on the fill, and the focus
        // underline visible against the fill.
        let sys = SysColors::read();
        let mut checked = 0;
        for theme in [Theme::Light, Theme::Dark] {
            let p = palette_for(theme, &sys);
            let f = field_look(&p, theme, 0);
            let ratio =
                |a: u32, b: u32| contrast_ratio(relative_luminance(a), relative_luminance(b));
            assert!(ratio(f.border, f.fill) >= 3.0, "{theme:?} border");
            assert!(ratio(f.text, f.fill) >= 4.5, "{theme:?} text");
            assert!(ratio(f.underline, f.fill) >= 3.0, "{theme:?} underline");
            assert_ne!(f.fill, p.bg, "{theme:?}: the fill is lifted off the card");
            checked += 1;
        }
        // High contrast: the system colours, no custom fill.
        let hc = palette_for(Theme::HighContrast, &theme::HC_BLACK);
        let f = field_look(&hc, Theme::HighContrast, theme::HC_BLACK.highlight);
        assert_eq!((f.fill, f.border, f.text), (hc.bg, hc.border, hc.headline));
        assert_ne!(f.border, f.fill);
        // Focus is its own colour, the system highlight, not the border's ink.
        assert_eq!(f.underline, theme::HC_BLACK.highlight);
        assert_ne!(f.underline, f.border);
        checked += 1;
        assert_eq!(checked, 3);
    }

    #[test]
    fn a_field_edit_sits_inside_its_frame_with_room_for_the_border() {
        let frame = RECT {
            left: 100,
            top: 40,
            right: 300,
            bottom: 68,
        };
        let r = field_edit_rect(&frame, 20, false, 8, 4);
        assert_eq!((r.left, r.right), (108, 292), "8 px padding each side");
        assert!(r.top > frame.top && r.bottom < frame.bottom);
        assert_eq!(r.bottom - r.top, 20);
        // A multi-line field is inset on all four sides.
        let tall = RECT {
            left: 0,
            top: 0,
            right: 200,
            bottom: 108,
        };
        let m = field_edit_rect(&tall, 20, true, 8, 4);
        assert_eq!((m.left, m.top, m.right, m.bottom), (8, 4, 192, 104));
        // A degenerate frame never inverts the rect.
        let thin = RECT {
            left: 0,
            top: 0,
            right: 10,
            bottom: 2,
        };
        let t = field_edit_rect(&thin, 20, true, 8, 4);
        assert!(t.right > t.left && t.bottom > t.top);
    }

    #[test]
    fn a_large_text_scale_keeps_the_padding_inside_an_editable_field() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.inner.text_scale = 1.5;
        card.inner.rebuild_fonts();
        let field = Field {
            name: "title".to_string(),
            label: "Title".to_string(),
            value: "Standup".to_string(),
            editable: true,
            required: false,
            multiline: false,
        };
        let metrics = card.inner.compute_preview_layout(&[field], false);
        let frame = metrics.rows[0].value_rect;
        let line_h = card.inner.line_height(card.inner.fonts.body);
        let pad_y = card.inner.scale(FIELD_PAD_Y_DP);
        assert!(
            frame.bottom - frame.top >= line_h + 2 * pad_y,
            "{} px frame for a {line_h} px line at 150% text",
            frame.bottom - frame.top
        );
    }

    #[test]
    fn a_focus_change_repaints_only_the_field_frame() {
        let edit = RECT {
            left: 108,
            top: 44,
            right: 292,
            bottom: 64,
        };
        let r = field_frame_dirty_rect(&edit, 96);
        assert_eq!((r.left, r.right), (100, 300));
        assert_eq!((r.top, r.bottom), (30, 78));
        // Scales with DPI and stays a small region, far from the whole card.
        let r2 = field_frame_dirty_rect(&edit, 192);
        assert_eq!(r2.right - r2.left, (edit.right - edit.left) + 32);
    }

    #[test]
    fn an_editable_field_is_a_taller_box_than_its_text_line() {
        let card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let field = |editable: bool| Field {
            name: "title".to_string(),
            label: "Title".to_string(),
            value: "Standup".to_string(),
            editable,
            required: false,
            multiline: false,
        };
        let metrics = card
            .inner
            .compute_preview_layout(&[field(true), field(false)], false);
        let line_h = card.inner.line_height(card.inner.fonts.body);
        let frame = metrics.rows[0].value_rect;
        assert!(
            frame.bottom - frame.top >= line_h + 2 * card.inner.scale(FIELD_PAD_Y_DP),
            "the frame has room above and below the text line"
        );
        let edit = field_edit_rect(
            &frame,
            line_h,
            false,
            card.inner.scale(FIELD_PAD_X_DP),
            card.inner.scale(FIELD_PAD_Y_DP),
        );
        assert_eq!(edit.left - frame.left, card.inner.scale(FIELD_PAD_X_DP));
        assert!(edit.top - frame.top >= card.inner.scale(FIELD_PAD_Y_DP));
    }

    #[test]
    fn set_text_scale_clamps_and_rebuilds() {
        let mut card = Card::new(instance()).expect("Card::new");
        assert!((card.inner.text_scale - 1.0).abs() < f32::EPSILON);

        // Within range: applied as-is.
        card.set_text_scale(1.5);
        assert!((card.inner.text_scale - 1.5).abs() < f32::EPSILON);

        // Below the floor: clamped up to 0.5.
        card.set_text_scale(0.01);
        assert!((card.inner.text_scale - 0.5).abs() < f32::EPSILON);

        // Above the ceiling: clamped down to 2.0.
        card.set_text_scale(50.0);
        assert!((card.inner.text_scale - 2.0).abs() < f32::EPSILON);

        // Fonts must survive the rebuild (never left null) and painting
        // whatever state is active must still not panic afterwards.
        assert!(!card.inner.fonts.headline.0.is_null());
        assert!(!card.inner.fonts.body.0.is_null());
        assert!(!card.inner.fonts.caption.0.is_null());

        card.show_answer("headline text", "detail text", 0, Some(Difficulty::Ultra));
        card.set_text_scale(0.75);
        assert_eq!(card.state(), CardState::Collapsed);
    }

    /// The pixel height and weight GDI actually gave a font.
    fn font_metrics(font: HFONT) -> (i32, i32) {
        use windows::Win32::Graphics::Gdi::{GetObjectW, LOGFONTW};
        let mut lf = LOGFONTW::default();
        let got = unsafe {
            GetObjectW(
                HGDIOBJ(font.0),
                std::mem::size_of::<LOGFONTW>() as i32,
                Some(&mut lf as *mut _ as *mut c_void),
            )
        };
        assert!(got > 0, "GetObjectW failed");
        (lf.lfHeight, lf.lfWeight)
    }

    #[test]
    fn the_card_text_sits_on_the_type_ramp() {
        // Headline is body strong (14 px semibold), fields are body (14 px),
        // detail/links/badge are caption (12 px), all at 96 DPI.
        let f = build_fonts(96, 1.0);
        assert_eq!(font_metrics(f.headline), (-14, FW_SEMIBOLD.0 as i32));
        assert_eq!(font_metrics(f.body), (-14, FW_NORMAL.0 as i32));
        assert_eq!(font_metrics(f.caption), (-12, FW_NORMAL.0 as i32));
        f.delete();

        // The DPI is applied once: 150% is 1.5x, not 2.25x.
        let f = build_fonts(144, 1.0);
        assert_eq!(font_metrics(f.headline).0, -21);
        assert_eq!(font_metrics(f.caption).0, -18);
        f.delete();
    }

    #[test]
    fn difficulty_none_reserves_no_width() {
        let card = Card::new(instance()).expect("Card::new");
        assert_eq!(card.inner.badge_reserve(None), 0);
        let content_w = card.inner.scale(CARD_WIDTH_DP) - 2 * card.inner.scale(PADDING_DP);
        assert_eq!(card.inner.headline_content_width(content_w), content_w);

        // A real difficulty always reserves *some* width.
        assert!(card.inner.badge_reserve(Some(Difficulty::Level(1))) > 0);
        assert!(card.inner.badge_reserve(Some(Difficulty::Level(10))) > 0);
        assert!(card.inner.badge_reserve(Some(Difficulty::Ultra)) > 0);
    }

    #[test]
    fn long_headline_wraps_clear_of_the_badge() {
        let mut card = Card::new(instance()).expect("Card::new");
        let content_w = card.inner.scale(CARD_WIDTH_DP) - 2 * card.inner.scale(PADDING_DP);
        let long_headline = "supercalifragilisticexpialidocious ".repeat(6);

        // Baseline: no badge, full content width.
        card.show_answer(&long_headline, "detail", 0, None);
        let no_badge_h =
            card.inner
                .measure_wrapped(card.inner.fonts.headline, &card.inner.headline, content_w);

        // With a badge: the headline must be measured/drawn into a strictly
        // narrower width (so it wraps clear of the badge), which can only
        // ever push the wrapped height up, never down.
        card.show_answer(&long_headline, "detail", 0, Some(Difficulty::Level(10)));
        let narrowed_w = card.inner.headline_content_width(content_w);
        assert!(narrowed_w < content_w);
        let badge_h =
            card.inner
                .measure_wrapped(card.inner.fonts.headline, &card.inner.headline, narrowed_w);
        assert!(badge_h >= no_badge_h);

        // The card must still lay out and paint without panicking for every
        // difficulty value with this long headline (exercises paint_collapsed
        // and paint_expanded's badge path end-to-end).
        for difficulty in [
            Difficulty::Level(1),
            Difficulty::Level(5),
            Difficulty::Level(10),
            Difficulty::Ultra,
        ] {
            card.show_answer(&long_headline, "some detail text", 0, Some(difficulty));
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.inner.try_expand();
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.hide();
        }
    }

    fn channels(c: u32) -> (u8, u8, u8) {
        (
            (c & 0xFF) as u8,
            ((c >> 8) & 0xFF) as u8,
            ((c >> 16) & 0xFF) as u8,
        )
    }

    #[test]
    fn difficulty_color_gradient_is_sane() {
        // Rank 1: dominantly green.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(1)));
        assert!(
            g > r && g > b,
            "rank 1 should read as green (r={r} g={g} b={b})"
        );

        // Rank 10: dominantly red.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(10)));
        assert!(
            r > g && r > b,
            "rank 10 should read as red (r={r} g={g} b={b})"
        );

        // Rank 5 sits at the amber midpoint stop: not muddy brown, i.e. red
        // and green channels should both be well above blue and reasonably
        // close to each other.
        let (r, g, b) = channels(difficulty_color(Difficulty::Level(5)));
        assert!(
            r > b && g > b,
            "rank 5 should read as amber (r={r} g={g} b={b})"
        );

        // Ultra: purple, outside the gradient -- blue and red both clearly
        // above green.
        let (r, g, b) = channels(difficulty_color(Difficulty::Ultra));
        assert!(
            b > g && r > g,
            "Ultra should read as purple (r={r} g={g} b={b})"
        );

        // All 11 ranks (Level(1..=10) plus Ultra) must be visually
        // distinguishable from one another.
        let mut colors: Vec<u32> = (1..=10u8)
            .map(|n| difficulty_color(Difficulty::Level(n)))
            .collect();
        colors.push(difficulty_color(Difficulty::Ultra));
        for i in 0..colors.len() {
            for j in (i + 1)..colors.len() {
                assert_ne!(
                    colors[i], colors[j],
                    "ranks {i} and {j} produced the same colour"
                );
            }
        }
    }

    #[test]
    fn badge_text_color_contrasts_with_its_fill() {
        // Every fill colour the badge actually paints (issue #356): ranks
        // 1..=10 plus Ultra, via `difficulty_color` -- the only caller of
        // `badge_text_color` (`paint_difficulty_badge`).
        let difficulties: Vec<Difficulty> = (1..=10u8)
            .map(Difficulty::Level)
            .chain(std::iter::once(Difficulty::Ultra))
            .collect();

        for d in difficulties {
            let fill = difficulty_color(d);
            let text = badge_text_color(fill);
            assert!(
                text == BADGE_INK_DARK || text == BADGE_INK_LIGHT,
                "{d:?} fill {fill:#08x}: badge_text_color returned neither badge ink"
            );

            let fill_luminance = relative_luminance(fill);
            let dark_ratio = contrast_ratio(fill_luminance, relative_luminance(BADGE_INK_DARK));
            let light_ratio = contrast_ratio(fill_luminance, relative_luminance(BADGE_INK_LIGHT));
            let chosen_ratio = if text == BADGE_INK_DARK {
                dark_ratio
            } else {
                light_ratio
            };
            let best_ratio = dark_ratio.max(light_ratio);

            // The actual bug this rewrite fixes: `badge_text_color` must
            // always pick whichever ink gives the higher ratio. The old flat
            // BT.601-luma threshold did not: it picked white for the
            // Level(1) green stop, whose real WCAG black-ink ratio (about
            // 4.83:1) is higher than its white-ink ratio (about 3.37:1).
            assert!(
                (chosen_ratio - best_ratio).abs() < 1e-9,
                "{d:?} fill {fill:#08x}: chose a {chosen_ratio:.3}:1 ink when {best_ratio:.3}:1 was achievable"
            );

            // Level(9)'s fill (#dc4523) is the one gradient stop that
            // cannot reach WCAG AA's 4.5:1 for small text with EITHER ink
            // (best achievable is white at about 4.27:1, computed by this
            // same formula, not eyeballed) -- noted rather than silently
            // waived: every other fill must clear 4.5:1 with the ink
            // actually chosen.
            if d == Difficulty::Level(9) {
                assert!(
                    best_ratio < 4.5,
                    "Level(9) was the one fill expected to fall short of AA with both inks; if the \
                     gradient changed and it now clears 4.5:1, this note (and the `else` branch \
                     below) should cover it instead"
                );
            } else {
                assert!(
                    chosen_ratio >= 4.5,
                    "{d:?} fill {fill:#08x}: chosen ink only reaches {chosen_ratio:.3}:1, below \
                     WCAG AA's 4.5:1 for small text"
                );
            }
        }
    }

    // -- preview_key_command: pure keyboard mapping (#26) --------------------

    #[test]
    fn preview_key_command_maps_enter_to_do_it() {
        assert_eq!(
            preview_key_command(VK_RETURN.0, true),
            Some(ID_PREVIEW_DO_IT)
        );
    }

    /// #105: Enter must not send. Esc must still cancel.
    #[test]
    fn the_send_preview_ignores_enter_but_esc_still_cancels() {
        assert!(!label_confirms_with_enter(SEND_CONFIRM_LABEL));
        assert!(label_confirms_with_enter("Do it"));
        assert_eq!(preview_key_command(VK_RETURN.0, false), None);
        assert_eq!(
            preview_key_command(VK_ESCAPE.0, false),
            Some(ID_PREVIEW_CANCEL)
        );
    }

    #[test]
    fn preview_key_command_maps_escape_to_cancel() {
        assert_eq!(
            preview_key_command(VK_ESCAPE.0, true),
            Some(ID_PREVIEW_CANCEL)
        );
    }

    #[test]
    fn preview_key_command_ignores_every_other_key() {
        // Spot-check a handful of unrelated virtual-key codes, including the
        // boundary values 0 and u16::MAX.
        for vk in [0u16, 1, 0x41 /* 'A' */, 0x09 /* Tab */, u16::MAX] {
            assert_eq!(preview_key_command(vk, true), None, "vk={vk:#06x}");
        }
    }

    // -- preview_focus_left_the_card: pure decision (#207) -------------------

    #[test]
    fn focus_staying_in_the_card_group_does_not_cancel() {
        // Tab between two preview fields, or focus landing on the Do
        // it/Edit button right before its own click fires: the new focus
        // target IS the card or a descendant of it.
        assert!(!preview_focus_left_the_card(true));
    }

    #[test]
    fn focus_leaving_the_card_group_cancels() {
        // The new focus target is neither the card itself nor a descendant
        // -- covers both "another window" and WM_KILLFOCUS's own
        // wparam == 0 case (see this function's doc comment: both compute
        // new_focus_is_card_or_descendant == false at the call site).
        assert!(preview_focus_left_the_card(false));
    }

    // -- preview state: real Win32 (#26) -------------------------------------
    //
    // Uses `Card::new_for_test`, which registers and creates against a
    // window class distinct from the production `Card` (rule 9). No window
    // is ever shown interactively and no message loop is pumped, so this is
    // safe and fast to run under `cargo test`, the same "smoke test" style
    // the module's other real-Win32 tests already use.

    fn calendar_schema_and_value() -> (serde_json::Value, serde_json::Value) {
        let schema = crate::actions::schema::schema_for("calendar_event", false)
            .expect("calendar_event is registered");
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": "bring laptop"
        });
        (schema, value)
    }

    fn gwl_style(hwnd: HWND) -> isize {
        const GWL_STYLE: i32 = -16;
        unsafe {
            GetWindowLongPtrW(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WINDOW_LONG_PTR_INDEX(GWL_STYLE),
            )
        }
    }

    #[test]
    fn preview_calendar_event_renders_editable_start_and_title() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();

        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        let preview = card
            .inner
            .preview
            .as_ref()
            .expect("preview state is active");
        // #26's Done-when: "a calendar_event proposal renders with editable
        // start and title" -- exactly those two fields get a real EDIT
        // control, nothing else.
        let editable_names: Vec<&str> = preview
            .edits
            .iter()
            .map(|(name, _)| name.as_str())
            .collect();
        assert_eq!(editable_names.len(), 2);
        assert!(editable_names.contains(&"title"));
        assert!(editable_names.contains(&"start"));
        for (_, hwnd) in &preview.edits {
            assert!(!hwnd.0.is_null());
        }
        assert!(!preview.do_it_btn.0.is_null());
        // main_window_exists is false here (#352): no Edit button at all.
        assert!(preview.edit_btn.is_none());

        // A real EDIT control's initial text round-trips through the
        // window, not just the pure model.
        let start_hwnd = preview
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        assert_eq!(window_text(start_hwnd), "09:00");
    }

    /// Walks the card's own top-level BUTTON children via `GetWindow`
    /// (`GW_CHILD` then `GW_HWNDNEXT`), reading each one's text with
    /// `window_text`. Independent of `PreviewUi`'s field names, so it keeps
    /// checking the real, on-screen button set even as the struct backing
    /// it is refactored (issue #392: the whole point is that no `HWND`
    /// with `BUTTON` class and "Cancel" text is ever created, not just that
    /// no field happens to be named `cancel_btn`).
    fn preview_button_texts(card_hwnd: HWND) -> Vec<String> {
        use windows::Win32::UI::WindowsAndMessaging::{GetWindow, GW_CHILD, GW_HWNDNEXT};
        let mut out = Vec::new();
        unsafe {
            let mut child = GetWindow(card_hwnd, GW_CHILD).unwrap_or(HWND(std::ptr::null_mut()));
            while !child.0.is_null() {
                let text = window_text(child);
                if !text.is_empty() {
                    out.push(text);
                }
                child = GetWindow(child, GW_HWNDNEXT).unwrap_or(HWND(std::ptr::null_mut()));
            }
        }
        out
    }

    #[test]
    fn send_preview_confirm_button_reads_send_and_there_is_no_cancel() {
        // #105: the send preview is the normal preview with "Send" on the
        // confirm button. Observable if wired to nothing: the button would
        // still read "Do it".
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_send_preview("Send to anthropic?", &schema, &value);

        let texts = preview_button_texts(card.hwnd());
        assert!(texts.iter().any(|t| t == "Send"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Do it"), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "Cancel"), "{texts:?}");
    }

    #[test]
    fn preview_note_gets_its_own_row_above_the_buttons_and_never_becomes_a_field() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "title": { "type": "string", "editable": false } }
        });
        let value = serde_json::json!({ "title": "Standup" });
        let note = "Answered by gemma3:4b (GPT-5.5 was unavailable)";

        let _ = card.show_preview_with_note("Add event", &schema, &value, false, None);
        let fields = card.inner.preview.as_ref().unwrap().model.fields().to_vec();
        let plain = card.inner.compute_preview_layout(&fields, false);
        assert!(plain.note_rect.is_none());

        let _ = card.show_preview_with_note("Add event", &schema, &value, false, Some(note));
        let fields = card.inner.preview.as_ref().unwrap().model.fields().to_vec();
        let noted = card.inner.compute_preview_layout(&fields, false);
        let rect = noted.note_rect.expect("a note row");
        assert!(rect.top >= noted.rows[0].value_rect.bottom);
        assert!(rect.bottom <= noted.buttons.do_it.top);
        assert!(noted.window_h > plain.window_h);
        // Display only: the note is not a field, so it cannot be confirmed.
        assert_eq!(fields.len(), 1);
    }

    #[test]
    fn multiline_preview_rows_stack_the_label_and_wrap_the_value_up_to_a_cap() {
        let card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let field = |name: &str, value: &str, multiline: bool| Field {
            name: name.to_string(),
            label: name.to_string(),
            value: value.to_string(),
            editable: false,
            required: false,
            multiline,
        };
        let long = "word ".repeat(2000);
        let fields = vec![
            field("short", "x", false),
            field("short_multi", "x", true),
            field("long_multi", &long, true),
        ];
        let metrics = card.inner.compute_preview_layout(&fields, false);
        let line_h = card.inner.line_height(card.inner.fonts.body).max(1);

        let single = &metrics.rows[0];
        let one = &metrics.rows[1];
        let capped = &metrics.rows[2];

        // The label sits above the value, not beside it, and the value
        // spans from the card's left padding.
        assert_eq!(one.value_rect.left, one.label_rect.left);
        assert!(one.value_rect.top >= one.label_rect.bottom);
        assert!(one.value_rect.left < single.value_rect.left);

        // Short text takes one line; a huge dump stops at the cap, so the
        // buttons stay on screen.
        assert_eq!(one.value_rect.bottom - one.value_rect.top, line_h);
        assert_eq!(
            capped.value_rect.bottom - capped.value_rect.top,
            line_h * PREVIEW_MULTILINE_MAX_LINES
        );
        assert!(metrics.buttons.do_it.top >= capped.value_rect.bottom);
    }

    #[test]
    fn preview_button_set_has_no_cancel_button() {
        // #392: nothing has run yet when a preview is shown, so there is
        // nothing to undo -- the Cancel button must not exist at all, only
        // "Do it". Esc and click-away still resolve to `preview_cancel`
        // (covered by `preview_key_command_maps_escape_to_cancel` and the
        // `preview_control_subclass_*` click-away tests below), just with
        // no visible button behind them.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let texts = preview_button_texts(card.hwnd());
        assert!(
            texts.iter().any(|t| t == "Do it"),
            "expected a \"Do it\" button among {texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t == "Cancel"),
            "no Cancel button on a preview where nothing has run yet, found {texts:?}"
        );
    }

    /// The wired-to-nothing check for issue #350 that does not depend on a
    /// desktop or compositor being attached to the process: sends the exact
    /// message a real preview EDIT control sends its parent
    /// (WM_CTLCOLOREDIT, wParam = the field's own HDC, lParam = its HWND)
    /// straight to `CardInner::handle_message`, the same path the real
    /// window proc uses, and checks the two GDI side effects a stock,
    /// unhandled WM_CTLCOLOREDIT would never produce: the HDC's text/
    /// background colors actually changed to the theme's, and a non-null
    /// brush came back (the stock default would leave `DefWindowProcW` to
    /// return `COLOR_WINDOW`, not our cached brush).
    #[test]
    fn wm_ctlcoloredit_paints_the_dark_palette_not_stock_white() {
        use windows::Win32::Graphics::Gdi::{GetBkColor, GetTextColor};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.inner.theme = Theme::Dark;
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let edit_hwnd = card
            .inner
            .preview
            .as_ref()
            .expect("preview state is active")
            .edits[0]
            .1;

        let hdc = unsafe { GetDC(Some(edit_hwnd)) };
        let result = card.inner.handle_message(
            WM_CTLCOLOREDIT,
            WPARAM(hdc.0 as usize),
            LPARAM(edit_hwnd.0 as isize),
        );
        let brush = result.expect("WM_CTLCOLOREDIT must be handled, not fall through to Def*");
        assert_ne!(brush.0, 0, "must return a real brush, not NULL");

        let dark = palette_for(Theme::Dark, &SysColors::read());
        let text_after = unsafe { GetTextColor(hdc) };
        let bg_after = unsafe { GetBkColor(hdc) };
        assert_eq!(text_after.0, dark.headline);
        // #811: the field fill is the card colour lifted a little, not the
        // card colour itself.
        assert_eq!(bg_after.0, field_look(&dark, Theme::Dark, 0).fill);
        assert_ne!(bg_after.0, dark.bg);
        // The bug this closes: stock EDIT white background, unconditionally.
        assert_ne!(bg_after.0, rgb(255, 255, 255));

        unsafe {
            ReleaseDC(Some(edit_hwnd), hdc);
        }
    }

    /// Manual observable for issue #350 (wired-to-nothing check): shows a
    /// real preview card, forced into `Theme::Dark`, pumps a few WM_PAINTs
    /// so the EDIT fields actually receive WM_CTLCOLOREDIT and repaint, then
    /// captures the window's own pixels with GetDIBits and writes a PNG.
    /// Captured in-process (not via a second process's screen-scrape)
    /// because this box's Bash and PowerShell tools run on window stations
    /// that cannot see each other's windows -- MEASURED 2026-09-24:
    /// `FindWindowW(NULL, "Wingman")` from a PowerShell tool call found
    /// nothing while the card window from a backgrounded `cargo test` was
    /// on screen and the test itself was still running. Not run by
    /// `cargo test`: `#[ignore]`d, same pattern as
    /// `ocr::ocr_live_recognizes_gdi_rendered_text`. Run with
    /// `cargo test dark_preview_manual_screenshot -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn dark_preview_manual_screenshot() {
        use image::ImageEncoder;
        use windows::Win32::Graphics::Gdi::{
            GetDIBits, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        };
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, GetWindowRect, PeekMessageW, TranslateMessage, PM_REMOVE,
        };

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        // Force dark regardless of the machine's actual theme, so this
        // check does not depend on the dev box's Settings.
        card.inner.theme = Theme::Dark;
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let hwnd = card.hwnd();

        unsafe {
            use windows::Win32::Graphics::Gdi::{
                RedrawWindow, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
            };

            // Pump enough messages for WM_PAINT (and the WM_CTLCOLOREDIT
            // each preview EDIT sends as it repaints) to actually run, then
            // force an immediate synchronous repaint before capture.
            for _ in 0..15 {
                let mut msg = std::mem::zeroed();
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
                std::thread::sleep(std::time::Duration::from_millis(30));
            }
            let _ = RedrawWindow(
                Some(hwnd),
                None,
                None,
                RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
            );

            let mut rect = RECT::default();
            GetWindowRect(hwnd, &mut rect).expect("GetWindowRect");
            let w = rect.right - rect.left;
            let h = rect.bottom - rect.top;

            let hdc = GetDC(Some(hwnd));
            let mem_dc = CreateCompatibleDC(Some(hdc));
            let bmp = CreateCompatibleBitmap(hdc, w, h);
            let old = SelectObject(mem_dc, HGDIOBJ(bmp.0));
            let _ = BitBlt(mem_dc, 0, 0, w, h, Some(hdc), 0, 0, SRCCOPY);
            SelectObject(mem_dc, old);

            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
            bmi.bmiHeader.biWidth = w;
            bmi.bmiHeader.biHeight = -h; // top-down DIB
            bmi.bmiHeader.biPlanes = 1;
            bmi.bmiHeader.biBitCount = 32;
            bmi.bmiHeader.biCompression = BI_RGB.0;

            let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
            GetDIBits(
                mem_dc,
                bmp,
                0,
                h as u32,
                Some(buf.as_mut_ptr() as *mut _),
                &mut bmi,
                DIB_RGB_COLORS,
            );

            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem_dc);
            ReleaseDC(Some(hwnd), hdc);

            // GetDIBits hands back BGRA; the PNG encoder wants RGBA.
            for px in buf.chunks_exact_mut(4) {
                px.swap(0, 2);
            }

            let path = std::env::temp_dir().join("wingman_dark_preview_350.png");
            let file = std::fs::File::create(&path).expect("create png file");
            let mut writer = std::io::BufWriter::new(file);
            image::codecs::png::PngEncoder::new(&mut writer)
                .write_image(&buf, w as u32, h as u32, image::ExtendedColorType::Rgba8)
                .expect("encode png");
            println!("saved dark preview screenshot to {}", path.display());
        }
    }

    #[test]
    fn preview_edit_button_only_exists_once_main_window_exists() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();

        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        assert!(
            card.inner.preview.as_ref().unwrap().edit_btn.is_none(),
            "Edit must not be created while no main window exists (#352)"
        );

        card.inner
            .show_preview("Add to calendar", &schema, &value, true);
        let edit_btn = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edit_btn
            .expect("Edit must be created once main_window_exists is true");
        assert_eq!(
            gwl_style(edit_btn) & (WS_DISABLED.0 as isize),
            0,
            "Edit must be enabled, not WS_DISABLED, once created"
        );
    }

    /// #355's pure layout-math test: `label_column_width` at several DPIs.
    /// Callers scale their inputs first, so this exercises the same
    /// function real DPIs would see without needing a real window.
    #[test]
    fn label_column_width_is_clamped_at_several_dpis() {
        for dpi in [96u32, 120, 144, 192] {
            let scale = |dp: i32| (dp * dpi as i32 + 48) / 96;
            let min_w = scale(PREVIEW_LABEL_W_DP);
            let content_w = scale(PREVIEW_WIDTH_DP) - 2 * scale(PADDING_DP);
            let max_w = ((content_w as f32) * PREVIEW_LABEL_MAX_FRACTION) as i32;

            // A short label never grows the column past its natural width.
            let short = scale(20);
            assert_eq!(
                label_column_width(&[short], min_w, max_w),
                min_w,
                "dpi {dpi}: a label narrower than the floor must not shrink the column"
            );

            // A label between the floor and the ceiling sizes the column
            // to exactly that label.
            let mid = (min_w + max_w) / 2;
            assert_eq!(
                label_column_width(&[mid], min_w, max_w),
                mid,
                "dpi {dpi}: a label between floor and ceiling sizes the column to it"
            );

            // A label wider than the ceiling is clamped, not honored in
            // full (it wraps instead -- see the row-height test below).
            let huge = max_w + scale(200);
            assert_eq!(
                label_column_width(&[huge], min_w, max_w),
                max_w,
                "dpi {dpi}: a label past the 45% ceiling must clamp, not widen the column further"
            );

            // The widest of several labels wins, still clamped.
            assert_eq!(
                label_column_width(&[short, mid, huge], min_w, max_w),
                max_w,
                "dpi {dpi}: the widest label among several drives the column"
            );

            // No fields at all: falls back to the floor, never zero/negative.
            assert_eq!(label_column_width(&[], min_w, max_w), min_w);
        }
    }

    /// #355's Done-when: a "Date of birth" field's whole label is visible
    /// (measured width fits inside `label_rect`, or it wraps rather than
    /// being cut) at both 100% and 150% scaling.
    #[test]
    fn preview_long_label_is_never_ellipsized_at_100_or_150_percent() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        for dpi in [96u32, 144] {
            card.inner.dpi = dpi;
            card.inner.rebuild_fonts();

            let fields = vec![Field {
                name: "dob".to_string(),
                label: "Date of birth".to_string(),
                value: "2000-01-01".to_string(),
                editable: false,
                required: false,
                multiline: false,
            }];
            let metrics = card.inner.compute_preview_layout(&fields, false);
            let row = &metrics.rows[0];
            let (label_w, _) = card
                .inner
                .measure_label(card.inner.fonts.body, "Date of birth");
            let column_w = row.label_rect.right - row.label_rect.left;

            assert!(
                column_w >= label_w || row.label_wrapped,
                "dpi {dpi}: \"Date of birth\" must fit the column ({column_w}px) or wrap, \
                 not be silently ellipsized (measured {label_w}px)"
            );
        }
    }

    #[test]
    fn row_is_stacked_for_multiline_long_label_or_long_read_only_value() {
        assert!(row_is_stacked(true, 10, 100, None, 100));
        assert!(row_is_stacked(false, 101, 100, None, 100));
        assert!(row_is_stacked(false, 10, 100, Some(101), 100));
        assert!(!row_is_stacked(false, 100, 100, Some(100), 100));
        // An editable field (no measured value) scrolls inside its box.
        assert!(!row_is_stacked(false, 10, 100, None, 100));
    }

    #[test]
    fn preview_long_read_only_value_is_shown_whole_below_its_label() {
        // #813: "742 Evergreen Terrace, Apt 4B (from profile)" must not be
        // cut to "742 Evergreen Terrace..." beside its label.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        for dpi in [96u32, 144] {
            card.inner.dpi = dpi;
            card.inner.rebuild_fonts();
            let value = "742 Evergreen Terrace, Apt 4B (from profile)";
            let fields = vec![Field {
                name: "addr".to_string(),
                label: "Mailing address, including apartment or unit number".to_string(),
                value: value.to_string(),
                editable: false,
                required: false,
                multiline: false,
            }];
            let m = card.inner.compute_preview_layout(&fields, false);
            let row = &m.rows[0];
            assert!(
                row.value_rect.top >= row.label_rect.bottom,
                "dpi {dpi}: value must sit below its label"
            );
            assert!(row.value_stacked, "dpi {dpi}");
            let format = value_draw_format(row.value_stacked);
            assert!(
                format.0 & DT_WORDBREAK.0 != 0 && format.0 & DT_SINGLELINE.0 == 0,
                "dpi {dpi}: the value must be drawn wrapped, not on one cut line"
            );
            assert!(format.0 & DT_VCENTER.0 == 0, "dpi {dpi}: top aligned");
            // The box holds the wrapped text.
            let box_w = row.value_rect.right - row.value_rect.left;
            let wrapped = card
                .inner
                .measure_wrapped(card.inner.fonts.body, value, box_w);
            assert!(row.value_rect.bottom - row.value_rect.top >= wrapped);
        }
    }

    #[test]
    fn preview_rows_keep_an_even_gap() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let mk = |name: &str, label: &str, value: &str, editable: bool| Field {
            name: name.to_string(),
            label: label.to_string(),
            value: value.to_string(),
            editable,
            required: false,
            multiline: false,
        };
        for dpi in [96u32, 144] {
            card.inner.dpi = dpi;
            card.inner.rebuild_fonts();
            let fields = vec![
                mk(
                    "a",
                    "Employer-provided legal first and middle name",
                    "Jordan Alexander (from profile)",
                    false,
                ),
                mk(
                    "b",
                    "Mailing address, including apartment or unit number",
                    "742 Evergreen Terrace, Apt 4B (from profile)",
                    false,
                ),
                mk(
                    "c",
                    "Social security number (last four digits only) (sensitive, clear to skip)",
                    "6789",
                    true,
                ),
            ];
            let m = card.inner.compute_preview_layout(&fields, false);
            let gap = card.inner.scale(PREVIEW_ROW_GAP_DP);
            for pair in m.rows.windows(2) {
                let prev_bottom = pair[0].label_rect.bottom.max(pair[0].value_rect.bottom);
                assert_eq!(pair[1].label_rect.top - prev_bottom, gap, "dpi {dpi}");
            }
        }
    }
    #[test]
    fn preview_without_edit_keeps_do_it_right_aligned_with_no_cancel_slot() {
        // #392 supersedes #352's Cancel-placement Done-when: Cancel no
        // longer exists at all, so there is no layout slot for it any more
        // -- only "Do it" (always right-aligned against the content edge)
        // and, when `show_edit` is true, "Edit" at the content's left edge.
        let card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let fields = vec![];
        let content_right = card.inner.scale(PREVIEW_WIDTH_DP) - card.inner.scale(PADDING_DP);

        let metrics = card.inner.compute_preview_layout(&fields, false);
        assert!(metrics.buttons.edit.is_none());
        assert_eq!(metrics.buttons.do_it.right, content_right);

        let with_edit = card.inner.compute_preview_layout(&fields, true);
        assert!(with_edit.buttons.edit.is_some());
        assert_eq!(with_edit.buttons.do_it.right, content_right);
        let edit_rect = with_edit.buttons.edit.unwrap();
        assert_eq!(edit_rect.left, card.inner.scale(PADDING_DP));
        assert!(edit_rect.right < with_edit.buttons.do_it.left);
    }

    #[test]
    fn preview_do_it_after_an_edit_builds_a_confirmed_equal_to_what_was_shown() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();

        // Programmatically edit the field, exactly like a real keystroke
        // would leave it.
        let edited = wide_z("10:30");
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                start_hwnd,
                PCWSTR(edited.as_ptr()),
            );
        }

        // Simulate the "Do it" command via SendMessage(WM_COMMAND), the same
        // message a real button click (or the Enter subclass) generates.
        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(
            card.state(),
            CardState::Hidden,
            "Do it must close the preview"
        );
        let confirmed = card
            .take_confirmed()
            .expect("Do it must produce a Confirmed");
        assert_eq!(confirmed.value()["start"], "10:30");
        assert_eq!(confirmed.value()["title"], "Standup");
        assert_eq!(confirmed.value()["end"], "09:15");
        assert_eq!(confirmed.value()["location"], "Room 2");

        // A take, not a peek.
        assert!(card.take_confirmed().is_none());
    }

    #[test]
    fn preview_cancel_produces_no_confirmed_and_closes() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_CANCEL as usize)),
                Some(LPARAM(0)),
            );
        }

        assert_eq!(card.state(), CardState::Hidden);
        assert!(
            card.take_confirmed().is_none(),
            "Cancel must produce nothing"
        );
    }

    #[test]
    fn preview_do_it_notifies_the_owner_window_via_wm_app_preview_decided() {
        // Issue #39: uses the card's own hwnd as a stand-in "owner" (a
        // self-notify) so the real observable -- a posted message sitting
        // in a real message queue -- can be checked with PeekMessageW, the
        // same idiom `preview_control_subclass_forwards_enter_as_a_posted_do_it_command`
        // already uses for the same reason.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_do_it must post WM_APP_PREVIEW_DECIDED to the owner window"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn preview_cancel_also_notifies_the_owner_window() {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_CANCEL as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_cancel must also post WM_APP_PREVIEW_DECIDED (App::on_preview_decided \
             calls take_confirmed(), which is None for Cancel -- see that method's doc comment)"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn hide_while_preview_is_open_notifies_the_owner_with_no_confirmation() {
        // Issue #225: every one of app.rs's "hide the stale card" call
        // sites (ask, extract_text, copy_region, open_settings,
        // pause_for, add_event_from_screen, review_this_email,
        // fill_form_from_screen) calls exactly this public `Card::hide()`
        // with no check for `CardState::Preview`. Before the fix,
        // `CardInner::hide` routed to `leave_preview_if_active`, which
        // destroyed the preview's controls and reset `state` to `Hidden`
        // -- but never posted `WM_APP_PREVIEW_DECIDED`, so whichever
        // pending-preview context `App` had stashed for it was never
        // cleared, and a later, unrelated preview's confirm could pick up
        // the stale one instead (or run neither, dropping the new one).
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        // The bug: hiding the card for an unrelated reason while a
        // preview is open (Settings opening, Pause, a second action
        // starting...), instead of the user pressing "Do it" or "Cancel".
        card.hide();

        assert_eq!(card.state(), CardState::Hidden, "hide() must still hide");
        assert!(
            card.take_confirmed().is_none(),
            "an externally-hidden preview must never look like a Do it"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "hide() must post WM_APP_PREVIEW_DECIDED when it destroys an \
             active preview, so App::on_preview_decided can clear its \
             pending-preview slot instead of leaving it to hijack a \
             later, unrelated preview's decision (#225)"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn show_preview_interrupting_an_active_preview_notifies_before_replacing_it() {
        // Same shape as the hide() case above, but for the path where a
        // NEW preview interrupts an active one directly (on_calendar_result
        // / on_review_result / on_form_fill_result all call show_preview
        // without checking for an existing one first).
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);

        let _ = card.show_preview("Fill this form", &schema, &value, false);
        assert_eq!(
            card.state(),
            CardState::Preview,
            "the second preview must still show"
        );

        let card_hwnd = card.hwnd();
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "replacing an active preview with a new one must notify the \
             owner about the FIRST one before showing the second"
        );
        assert_eq!(msg.message, WM_APP_PREVIEW_DECIDED);
    }

    #[test]
    fn show_preview_stops_a_card_that_is_being_read_aloud() {
        // Issue #77: a preview replacing an expanded answer must stop the
        // voice and clear `reading`, like hide() and the other show_* paths.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        card.show_answer("Right.", "Details.", 0, None);
        card.inner.try_expand();
        card.inner.reading = true;
        let (schema, value) = calendar_schema_and_value();
        let _ = card.show_preview("Add to calendar", &schema, &value, false);
        assert!(
            !card.inner.reading,
            "a preview must stop the read-aloud state"
        );
    }

    #[test]
    fn preview_do_it_posts_exactly_one_wm_app_preview_decided() {
        // Mutation guard for #225's fix: `leave_preview_if_active` now
        // does the notifying (so an external `hide()` is covered too),
        // and `preview_do_it`'s own explicit call was removed -- if it
        // had not been, "Do it" would double-post and this would go red.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.set_owner(card.hwnd());
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let card_hwnd = card.hwnd();
        unsafe {
            SendMessageW(
                card_hwnd,
                WM_COMMAND,
                Some(WPARAM(ID_PREVIEW_DO_IT as usize)),
                Some(LPARAM(0)),
            );
        }

        let mut count = 0;
        let mut msg = MSG::default();
        while unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() } {
            if msg.message == WM_APP_PREVIEW_DECIDED {
                count += 1;
            }
        }
        assert_eq!(
            count, 1,
            "leave_preview_if_active must not double-post alongside a \
             separate explicit notify"
        );
    }

    #[test]
    fn no_owner_set_means_preview_do_it_never_panics_and_posts_nothing() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        // No `set_owner` call: `preview_do_it` must degrade quietly, same
        // as every other best-effort Win32 call in this module.
        card.inner.preview_do_it();
        assert!(card.take_confirmed().is_some());
    }

    #[test]
    fn preview_do_it_with_an_unparseable_typed_edit_stays_open_and_confirms_nothing() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "count": { "type": "integer", "editable": true },
                "note": { "type": "string", "editable": true }
            }
        });
        let value = serde_json::json!({ "count": 3, "note": "keep me" });
        card.inner.show_preview("Typed", &schema, &value, false);
        let set_text = |hwnd: HWND, s: &str| {
            let text: Vec<u16> = format!("{s}\0").encode_utf16().collect();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowTextW(
                    hwnd,
                    windows::core::PCWSTR(text.as_ptr()),
                );
            }
        };
        let (count_edit, note_edit) = {
            let p = card.inner.preview.as_ref().expect("preview");
            (p.edits[0].1, p.edits[1].1)
        };
        set_text(count_edit, "abc");
        set_text(note_edit, "edited note");
        card.inner.preview_do_it();
        assert!(card.take_confirmed().is_none(), "nothing may be confirmed");
        assert_eq!(card.state(), CardState::Preview, "the preview stays open");
        let p = card.inner.preview.as_ref().expect("preview still there");
        assert_eq!(
            p.error.as_deref(),
            Some("Count must be a whole number"),
            "the problem is shown inline"
        );
        assert_eq!(
            p.model.to_value()["note"],
            "edited note",
            "other edits survive"
        );

        set_text(count_edit, "4");
        card.inner.preview_do_it();
        let confirmed = card.take_confirmed().expect("confirmed after the fix");
        assert_eq!(confirmed.value()["count"], serde_json::json!(4));
        assert_eq!(confirmed.value()["note"], "edited note");
    }

    #[test]
    fn preview_do_it_keeps_a_typed_field_typed() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let schema = serde_json::json!({
            "type": "object",
            "properties": { "count": { "type": "integer", "editable": true } }
        });
        card.inner
            .show_preview("Typed", &schema, &serde_json::json!({ "count": 3 }), false);
        card.inner.preview_do_it();
        let confirmed = card.take_confirmed().expect("confirmed");
        assert_eq!(confirmed.value()["count"], serde_json::json!(3));
    }

    #[test]
    fn preview_never_auto_dismisses_on_the_safety_timer() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        // Simulate a TIMER_DISMISS message arriving while Preview is active
        // (rule 5: a card must never close on a timer while showing an
        // action that has not been confirmed yet).
        let handled = card.handle_message(WM_TIMER, WPARAM(TIMER_DISMISS), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(
            card.state(),
            CardState::Preview,
            "TIMER_DISMISS must never close an open preview"
        );
    }

    #[test]
    fn preview_enter_key_on_the_card_window_itself_triggers_do_it() {
        // Exercises the card's own WM_KEYDOWN arm (as opposed to a child
        // control's subclass), which uses the same preview_key_command
        // mapping.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);

        let handled = card.handle_message(WM_KEYDOWN, WPARAM(VK_RETURN.0 as usize), LPARAM(0));
        assert!(handled.is_some());
        assert_eq!(card.state(), CardState::Hidden);
        assert!(card.take_confirmed().is_some());
    }

    #[test]
    fn preview_control_subclass_forwards_enter_as_a_posted_do_it_command() {
        // Real Enter/Esc keystrokes reach `run_preview_command` through
        // `preview_control_subclass` (installed on every EDIT/BUTTON child
        // by `create_preview_controls`), not through the card's own
        // WM_KEYDOWN arm -- see that function's doc comment for why a child
        // control's WM_KEYDOWN never reaches the parent on its own. This
        // test calls the actual installed function pointer directly, the
        // same way Windows would while a preview EDIT control has focus,
        // and checks the real observable: a WM_COMMAND sitting in this
        // thread's message queue (PostMessageW, not SendMessageW), which is
        // exactly what `run_preview_command` needs delivered to fire.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KEYDOWN,
                WPARAM(VK_RETURN.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        // A single, non-looping check: posted messages (like the WM_COMMAND
        // above) take priority over synthesized WM_PAINT/WM_TIMER messages,
        // so one PeekMessageW call retrieves it directly. This deliberately
        // does NOT loop with PM_REMOVE: discarding a synthesized WM_PAINT
        // without running it through DispatchMessageW (which is what
        // validates the window via BeginPaint/EndPaint) would make Windows
        // re-synthesize WM_PAINT forever for this still-invalid window --
        // an infinite busy loop, not a hang that shows up as "no output".
        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "preview_control_subclass must post a WM_CARD_PREVIEW_COMMAND to the card window"
        );
        assert_eq!(msg.message, WM_CARD_PREVIEW_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_DO_IT);
    }

    // -- stale preview command must not hijack a later preview (#255) -----

    #[test]
    fn a_click_away_cancel_posted_for_one_preview_does_not_close_the_next() {
        // #255: the click-away cancel is posted for preview A but only
        // delivered after preview B replaced it. Uses the real subclass to
        // post it, so the tag the receiver checks is the one it really sets.
        // The posted message is taken off the queue straight away and
        // delivered by hand: draining the queue later would also pick up
        // focus changes the OS makes under a parallel test run, which
        // legitimately cancel the live preview.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner.show_preview("Preview A", &schema, &value, false);
        let start_hwnd = card.inner.preview.as_ref().unwrap().edits[1].1;
        let card_hwnd = card.hwnd();
        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(0),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }
        let mut posted = MSG::default();
        let found = unsafe {
            PeekMessageW(
                &mut posted,
                Some(card_hwnd),
                WM_CARD_PREVIEW_COMMAND,
                WM_CARD_PREVIEW_COMMAND,
                PM_REMOVE,
            )
            .as_bool()
        };
        assert!(found, "the click-away must post a command");
        card.inner.show_preview("Preview B", &schema, &value, false);

        card.handle_message(posted.message, posted.wParam, posted.lParam);

        assert_eq!(card.state(), CardState::Preview);
        assert_eq!(
            card.inner.preview.as_ref().map(|p| p.title.as_str()),
            Some("Preview B")
        );
    }

    #[test]
    fn stale_preview_command_is_ignored_and_current_one_runs() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner.show_preview("Preview", &schema, &value, false);
        let live = card.inner.preview_generation;

        card.handle_message(
            WM_CARD_PREVIEW_COMMAND,
            WPARAM(ID_PREVIEW_CANCEL as usize),
            LPARAM(live.wrapping_sub(1) as isize),
        );
        assert_eq!(
            card.state(),
            CardState::Preview,
            "stale cancel must be dropped"
        );

        card.handle_message(
            WM_CARD_PREVIEW_COMMAND,
            WPARAM(ID_PREVIEW_CANCEL as usize),
            LPARAM(live as isize),
        );
        assert_eq!(
            card.state(),
            CardState::Hidden,
            "current cancel must still close"
        );
    }

    #[test]
    fn preview_control_subclass_tags_posted_commands_with_the_live_generation() {
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner.show_preview("Preview", &schema, &value, false);
        let live = card.inner.preview_generation;
        let start_hwnd = card.inner.preview.as_ref().unwrap().edits[0].1;
        let card_hwnd = card.hwnd();

        for (key, command) in [
            (VK_ESCAPE, ID_PREVIEW_CANCEL),
            (VK_RETURN, ID_PREVIEW_DO_IT),
        ] {
            unsafe {
                let _ = preview_control_subclass(
                    start_hwnd,
                    WM_KEYDOWN,
                    WPARAM(key.0 as usize),
                    LPARAM(0),
                    PREVIEW_SUBCLASS_ID,
                    card_hwnd.0 as usize,
                );
            }

            let mut msg = MSG::default();
            let found = unsafe {
                PeekMessageW(
                    &mut msg,
                    Some(card_hwnd),
                    WM_CARD_PREVIEW_COMMAND,
                    WM_CARD_PREVIEW_COMMAND,
                    PM_REMOVE,
                )
                .as_bool()
            };
            assert!(found, "Esc and Enter must post a WM_CARD_PREVIEW_COMMAND");
            assert_eq!((msg.wParam.0 & 0xFFFF) as i32, command);
            assert_eq!(msg.lParam.0 as u32, live);
        }
    }

    // -- click-away cancels the preview (#207) --------------------------

    #[test]
    fn preview_control_subclass_posts_cancel_when_focus_leaves_the_card() {
        // Simulates a real click-away: WM_KILLFOCUS on a preview field whose
        // new focus target (wparam, per the real WM_KILLFOCUS contract) is
        // some other, unrelated window -- not the card, not one of its
        // children.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        let other_hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(wide_z(TEST_CLASS_NAME).as_ptr()),
                PCWSTR(wide_z("unrelated window").as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance()),
                None,
            )
        }
        .expect("CreateWindowExW for the unrelated window");

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(other_hwnd.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(
            found,
            "a click-away WM_KILLFOCUS must post a WM_CARD_PREVIEW_COMMAND to the card window"
        );
        assert_eq!(msg.message, WM_CARD_PREVIEW_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_CANCEL);

        unsafe {
            let _ = DestroyWindow(other_hwnd);
        }
    }

    #[test]
    fn preview_control_subclass_posts_cancel_when_nothing_gains_focus() {
        // WM_KILLFOCUS's wparam is 0 when no window is gaining the focus at
        // all (e.g. the whole app losing the foreground) -- must cancel the
        // same as a click into another window, per preview_focus_left_the_card's
        // doc comment.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let start_hwnd = card
            .inner
            .preview
            .as_ref()
            .unwrap()
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(0),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        let mut msg = MSG::default();
        let found = unsafe { PeekMessageW(&mut msg, Some(card_hwnd), 0, 0, PM_REMOVE).as_bool() };
        assert!(found, "wparam == 0 must also be treated as a click-away");
        assert_eq!(msg.message, WM_CARD_PREVIEW_COMMAND);
        assert_eq!((msg.wParam.0 & 0xFFFF) as i32, ID_PREVIEW_CANCEL);
    }

    #[test]
    fn preview_control_subclass_does_not_cancel_on_focus_moving_to_another_preview_control() {
        // Tab moving focus from the "start" EDIT to the Do it button (both
        // preview children) must not cancel -- the exact scenario
        // preview_focus_left_the_card's doc comment names.
        use windows::Win32::UI::WindowsAndMessaging::{PeekMessageW, MSG, PM_REMOVE};

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        let preview = card.inner.preview.as_ref().unwrap();
        let start_hwnd = preview
            .edits
            .iter()
            .find(|(name, _)| name == "start")
            .map(|(_, hwnd)| *hwnd)
            .unwrap();
        let do_it_btn = preview.do_it_btn;
        let card_hwnd = card.hwnd();

        unsafe {
            let _ = preview_control_subclass(
                start_hwnd,
                WM_KILLFOCUS,
                WPARAM(do_it_btn.0 as usize),
                LPARAM(0),
                PREVIEW_SUBCLASS_ID,
                card_hwnd.0 as usize,
            );
        }

        // Filtered to the WM_COMMAND range specifically (rather than 0..0,
        // which every other real-Win32 test in this module uses): an EDIT
        // control's OWN default WM_KILLFOCUS handling (reached via
        // DefSubclassProc, since `preview_control_subclass` does not
        // swallow WM_KILLFOCUS) legitimately posts its own WM_COMMAND
        // (EN_KILLFOCUS) to its parent regardless of this fix, and an
        // unfiltered peek would also pick up an unrelated synthesized
        // WM_PAINT for this never-validated test window. Neither is what
        // this test checks; only whether ID_PREVIEW_CANCEL specifically was
        // posted.
        let mut msg = MSG::default();
        let found = unsafe {
            PeekMessageW(
                &mut msg,
                Some(card_hwnd),
                WM_CARD_PREVIEW_COMMAND,
                WM_CARD_PREVIEW_COMMAND,
                PM_REMOVE,
            )
            .as_bool()
        };
        let cancelled = found && (msg.wParam.0 & 0xFFFF) as i32 == ID_PREVIEW_CANCEL;
        assert!(
            !cancelled,
            "focus moving between two preview controls must not cancel the preview"
        );
    }

    #[test]
    fn preview_closing_restores_the_previous_foreground_window() {
        // The settings window's own smoke test constructs a real HWND the
        // same way; reused here purely as "some other real window that
        // existed before the preview" -- never shown, never pumped.
        let settings_hwnd = unsafe {
            CreateWindowExW(
                windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
                PCWSTR(wide_z(TEST_CLASS_NAME).as_ptr()),
                PCWSTR(wide_z("other window").as_ptr()),
                windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(0),
                0,
                0,
                10,
                10,
                None,
                None,
                Some(instance()),
                None,
            )
        };
        // This environment may have no other window able to become the
        // foreground window (headless CI); the important, always-checkable
        // part of this test is that close_preview does not panic and always
        // leaves the card itself Hidden -- see the two asserts below, which
        // run regardless of whether CreateWindowExW above succeeded.
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, value) = calendar_schema_and_value();
        card.inner
            .show_preview("Add to calendar", &schema, &value, false);
        card.inner.preview_cancel();
        assert_eq!(card.state(), CardState::Hidden);
        if let Ok(hwnd) = settings_hwnd {
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
        }
    }

    #[test]
    fn preview_gdi_objects_do_not_leak_across_create_show_close_drop() {
        // Best-effort per the task's "if practical": GetGuiResources counts
        // GDI objects for the whole process, and `cargo test` runs many
        // tests concurrently on other threads, so this cannot assert an
        // exact before/after match without flaking on unrelated tests'
        // allocations. What it CAN assert without flaking: creating,
        // showing, editing, and closing a preview, then dropping the card,
        // must not leave the process's GDI object count higher than it was
        // immediately after the card's own fonts were built (i.e. nothing
        // preview-specific leaks), allowing generous slack for concurrent
        // tests.
        use windows::Win32::System::Threading::{
            GetCurrentProcess, GetGuiResources, GR_GDIOBJECTS,
        };

        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let baseline = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };

        let (schema, value) = calendar_schema_and_value();
        for _ in 0..3 {
            card.inner
                .show_preview("Add to calendar", &schema, &value, false);
            let _ = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
            card.inner.preview_cancel();
        }
        drop(card);

        let after = unsafe { GetGuiResources(GetCurrentProcess(), GR_GDIOBJECTS) };
        let slack = 64; // headroom for concurrently-running unrelated tests
        assert!(
            after <= baseline + slack,
            "GDI object count grew by more than {slack} after repeated preview create/close/drop \
             (baseline={baseline}, after={after}); investigate a leak before raising this slack"
        );
    }

    // -- issue #221: DrawTextW must not crash on empty headline/detail/preview
    // text --------------------------------------------------------------------
    //
    // `Answer::headline`/`Answer::detail` are plain, unvalidated `String`s
    // (`provider::common::answer_schema` has no `minLength`), and a
    // non-editable preview field's value can be empty too (an omitted or
    // empty proposal property -- see `PreviewModel::from_schema`'s
    // `unwrap_or_default()`). MEASURED 2026-09-17 (`src/ui/palette.rs`'s
    // `draw_text_line_tolerates_an_empty_string`, commit `453fe0b`, and
    // independently re-confirmed in this worktree by
    // `crate::ui::text::tests::raw_drawtextw_crashes_on_empty_text`, a
    // dedicated `#[ignore]`d test run in isolation -- see that module's own
    // doc comment): an unguarded `DrawTextW` call with a zero-length UTF-16
    // buffer -- what `utf16("")` produces -- reliably crashes with exit
    // code `0xC0000005` (`STATUS_ACCESS_VIOLATION`) through this crate's
    // `windows` 0.62 binding. These tests here do NOT re-run that raw,
    // unguarded call: doing so as part of this ordinary `cargo test`
    // invocation would take down this whole test binary
    // (`STATUS_ACCESS_VIOLATION` is not a catchable panic), losing every
    // other test in the same run. What these tests prove instead is that
    // painting an empty headline/detail/preview-field DOES NOT crash now
    // that every call site routes through
    // `crate::ui::text::draw_text_line`.

    #[test]
    fn paint_collapsed_with_empty_headline_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_answer("", "", 0, None);
        assert_eq!(card.state(), CardState::Collapsed);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_expanded_with_empty_headline_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        // `try_expand` only enters Expanded when `detail` is non-empty (see
        // its own `self.detail.is_empty()` early return), so a genuinely
        // empty detail can never reach Expanded through the public API --
        // that DrawTextW call is provably unreachable with empty text
        // (paint_expanded's own `if !self.detail.is_empty()` guard around
        // it), unlike the headline's, which is unconditional. This still
        // exercises the headline's DrawTextW call with an empty string, the
        // reachable half of the bug.
        card.show_answer("", "non-empty detail", 0, Some(Difficulty::Level(3)));
        card.inner.try_expand();
        assert_eq!(card.state(), CardState::Expanded);
        // Scroll past the fold so the "more below" hint band (also a
        // DrawTextW call site) paints too.
        card.inner.scroll_offset = 1;
        card.inner.scroll_max = 100;
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_error_with_empty_headline_and_detail_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        card.show_error("", "");
        assert_eq!(card.state(), CardState::Collapsed);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }

    #[test]
    fn paint_preview_with_empty_title_and_empty_field_value_does_not_crash() {
        let mut card = Card::new_for_test(instance()).expect("Card::new_for_test");
        let (schema, _) = calendar_schema_and_value();
        // "location" is a non-editable field (see calendar_schema_and_value's
        // schema): an empty value here reaches paint_preview's value
        // DrawTextW call directly, the same shape as a proposal that simply
        // omitted the property (PreviewModel::from_schema's
        // unwrap_or_default()).
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "", "notes": "bring laptop"
        });
        // An empty title also exercises paint_preview's own title DrawTextW
        // call.
        card.inner.show_preview("", &schema, &value, false);
        assert_eq!(card.state(), CardState::Preview);
        let handled = card.handle_message(WM_PAINT, WPARAM(0), LPARAM(0));
        assert!(handled.is_some());
    }
}
