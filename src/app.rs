//! Orchestration: the hidden owner window, the single message loop, and the
//! state machine that ties hotkeys, capture, providers and the card together.
//!
//! This module is wiring only — every piece of real logic lives in the module
//! that owns it. The one rule that matters here: the main thread owns every
//! `HWND` and runs the only message loop. Work that can block (capture is
//! quick, the API call is not) happens on a worker thread, which reports back
//! exclusively by `PostMessageW`.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use serde_json::Value;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{FILETIME, HINSTANCE, HWND, LPARAM, LRESULT, SYSTEMTIME, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::{
    FileTimeToSystemTime, SystemTimeToFileTime, SystemTimeToTzSpecificLocalTime,
    TzSpecificLocalTimeToSystemTime,
};
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, KillTimer, PostQuitMessage, RegisterClassExW, SetTimer, SetWindowLongPtrW,
    TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, MSG, PBT_APMRESUMEAUTOMATIC, SW_SHOWNORMAL,
    WINDOW_EX_STYLE, WM_APP, WM_DESTROY, WM_NCCREATE, WM_POWERBROADCAST, WM_TIMECHANGE, WM_TIMER,
    WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::actions;
use crate::capture;
use crate::config::{Config, Providers};
use crate::connectors::civil_time::{CivilDate, CivilDateTime};
use crate::dismiss::{unpack_point, ClickWatcher, WM_APP_DISMISS};
use crate::executors;
use crate::hotkey::{
    chord_to_string, Chord, HotkeyHook, HK_PRIMARY, HK_SECONDARY, WM_APP_HOTKEY, WM_APP_LEARNED,
    WM_APP_PALETTE_TOGGLE, WM_APP_PAUSE_TOGGLE,
};
use crate::mode::{self, Mode};
use crate::pause::{self, PauseChoice, PauseState};
use crate::provider::{
    calendar_request, parse_answer, physics_request, review_request_from_screen,
    review_request_from_text, Answer, Chain, Provider, Shot,
};
use crate::router;
use crate::ui::card::{Card, WM_APP_PREVIEW_DECIDED};
use crate::ui::confirm;
use crate::ui::palette::{Palette, WM_APP_PALETTE_RUN};
use crate::ui::palette_model::{self, DispatchTarget};
use crate::ui::settings;
use crate::ui::tray::{cmd, decode, register_taskbar_created, MenuChoice, Tray, WM_APP_TRAY};

/// Posted by the worker when a request finishes. `lparam` is
/// `Box::into_raw(Box::new(Result<Answer, String>))`; the handler takes
/// ownership and must reconstruct the `Box` to free it.
pub const WM_APP_RESULT: u32 = WM_APP + 3;

/// Posted by a second launch (the Copilot key with no argv to steer it) to
/// tell the running instance to act as if the hotkey fired. `WM_APP + 6`:
/// `+1` through `+5` are already claimed by the tray, hotkey and dismiss
/// modules. Add a new `WM_APP_*` constant to this module's
/// `tests::ALL_WM_APP_IDS` too (issue #163); `wm_app_ids_are_pairwise_unique`
/// and `wm_app_ids_registry_is_exhaustive` enforce the crate-wide list.
pub const WM_APP_ACTIVATE: u32 = WM_APP + 6;

/// Posted by the calendar worker thread (issue #39, `add_event_from_screen`)
/// when the provider chain finishes: `lparam` is
/// `Box::into_raw(Box::new(Result<serde_json::Value, String>))`, the
/// calendar-flow analogue of [`WM_APP_RESULT`]. Kept as its own message
/// (not reusing `WM_APP_RESULT`) because the two carry different boxed
/// payload types, and reconstructing the wrong one from a raw pointer is
/// undefined behaviour.
pub const WM_APP_CALENDAR_RESULT: u32 = WM_APP + 8;

/// Posted by the review-email worker thread (issue #38, `review_this_email`)
/// when the provider chain finishes: `lparam` is
/// `Box::into_raw(Box::new(Result<actions::review_email::ReviewOutcome, String>))`,
/// the review-flow analogue of [`WM_APP_CALENDAR_RESULT`]. `WM_APP + 10`:
/// `+9` is already [`crate::ui::card::WM_APP_PREVIEW_DECIDED`]. Add any new
/// `WM_APP_*` constant to `tests::ALL_WM_APP_IDS` too (issue #163).
pub const WM_APP_REVIEW_RESULT: u32 = WM_APP + 12;

/// Posted by the "Fill this form" worker thread (#40, `fill_form_from_screen`)
/// when local mapping (and, only if needed, the model) finish building a
/// `form_fill` proposal: `lparam` is `Box::into_raw(Box::new(Result<serde_json::Value,
/// String>))`, the fill_form-flow analogue of [`WM_APP_CALENDAR_RESULT`].
/// Kept as its own message for the same reason that one is: a different
/// boxed payload meaning than any other `WM_APP_*_RESULT`, even though the
/// Rust type happens to be the same `Result<Value, String>` -- see
/// `on_form_fill_result`'s own doc comment for the extra shapes this one
/// value can carry (an empty-profile signal, a no-fillable-fields signal,
/// or a real proposal). `WM_APP + 13`: `+9` through `+12` are already
/// `WM_APP_PREVIEW_DECIDED`/`WM_APP_PALETTE_TOGGLE`/`WM_APP_PALETTE_RUN`/
/// `WM_APP_REVIEW_RESULT`.
pub const WM_APP_FORM_FILL_RESULT: u32 = WM_APP + 13;

/// #24: posted by the intent router's worker thread when its classification
/// call finishes (success or failure). `lparam` is
/// `Box::into_raw(Box::new((u64, Result<router::RouterResult, String>)))` --
/// the `u64` is the generation the request was built for
/// (`Palette::router_generation()`, read right after `Palette::show`), so
/// the handler can tell a result for an already-hidden-or-reshown palette
/// from one for the session still on screen (`router::is_stale`, applied
/// inside `Palette::apply_router_suggestion`). The receiver takes ownership
/// and must reconstruct the `Box` to free it. `WM_APP + 14`: `+9` through
/// `+13` are already `WM_APP_PREVIEW_DECIDED`/`WM_APP_PALETTE_TOGGLE`/
/// `WM_APP_PALETTE_RUN`/`WM_APP_REVIEW_RESULT`/`WM_APP_FORM_FILL_RESULT`.
pub const WM_APP_ROUTER_RESULT: u32 = WM_APP + 14;

const WINDOW_CLASS: PCWSTR = w!("Wingman.Owner.Window.4d1b62f0");

/// How long to wait after hiding a visible card before capturing, so the
/// compositor has actually taken it off the screen. Without this the old card
/// can end up inside the screenshot we send to the model.
const CARD_SETTLE_MS: u64 = 60;

/// `SetTimer`'s `nIDEvent` for the one-shot pause-expiry timer (issue #20).
/// Killed as soon as it fires (turning it into a genuine one-shot -- a bare
/// `SetTimer` would otherwise repeat forever, rule 5) or whenever pausing is
/// re-armed or cancelled.
const PAUSE_TIMER_ID: usize = 1;

struct App {
    /// Kept so the settings window can be created on demand.
    instance: HINSTANCE,
    /// The runtime id of the shell's `TaskbarCreated` message (see
    /// `ui::tray`'s module docs). Compared against `msg` in `wnd_proc` to
    /// re-add the tray icon after Explorer crashes or restarts. Also mirrored
    /// into the `TASKBAR_CREATED_MSG` thread-local at startup, so `wnd_proc`
    /// can recognise it before `&mut App` exists -- see that thread-local's
    /// doc comment.
    taskbar_created_msg: u32,
    config: Config,
    chain: Arc<Chain>,
    card: Card,
    /// #25: the Quick Ask palette. Pre-created hidden at startup, shown and
    /// hidden repeatedly (never re-created) -- see `ui::palette`'s module
    /// doc comment.
    palette: Palette,
    tray: Tray,
    hook: Option<HotkeyHook>,
    /// Global click watcher. Armed only while an answer is on screen, so a
    /// click during the request cannot dismiss the pending spinner.
    watcher: Option<ClickWatcher>,
    /// A request is in flight; further triggers are ignored until it lands.
    busy: bool,
    last: Option<Answer>,
    /// Pause (issue #20). Not persisted across restart -- a restart is an
    /// explicit resume (see `pause.rs`'s module doc).
    pause: PauseState,
    /// Issue #112: the `(slot, chord)` last warned about by
    /// `hotkey_conflicts::decide`, so capturing the SAME conflicting chord
    /// twice in a row applies it instead of warning forever. `None` after a
    /// clean apply, a timeout, or a warn for a different pair -- see
    /// `on_learned`.
    pending_conflict: Option<(usize, Chord)>,
    /// Issue #38: the captured target and already-applied new text for a
    /// "Review this email" preview currently on screen, so
    /// `on_preview_decided` knows to build and run a `replace_text`
    /// proposal instead of resolving `"calendar_add"`. `Some` only between
    /// `on_review_result` showing the preview and the next
    /// `WM_APP_PREVIEW_DECIDED` (taken, and always cleared, by that
    /// handler regardless of Do it/Cancel) -- `None` the rest of the time,
    /// including while a calendar preview is on screen, so the two flows'
    /// previews can never be confused with each other.
    pending_review: Option<actions::review_email::ReviewContext>,
    /// #40: the merged, pre-approval `form_fill` proposal
    /// `actions::fill_form::build_proposal` built, kept between
    /// `on_form_fill_result` (which shows its translated preview) and
    /// `on_preview_decided` (which rebuilds the real proposal from it once
    /// "Do it" fires -- see `actions::fill_form::rebuild_after_confirm`).
    /// `None` whenever no fill_form preview is currently on screen; always
    /// taken (never merely read) at the top of `on_preview_decided`, so a
    /// Cancel on this preview can never leak into an unrelated LATER
    /// preview's decision.
    pending_form_fill: Option<Value>,
    /// #40: the `Undo` the most recent successful `fill_form` run returned,
    /// so "Restore last form" (tray) can put its fields back. `FnOnce`
    /// (`executors::Undo::undo` consumes it), so this is `take()`n on use --
    /// a second "Restore" click after a successful restore reports "nothing
    /// to restore" rather than attempting a stale undo twice.
    last_form_undo: Option<executors::Undo>,
}

pub fn run() -> Result<()> {
    // Before any window exists, so the card's metrics are right on a mixed-DPI
    // setup (the XPS panel next to an external monitor).
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // Claim the single-instance name before anything else is created. A
    // duplicate must not register a tray icon or install a keyboard hook,
    // because both would double every keypress -- and the second API call is
    // billed just like the first.
    let _instance = match crate::single_instance::acquire() {
        crate::single_instance::Instance::First(lock) => lock,
        crate::single_instance::Instance::Already => {
            // The shell launches this app by AUMID with no way to pass
            // arguments, so the Copilot key always arrives here bare; only a
            // launch explicitly carrying `--settings` should open Settings
            // instead of asking.
            let activation = crate::single_instance::activation_from_args(std::env::args());
            crate::single_instance::poke_existing(activation);
            return Ok(());
        }
    };

    // If autostart is on but aimed at an old path (the exe was moved or
    // rebuilt elsewhere), point it back here. Otherwise it fails silently
    // while Settings still reports it as enabled.
    crate::autostart::repair_if_stale();

    // One-time copilot-ask -> Wingman migration. Both steps are individually
    // idempotent (a no-op once done), so this runs unconditionally on every
    // launch rather than needing its own "have we migrated yet" flag.
    if let (Ok(old_path), Ok(new_path)) = (Config::old_path(), Config::path()) {
        let _ = Config::migrate_from(&old_path, &new_path);
    }
    let _ = crate::autostart::remove_old_run_value();

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)?.into() };
    let config = Config::load().unwrap_or_default();
    // Issue #19: publish the loaded mode to the process-wide atomic BEFORE
    // anything below can start a request (the hotkey hook isn't installed
    // yet, but `--settings`'s `open_settings` path below can still save,
    // and `provider::common`'s Offline guard must be correctly configured
    // from the very first request, not just from the first tray click).
    mode::set_current(config.mode);
    let chain = Arc::new(config.build_chain());

    let hwnd = create_owner_window(instance)?;

    // Registering does not depend on the window existing yet, but doing it
    // right after keeps every piece of startup wiring for the tray icon
    // together (see ui::tray's module docs on TaskbarCreated).
    let taskbar_created_msg = register_taskbar_created();
    // Mirror it before any message can arrive: `wnd_proc` needs this id to
    // recognise a TaskbarCreated broadcast while `SETTINGS_OPEN` is set,
    // i.e. before it is allowed to form `&mut App` at all.
    TASKBAR_CREATED_MSG.with(|c| c.set(taskbar_created_msg));

    let mut card = Card::new(instance).context("creating the notification card")?;
    card.set_text_scale(config.ui.text_scale);
    // Issue #39: so the preview card can PostMessageW WM_APP_PREVIEW_DECIDED
    // here when "Do it"/Cancel closes it -- see Card::set_owner's doc
    // comment.
    card.set_owner(hwnd);
    // #175: a stored key that could not be read is never silently dropped
    // (rule 7) -- the card is the first thing to exist that can show it.
    if !config.unreadable_secrets.is_empty() {
        let (headline, detail) = unreadable_secrets_card(&config.unreadable_secrets);
        card.show_error(&headline, &detail);
    }
    let tray = Tray::new(hwnd, instance).context("creating the tray icon")?;

    // #25: pre-created hidden here (rule 5: zero work while hidden --
    // App::toggle_palette only ever ShowWindow/HideWindow this, never
    // re-creates it).
    let mut palette = Palette::new(instance).context("creating the Quick Ask palette window")?;
    palette.set_owner(hwnd);

    let mut app = Box::new(App {
        instance,
        taskbar_created_msg,
        config,
        chain,
        card,
        palette,
        tray,
        hook: None,
        watcher: None,
        busy: false,
        last: None,
        pause: PauseState::Running,
        pending_conflict: None,
        pending_review: None,
        pending_form_fill: None,
        last_form_undo: None,
    });
    app.refresh_tray_labels();
    // Issue #19: reflect the loaded mode in the tray submenu/icon from the
    // start, same as `refresh_tray_labels` already does for the provider
    // submenu and key bindings.
    app.tray.set_mode(app.config.mode);

    // issue #149: install.ps1's post-install step launches the freshly
    // installed exe with `--settings`, expecting Settings to open so the
    // user can enter an API key. That only happened on the *duplicate*
    // launch path (`Instance::Already`, via `poke_existing`); a fresh
    // install is always the *first* instance, so nothing ever read argv
    // here and the flag was silently dropped. A bare launch (every other
    // caller: the Copilot key, the Start Menu entry, autostart at login)
    // must still do nothing, which is what `FirstLaunchAction::None` is for.
    if let crate::single_instance::FirstLaunchAction::OpenSettings =
        crate::single_instance::first_launch_action(std::env::args())
    {
        app.open_settings();
    }

    // The hook must be installed on the thread that pumps messages — this one.
    match HotkeyHook::install(
        hwnd,
        app.config.hotkeys.primary,
        app.config.hotkeys.secondary,
    ) {
        Ok(h) => {
            // #181: no default binding, so this is a no-op (packs to the
            // "unconfigured" sentinel) unless the owner hand-edited
            // config.toml.
            h.set_pause_chord(app.config.hotkeys.pause);
            // #25: no default binding, same as `pause` -- a no-op unless the
            // owner hand-edited config.toml.
            h.set_palette_chord(app.config.hotkeys.palette);
            app.hook = Some(h);
        }
        Err(e) => app.card.show_error(
            "Hotkeys unavailable",
            &format!("{e:#}\n\nUse Ask now from the tray menu instead."),
        ),
    }

    // Non-fatal: without it the card still auto-dismisses on its timer.
    app.watcher = ClickWatcher::install(hwnd).ok();

    // Hand the App to the window proc. It stays alive until WM_DESTROY.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize) };

    pump_messages();
    Ok(())
}

/// Card text for issue #175: one or more stored provider keys exist in
/// Credential Manager but could not be read back on this load. Pure so the
/// wording is unit-tested without a real `Card`/HWND (CLAUDE.md rule 8);
/// never includes any key material, only provider names, which are public
/// config labels, never secrets. No em dash (rule 11).
fn unreadable_secrets_card(providers: &[String]) -> (String, String) {
    let list = providers.join(", ");
    (
        "Stored API key unreadable".to_string(),
        format!(
            "Could not read the saved key for: {list}. It was not deleted. Retype it in Settings to replace it."
        ),
    )
}

fn create_owner_window(instance: HINSTANCE) -> Result<HWND> {
    let class = WNDCLASSEXW {
        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
        lpfnWndProc: Some(wnd_proc),
        hInstance: instance,
        lpszClassName: WINDOW_CLASS,
        ..Default::default()
    };
    if unsafe { RegisterClassExW(&class) } == 0 {
        anyhow::bail!("RegisterClassExW failed for the owner window");
    }

    // Never shown. It is a real top-level window rather than a message-only one
    // because TrackPopupMenu's dismiss-on-click-away workaround needs
    // SetForegroundWindow, which HWND_MESSAGE windows cannot receive.
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            WINDOW_CLASS,
            w!("Wingman"),
            WS_OVERLAPPED,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            0,
            0,
            None,
            None,
            Some(instance),
            None,
        )?
    };
    Ok(hwnd)
}

fn pump_messages() {
    let mut msg = MSG::default();
    // GetMessageW returns -1 on error; treat anything non-positive as "stop".
    while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Issue #214: whether `begin_model_action` bails at its very first step
/// (busy, then paused) or proceeds -- pulled out as a pure function so this
/// PRECEDENCE is exhaustively testable without a real `HWND`/`Card`/`Tray`
/// (an `App` cannot be constructed in a unit test at all -- see this
/// module's other tests, which only ever exercise pure helpers like
/// `readiness_gate` and `settings_reentrancy_policy` directly).
/// `begin_model_action`'s own `match` on this result, immediately followed
/// by its hide-stale-card/readiness-gate/`extra`/capture steps in that
/// fixed order, is the rest of the sequence; those later steps are not
/// folded into this table because the readiness gate needs `&Config` and
/// `extra` is a caller-supplied closure that may itself show a card, so
/// neither can be safely precomputed as a plain `bool` the way `busy` and
/// `paused` can.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ModelActionGate {
    /// A request is already in flight; every pre-#214 caller returned here
    /// silently (no card).
    Busy,
    /// Nothing Wingman does runs while paused (issue #20).
    Paused,
    /// Neither of the above: `begin_model_action` continues on to
    /// hide-stale-card, the readiness gate, `extra`, then capture.
    Proceed,
}

fn model_action_gate(busy: bool, paused: bool) -> ModelActionGate {
    if busy {
        ModelActionGate::Busy
    } else if paused {
        ModelActionGate::Paused
    } else {
        ModelActionGate::Proceed
    }
}

impl App {
    /// Issue #192: the card `App::ask`'s pre-flight gate should show, or
    /// `None` when at least one provider the current `Mode` would actually
    /// try is ready. Decided entirely from `config` -- no network call, so
    /// Auto mode's real Ollama reachability probe (`mode::probe_ollama_ready`,
    /// which only ever runs on the worker thread) never runs here.
    ///
    /// The old gate checked `self.chain`, built mode-agnostically by
    /// `Config::build_chain` -- every configured provider, regardless of
    /// `Mode`. That meant e.g. Local mode with only a cloud key configured
    /// passed the gate (the cloud provider is `ready()`), let capture run,
    /// and only failed later, on the worker, with a less specific "no
    /// providers configured" error. This selects with
    /// `Providers::build_chain_for_mode` instead, the same selection the
    /// worker itself uses.
    ///
    /// `ollama_ready: true` passed into `build_chain_for_mode` is the
    /// optimistic upper bound that function's own doc comment describes: it
    /// decides only whether Ollama counts as a candidate under Auto mode
    /// (so a user who configured it is never blocked here just because this
    /// gate cannot probe), never a claim that Ollama is actually reachable
    /// right now -- that real check still happens only on the worker.
    fn readiness_gate(
        mode: Mode,
        providers: &Providers,
        config_path: &str,
    ) -> Option<(String, String)> {
        let selected = providers.build_chain_for_mode(mode, true);
        if !selected.ready_provider_names().is_empty() {
            return None;
        }
        Some(match mode {
            Mode::Local | Mode::Offline => (
                "Local mode needs Ollama configured".to_string(),
                format!(
                    "Add \"ollama\" to providers.order and a base_url and model under [providers.ollama] in:\n{config_path}"
                ),
            ),
            Mode::Cloud => (
                "No API key: open Edit settings".to_string(),
                format!("Add a key under [providers.openai] or [providers.anthropic] in:\n{config_path}"),
            ),
            Mode::Auto => (
                "No provider ready: open Edit settings".to_string(),
                format!("Add a cloud API key, or configure Ollama, in:\n{config_path}"),
            ),
        })
    }

    /// Issue #214: the pause/busy/hide-stale-card/readiness/capture pipeline
    /// shared by every action that shows a pending card, captures the
    /// screen, and hands off to a worker thread which posts back one of the
    /// `WM_APP_*` result messages. Before this existed, `ask()`,
    /// `add_event_from_screen()` and `review_this_email()` each duplicated
    /// roughly the same 40 lines almost verbatim (#214's own body: "the
    /// overnight task instructions asked for app.rs edits to stay additive
    /// and minimal since another agent was editing it concurrently"). A
    /// future fill-form-from-screen action (fill_form.rs's own executor
    /// already exists; nothing in `app.rs` calls it yet) should call this
    /// too instead of adding a fourth near-copy.
    ///
    /// `model_action_gate` decides the first three steps' precedence (busy
    /// beats paused beats readiness-blocked); this method's own `if`s
    /// implement that same order, in the same sequence, so a future edit
    /// that reorders one without the other is a visible diff, not a silent
    /// drift. `extra` is the one point the three current callers differ at
    /// -- it runs after readiness passes and before capture (matching
    /// `add_event_from_screen`'s pre-#214 position for its
    /// `local_today_and_utc_offset()` read); `ask`/`review_this_email` pass
    /// one that does nothing. Like every other step here, `extra` must show
    /// its own card and return `None` to bail; `Some(value)` continues, and
    /// `value` is threaded back out unchanged so the caller can use it in
    /// its own worker closure.
    ///
    /// Returns `None` once this has already shown whatever card explains
    /// why (busy shows nothing at all, matching every pre-#214 caller; the
    /// other gates show their own paused/error card); `Some((raw,
    /// foreground_hwnd_isize, value))` once the pending card is showing and
    /// `self.busy` is `true`.
    fn begin_model_action<T>(
        &mut self,
        extra: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<(capture::RawShot, isize, T)> {
        match model_action_gate(self.busy, pause::is_paused_now()) {
            ModelActionGate::Busy => return None,
            ModelActionGate::Paused => {
                self.card
                    .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
                return None;
            }
            ModelActionGate::Proceed => {}
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        // Checked before capture: readiness is a cheap synchronous check, and
        // a machine with no key configured should never pay for a screenshot
        // grab (or have a capture failure mask the actually-actionable "No
        // API key" card) just to find out it has nothing to ask (issue #158).
        // Issue #192: gated on the MODE-AWARE selection (matching what the
        // worker will actually try), not the mode-agnostic self.chain --
        // otherwise e.g. Local mode with only a cloud key, or Cloud mode
        // with only Ollama, shows the less specific message.
        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "config.toml".into());
        if let Some((headline, detail)) =
            Self::readiness_gate(self.config.mode, &self.config.providers, &path)
        {
            self.card.show_error(&headline, &detail);
            return None;
        }

        let extra_value = extra(self)?;

        // Capture (grab the pixels and downscale) runs here, on the main
        // thread, and must happen before the pending card is shown --
        // otherwise the card is in its own screenshot. Encoding those pixels
        // to PNG does NOT happen here: issue #177 measured
        // `CompressionType::Best` PNG encoding at up to ~1s in a release
        // build on a 1402x876 image, which froze the message loop for that
        // whole time with nothing on screen after the key press. `encode`
        // runs on each caller's own worker thread instead, after
        // `show_pending`.
        //
        // Issue #169: the downscale target comes from the FIRST provider a
        // caller's own worker will actually try, not a provider-agnostic
        // heuristic. That real, mode-aware chain is only built on the
        // worker thread (its Ollama-reachability probe is a real network
        // call, and capture is the last thing allowed to block this
        // thread), so this reuses `readiness_gate`'s same optimistic,
        // network-free selection (`build_chain_for_mode(mode, true)`) --
        // "ready" here means "configured", not "reachable right now", which
        // is exactly the upper bound that gate's own doc comment describes.
        let optimistic_chain = self
            .config
            .providers
            .build_chain_for_mode(self.config.mode, true);
        let image_limits = optimistic_chain
            .first_ready_caps()
            .and_then(|caps| caps.image_limits);
        let (max_long_edge, max_pixels) =
            capture::resolve_limits(image_limits, self.config.capture.max_edge);
        let raw = match capture::grab_raw(&self.config.capture.monitor, max_long_edge, max_pixels) {
            Ok(r) => r,
            Err(e) => {
                self.card
                    .show_error("Couldn't capture the screen", &format!("{e:#}"));
                return None;
            }
        };
        // Issue #18/#206: the foreground window's HWND, captured HERE on the
        // main thread, right alongside the pixel capture -- both are "what
        // was actually on screen at press time", and both must be read
        // before `show_pending()` below puts Wingman's own card on top.
        // Only the isize is carried into the caller's worker closure (an
        // `HWND` wraps a raw pointer and is not `Send`; `hwnd_isize`/
        // `target` at each call site already use the same pattern for the
        // owner window). This HWND is used only lazily, inside the
        // non-vision fallback -- see `non_vision_inputs` -- so capturing it
        // costs nothing when every provider in the chain turns out to have
        // vision.
        let foreground_hwnd_isize =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;

        self.busy = true;
        // Disarmed for the whole in-flight window: a click while the spinner
        // is up must not touch the card.
        self.set_watch(false);
        self.card.show_pending();

        Some((raw, foreground_hwnd_isize, extra_value))
    }

    /// The whole flow: hide any stale card, check a provider is actually
    /// ready, grab the screen, then hand the bytes to a worker so the
    /// message loop stays responsive during the call.
    fn ask(&mut self) {
        let Some((raw, foreground_hwnd_isize, ())) = self.begin_model_action(|_| Some(())) else {
            return;
        };

        // Issue #19: the mode-aware chain is built fresh on the WORKER
        // thread (inside `worker`, below), not here on the main thread --
        // Auto mode's Ollama reachability probe is a real network call,
        // and capture is the last thing allowed to touch anything blocking
        // on this thread (the "Instant" constraint). `self.chain` (built
        // mode-agnostically, at config load/reload/switch time) stays the
        // main-thread-only "is anything configured at all" gate above and
        // the tooltip's source, unaffected by this.
        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let prompt = self.config.ui.prompt.clone();
        let want_difficulty = self.config.ui.show_difficulty;
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Answer, String> = (|| -> Result<Answer> {
                let shot = capture::encode(&raw)?;
                worker(
                    &providers,
                    mode,
                    &shot,
                    &raw,
                    foreground_hwnd_isize,
                    &prompt,
                    want_difficulty,
                )
            })()
            .map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// #25: shows or hides the Quick Ask palette. Toggling hide-if-visible
    /// (rather than always showing) matches both the tray item and the
    /// configured hotkey feeling like a single on/off press, and lets Esc
    /// (handled entirely inside `ui::palette`, no round trip through here)
    /// and a second press agree on what "off" means.
    ///
    /// Honors Pause the same way `ask()` does (issue #20: nothing Wingman
    /// does runs while paused) -- the hook already never posts
    /// `WM_APP_PALETTE_TOGGLE` while paused, and the tray's "Quick Ask" item
    /// is greyed while paused, but this guard is the one place both of
    /// those paths (plus any future caller) actually converge, so it stays
    /// here rather than trusting every caller to check first.
    fn toggle_palette(&mut self) {
        if self.palette.is_visible() {
            self.palette.hide();
            return;
        }
        if pause::is_paused_now() {
            return;
        }

        let resolved = match actions::load_actions() {
            Ok(r) => r,
            Err(e) => {
                self.card
                    .show_error("Couldn't load actions.toml", &format!("{e:#}"));
                return;
            }
        };
        let catalogue = palette_model::catalogue(&resolved);

        // Same mode-aware, network-free "is anything configured" selection
        // `ask()`'s `readiness_gate` and `worker`'s downscale-limit lookup
        // already use -- see #192's reasoning on why this must be mode-aware
        // rather than `self.chain` (built mode-agnostically).
        let selected = self
            .config
            .providers
            .build_chain_for_mode(self.config.mode, true);
        let model_configured = !selected.ready_provider_names().is_empty();

        let footer = palette_model::footer_line(
            self.config.mode.label(),
            self.first_provider_model_label().as_deref(),
        );

        // #24: candidates are built from the SAME catalogue about to be
        // shown, before it moves into `Palette::show` below.
        let candidates = router::candidates_from_catalogue(&catalogue);
        self.palette.show(catalogue, model_configured, footer);
        self.maybe_start_router(candidates);
    }

    /// #24: the intent router's hook, called right after
    /// [`App::toggle_palette`]'s `Palette::show` -- the palette is already
    /// visible and painted by the time this runs (`Palette::show` forces a
    /// synchronous first paint), so nothing here can add to the palette's
    /// own sub-100ms show latency; only the eventual card-free suggestion
    /// arrives late. Mirrors `App::ask`'s own capture-on-main-thread-then-
    /// spawn ordering.
    ///
    /// A silent no-op (never a card, never a hint) when: Paused; no
    /// provider is ready at all (the cheap, optimistic "is anything
    /// configured" check `ask`'s `readiness_gate` also uses); or the
    /// screenshot capture fails. Issue #24's "skip entirely when no
    /// provider is ready (no hint needed)" applies to every one of these --
    /// the router is a background convenience, never something whose
    /// failure the user is told about (rule 7's "every failure ends in a
    /// card" is about the action the user actually asked for; the router
    /// never was one).
    fn maybe_start_router(&mut self, candidates: Vec<router::RouterCandidate>) {
        if pause::is_paused_now() {
            return;
        }

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let optimistic_ready = !providers
            .build_chain_for_mode(mode, true)
            .ready_provider_names()
            .is_empty();
        if !optimistic_ready {
            return;
        }

        // #24: a heavily downscaled capture -- see `router::ROUTER_IMAGE_LONG_EDGE`'s
        // doc comment for why this is far smaller than the real action's
        // capture. Runs on the MAIN thread (Win32 capture must not run on a
        // worker thread, same constraint `ask` documents at its own capture
        // call site); encoding to PNG happens below, on the worker thread,
        // exactly like `ask` defers its own (more expensive) encode.
        let raw = match capture::grab_raw(
            &self.config.capture.monitor,
            router::ROUTER_IMAGE_LONG_EDGE,
            router::ROUTER_IMAGE_MAX_PIXELS,
        ) {
            Ok(r) => r,
            Err(_) => return,
        };

        let generation = self.palette.router_generation();
        let target = self.hwnd_isize();

        std::thread::spawn(move || {
            let result: std::result::Result<router::RouterResult, String> =
                (|| -> Result<router::RouterResult> {
                    let shot = capture::encode(&raw)?;
                    router_worker(&providers, mode, shot.png, &candidates)
                })()
                .map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new((generation, result)));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_ROUTER_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// #24: applies (or drops) the router's result against the palette's
    /// CURRENT session. `Palette::apply_router_suggestion` performs the
    /// actual staleness/threshold/interacted decision (`crate::router`'s
    /// pure functions) -- this method only unwraps the worker's `Result`
    /// and reads the live threshold from config. An `Err` (the router
    /// failed, or no provider ended up ready by the time the worker thread
    /// ran) is silently dropped, never a card -- see `maybe_start_router`'s
    /// doc comment for why.
    fn on_router_result(
        &mut self,
        generation: u64,
        result: std::result::Result<router::RouterResult, String>,
    ) {
        if let Ok(result) = result {
            let threshold = self.config.palette.router_threshold;
            self.palette
                .apply_router_suggestion(generation, &result, threshold);
        }
    }

    /// `"<name>:<model>"` for the first entry in `providers.order`, or just
    /// `"<name>"` when that provider has no distinct model field set to a
    /// non-empty value. `None` when nothing is configured at all -- the
    /// palette footer then shows just the mode label (see
    /// `palette_model::footer_line`).
    fn first_provider_model_label(&self) -> Option<String> {
        let name = self.config.providers.order.first()?;
        let model: &str = match name.as_str() {
            "openai" => &self.config.providers.openai.model,
            "anthropic" => &self.config.providers.anthropic.model,
            "gemini" => &self.config.providers.gemini.model,
            "ollama" => &self.config.providers.ollama.model,
            n if n.starts_with("compat:") => {
                let compat_name = &n["compat:".len()..];
                self.config
                    .providers
                    .compat
                    .iter()
                    .find(|c| c.name == compat_name)
                    .map(|c| c.model.as_str())
                    .unwrap_or("")
            }
            _ => "",
        };
        if model.is_empty() {
            Some(name.clone())
        } else {
            Some(format!("{name}:{model}"))
        }
    }

    /// #25: Enter in the palette routes here through the SAME dispatch table
    /// its rows were built from (`palette_model::dispatch_target_for`),
    /// which is unit tested to cover every built-in action id -- see that
    /// function's doc comment. Each arm calls the EXACT method the
    /// corresponding tray item already calls; there is no second,
    /// palette-only code path. A `None` target (an id the palette itself
    /// never produces) is a no-op, not a panic (rule 7).
    fn dispatch_palette_action(&mut self, action_id: &str) {
        match palette_model::dispatch_target_for(action_id) {
            Some(DispatchTarget::CheckMyWork) => self.ask(),
            Some(DispatchTarget::ExtractText) => self.extract_text(),
            Some(DispatchTarget::AddToCalendar) => self.add_event_from_screen(),
            Some(DispatchTarget::ReviewEmail) => self.review_this_email(),
            Some(DispatchTarget::FillForm) => self.fill_form_from_screen(),
            Some(DispatchTarget::CalculateSelection) => self.calculate_selection(),
            Some(DispatchTarget::CopyRegion) => self.copy_region(),
            None => {}
        }
    }

    /// "Copy text from screen" (#41): OCR the active monitor and copy the
    /// text to the clipboard, no model, no network. Mirrors `ask`'s
    /// ordering (hide any stale card, capture on the MAIN thread before the
    /// pending card shows, then do the expensive part -- here OCR, there
    /// `encode` plus the provider call -- on a worker thread, reporting back
    /// through the same `WM_APP_RESULT`/`on_result` path) except for one
    /// thing `ask` does that this must NOT: no readiness gate, no provider
    /// chain, no `Mode` is ever consulted -- this action works in every
    /// Mode, including Offline with no provider configured at all, because
    /// `actions::extract_text`'s functions have nowhere to get a chain from
    /// (see that module's doc comment). The only gate is Paused, same as
    /// `ask`.
    fn extract_text(&mut self) {
        if self.busy {
            return;
        }

        if let Err((headline, detail)) = actions::extract_text::gate(pause::is_paused_now()) {
            self.card.show_answer(headline, detail, 3, None);
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let raw = match actions::extract_text::capture_screen() {
            Ok(r) => r,
            Err(e) => {
                self.card
                    .show_error("Couldn't capture the screen", &format!("{e:#}"));
                return;
            }
        };

        self.busy = true;
        self.set_watch(false);
        self.card.show_pending();

        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Answer, String> =
                actions::extract_text::recognize_and_copy(&raw).map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// "Copy region to clipboard" (#29): opens the region/window-selection
    /// overlay and, on confirm, copies the crop to the clipboard as
    /// `CF_DIB` via the `"image_clipboard"` executor. Unlike `ask`/
    /// `extract_text`, this needs no worker thread: opening the overlay
    /// already blocks the caller (it pumps its own message loop --
    /// `ui::region::Overlay::run`) until the user is done, and the
    /// clipboard write itself is local CPU-only work, not a network call --
    /// there is nothing left here that would justify a second thread. On
    /// cancel (Esc/right-click) this shows no card at all, the same
    /// "backing out is not a failure" status `Ok(None)` gets everywhere
    /// else in this crate (e.g. a declined confirm-card proposal).
    fn copy_region(&mut self) {
        if self.busy {
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let raw = match crate::ui::region::select_region(self.instance) {
            Ok(Some(raw)) => raw,
            Ok(None) => return, // cancelled: no card, nothing changed
            Err(e) => {
                self.card
                    .show_error("Couldn't open the region selector", &format!("{e:#}"));
                return;
            }
        };

        let (width, height) = (raw.width, raw.height);
        let proposal_value = serde_json::json!({
            "rgba_base64": base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &raw.rgba,
            ),
            "width": width,
            "height": height,
        });

        let result: Result<()> = (|| {
            let executor = crate::executors::registry::resolve("image_clipboard")?;
            let confirmed = crate::ui::confirm::auto_confirm_read_only(
                executor.as_ref(),
                crate::ui::confirm::Proposal::new(proposal_value),
            )?;
            executor.execute(confirmed)?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                self.card
                    .show_answer(&format!("Copied {width}x{height} region"), "", 3, None)
            }
            Err(e) => self
                .card
                .show_error("Couldn't copy the region", &format!("{e:#}")),
        }
    }

    /// #39: "Add event from screen", the action framework's first real
    /// Look/Propose/Confirm/Do run -- the tray's second one-shot action,
    /// parallel to `ask()` (which stays wired to "check-my-work" exactly
    /// as before: this method does not touch it) but routed through the
    /// `add-to-calendar` built-in action (`actions::calendar::ACTION_ID`:
    /// proposal `calendar_event`, executor `calendar_add`, `confirm =
    /// true`) instead of a fixed request shape. Shares `ask()`'s pause/
    /// busy/readiness/capture steps via `begin_model_action` (#214); the
    /// one thing that differs is `local_today_and_utc_offset()`, passed as
    /// that helper's `extra` closure so it still runs exactly where it did
    /// before -- after the readiness gate, before capture.
    fn add_event_from_screen(&mut self) {
        let Some((raw, foreground_hwnd_isize, (today, offset_minutes))) =
            self.begin_model_action(|app| match local_today_and_utc_offset() {
                Ok(v) => Some(v),
                Err(e) => {
                    app.card
                        .show_error("Couldn't read the local date", &format!("{e:#}"));
                    None
                }
            })
        else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Value, String> = (|| -> Result<Value> {
                let shot = capture::encode(&raw)?;
                calendar_worker(
                    &providers,
                    mode,
                    &shot,
                    &raw,
                    foreground_hwnd_isize,
                    today,
                    offset_minutes,
                )
            })()
            .map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_CALENDAR_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// Handles the calendar worker's result (#39). The documented "no
    /// event" shape (`actions::calendar::is_no_event`) ends in a plain
    /// informational card with nothing further to confirm -- Cancel/no-op,
    /// per the flow's own `FlowState::Cancelled`. Any other successful
    /// proposal shows the preview card (or, if a hypothetical
    /// `actions.toml` override ever sets this action's `confirm = false`,
    /// tries to auto-confirm -- `ui::confirm::auto_confirm_read_only`
    /// correctly refuses that for a `Writes` executor, which is what
    /// makes this branch dead in practice today; see that function's own
    /// doc comment). A provider-chain failure shows an error card (rule
    /// 7).
    fn on_calendar_result(&mut self, result: std::result::Result<Value, String>) {
        self.busy = false;

        let proposal = match result {
            Ok(p) => p,
            Err(e) => {
                let headline = first_line(&e, 88);
                self.card.show_error(&headline, &e);
                self.set_watch(true);
                return;
            }
        };

        if actions::calendar::is_no_event(&proposal) {
            self.card.show_answer(
                "No event found on screen",
                "Point the Copilot key at something with a date and time, then try again.",
                5,
                None,
            );
            self.set_watch(true);
            return;
        }

        let resolved = match actions::load_actions() {
            Ok(r) => r,
            Err(e) => {
                self.card
                    .show_error("Couldn't load actions.toml", &format!("{e:#}"));
                self.set_watch(true);
                return;
            }
        };
        let action = resolved
            .iter()
            .find(|r| r.action.id == actions::calendar::ACTION_ID)
            .map(|r| &r.action);

        let confirm_required = action.map(|a| a.confirm).unwrap_or(true);
        if confirm_required {
            let schema = actions::schema::schema_for("calendar_event", false)
                .expect("\"calendar_event\" is always registered in actions::schema");
            self.card
                .show_preview("Add event from screen", &schema, &proposal, false);
            // Preview manages its own lifecycle (Do it / Cancel / Esc) and
            // takes real focus -- unlike Collapsed/Expanded it is never
            // dismissed by a click elsewhere (Card::show_preview's "Focus"
            // doc comment), so the global click watcher stays disarmed.
            return;
        }

        // `action.confirm == false`: only reachable via an actions.toml
        // override, since the built-in always sets `confirm = true`.
        let action = action.expect("checked above");
        match actions::resolve_executor(action) {
            Ok(executor) => {
                match confirm::auto_confirm_read_only(
                    executor.as_ref(),
                    confirm::Proposal::new(proposal),
                ) {
                    Ok(confirmed) => self.run_calendar_executor(executor.as_ref(), confirmed),
                    Err(e) => {
                        self.card
                            .show_error("Couldn't add the event", &format!("{e:#}"));
                        self.set_watch(true);
                    }
                }
            }
            Err(e) => {
                self.card
                    .show_error("Couldn't add the event", &format!("{e:#}"));
                self.set_watch(true);
            }
        }
    }

    /// The card's preview closed with a decision
    /// (`ui::card::WM_APP_PREVIEW_DECIDED`). `Card::take_confirmed()` is
    /// `None` for Cancel/Esc -- nothing runs, per Look/Propose/Confirm/Do:
    /// "Do" never happens without an explicit confirm -- and `Some` for
    /// "Do it".
    ///
    /// Three actions can leave a preview on screen (`add_event_from_screen`,
    /// `review_this_email`, `fill_form_from_screen`) and only one preview is
    /// ever on screen at a time, so exactly one of `self.pending_review` /
    /// `self.pending_form_fill` is `Some` -- or neither, for calendar's own
    /// flow. Both are `take()`n unconditionally, before `take_confirmed()`
    /// is even consulted, on Cancel/Esc as much as on "Do it", so a
    /// cancelled preview of either kind can never leave a stale slot behind
    /// for the next, unrelated preview's decision to pick up by mistake
    /// (#38, #40).
    ///
    /// #38: when `pending_review` is `Some`, this is "Review this email"'s
    /// decision -- `run_review_executor` resolves the `replace_text`
    /// executor against the stashed target.
    ///
    /// #40: when `pending_form_fill` is `Some`, this is "Fill this form"'s
    /// decision. The flat value the card confirmed is only the PREVIEW's
    /// translation (`actions::fill_form::build_preview_schema_and_value`),
    /// so it is rebuilt against the real merged proposal
    /// (`actions::fill_form::rebuild_after_confirm`) before running the
    /// `fill_form` executor.
    ///
    /// Otherwise (`pending_review` and `pending_form_fill` both `None`),
    /// this is calendar's own flow: the executor is resolved fresh by the
    /// fixed `"calendar_add"` name, the same "fixed until a second
    /// confirm-required action exists" status `CalendarAddExecutor::new`'s
    /// own fixed `"ics"` connector choice had before #38/#40 landed.
    fn on_preview_decided(&mut self) {
        let pending_review = self.pending_review.take();
        let pending_form_fill = self.pending_form_fill.take();
        let Some(confirmed) = self.card.take_confirmed() else {
            return;
        };

        if let Some(ctx) = pending_review {
            self.run_review_executor(ctx);
            return;
        }

        if let Some(original) = pending_form_fill {
            let rebuilt = actions::fill_form::rebuild_after_confirm(&original, confirmed.value());
            let token = confirm::user_confirmed();
            let final_confirmed = confirm::confirm(confirm::Proposal::new(rebuilt), token);
            match executors::registry::resolve("fill_form") {
                Ok(executor) => self.run_form_fill_executor(executor.as_ref(), final_confirmed),
                Err(e) => {
                    self.card
                        .show_error("Couldn't fill the form", &format!("{e:#}"));
                    self.set_watch(true);
                }
            }
            return;
        }

        match executors::registry::resolve("calendar_add") {
            Ok(executor) => self.run_calendar_executor(executor.as_ref(), confirmed),
            Err(e) => {
                self.card
                    .show_error("Couldn't add the event", &format!("{e:#}"));
                self.set_watch(true);
            }
        }
    }

    /// Runs `executor` against `confirmed` and shows the result card:
    /// honest per the connector design doc (`Undo.summary` already names
    /// the generated file and that undo only deletes it, never touching
    /// whatever the calendar app itself created) -- never a second action
    /// taken automatically. This is as far as "Do" goes; Wingman never
    /// presses Send, Submit, Buy or Pay.
    fn run_calendar_executor(
        &mut self,
        executor: &dyn executors::Executor,
        confirmed: confirm::Confirmed<Value>,
    ) {
        match executor.execute(confirmed) {
            Ok(undo) => {
                self.card
                    .show_answer("Event opened in your calendar app", &undo.summary, 0, None);
            }
            Err(e) => {
                self.card
                    .show_error("Couldn't add the event", &format!("{e:#}"));
            }
        }
        self.set_watch(true);
    }

    /// #38: "Review this email", the second action to run the full
    /// Look/Propose/Confirm/Do loop -- the tray's third one-shot action,
    /// parallel to `ask()`/`add_event_from_screen()` (neither of which this
    /// method touches). Shares `add_event_from_screen`'s pause/busy/
    /// readiness/capture steps via `begin_model_action` (#214), with one
    /// difference `begin_model_action` does not need to know about: the
    /// (potentially slow) UIA compose-body/selection capture attempts run
    /// on the SPAWNED worker thread below, never here, per `inputs::uia`'s
    /// and `inputs::selection`'s own module docs ("call from a dedicated
    /// worker thread"); only the screenshot -- needed only as the
    /// last-resort fallback, but cheap -- is still grabbed inside
    /// `begin_model_action`, exactly like `add_event_from_screen`'s already
    /// does.
    fn review_this_email(&mut self) {
        let Some((raw, foreground_hwnd_isize, ())) = self.begin_model_action(|_| Some(())) else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<actions::review_email::ReviewOutcome, String> =
                (|| -> Result<actions::review_email::ReviewOutcome> {
                    let shot = capture::encode(&raw)?;
                    review_worker(&providers, mode, &shot, &raw, foreground_hwnd_isize)
                })()
                .map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_REVIEW_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// Handles the review-email worker's result (#38). `verdict ==
    /// "good_to_go"` ends in a plain informational card, no preview, no "Do
    /// it" -- the task brief's own wording. Otherwise: edits are applied
    /// deterministically (`actions::review_email::apply_edits`) once, here,
    /// before the preview ever shows, so "Do it" later never re-derives
    /// anything. Only a [`actions::review_email::CapturedTarget`] (i.e. a
    /// `ComposeBody`-sourced review) gets a real preview with "Do it";
    /// `Selection`/`Screen` sources show the proposal as a read-only
    /// informational card instead, since there is nothing "Do it" could
    /// write back to (rule 7: never offer a button that cannot work).
    fn on_review_result(
        &mut self,
        result: std::result::Result<actions::review_email::ReviewOutcome, String>,
    ) {
        self.busy = false;

        let outcome = match result {
            Ok(o) => o,
            Err(e) => {
                let headline = first_line(&e, 88);
                self.card.show_error(&headline, &e);
                self.set_watch(true);
                return;
            }
        };

        if actions::review_email::is_good_to_go(&outcome.proposal) {
            self.card
                .show_answer("Good to go", "No changes needed.", 5, None);
            self.set_watch(true);
            return;
        }

        if !actions::review_email::source_has_target(outcome.source) {
            // Selection- or Screen-sourced: informational only, no target
            // to write back through (see this method's doc comment).
            let edits = actions::review_email::edits_from_value(&outcome.proposal);
            let detail = if edits.is_empty() {
                "No specific edits proposed.".to_string()
            } else {
                edits
                    .iter()
                    .map(|e| format!("{} -> {} ({})", e.before, e.after, e.reason))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            self.card
                .show_answer("Review needs edits", &detail, 0, None);
            self.set_watch(true);
            return;
        }
        let target = outcome
            .target
            .clone()
            .expect("source_has_target(outcome.source) is true, so ComposeBody always set target");

        let edits = actions::review_email::edits_from_value(&outcome.proposal);
        let applied = actions::review_email::apply_edits(&outcome.original_text, &edits);

        self.pending_review = Some(actions::review_email::ReviewContext {
            target,
            original_text: outcome.original_text.clone(),
            new_text: applied.new_text,
        });

        let schema = actions::schema::schema_for("text_review", false)
            .expect("\"text_review\" is always registered in actions::schema");
        self.card
            .show_preview("Review this email", &schema, &outcome.proposal, false);
        // Same "Preview manages its own lifecycle" reasoning as
        // `on_calendar_result`'s own return here -- the global click
        // watcher stays disarmed.
    }

    /// #38: the review flow's half of `on_preview_decided` -- resolves the
    /// `replace_text` executor and runs it against the stashed
    /// `pending_review` context via `actions::review_email::do_review_confirmed`.
    fn run_review_executor(&mut self, ctx: actions::review_email::ReviewContext) {
        match executors::registry::resolve("replace_text") {
            Ok(executor) => {
                match actions::review_email::do_review_confirmed(executor.as_ref(), &ctx) {
                    Ok(undo) => {
                        self.card
                            .show_answer("Email updated", &undo.summary, 0, None);
                    }
                    Err(e) => {
                        self.card
                            .show_error("Couldn't update the email", &format!("{e:#}"));
                    }
                }
            }
            Err(e) => {
                self.card
                    .show_error("Couldn't update the email", &format!("{e:#}"));
            }
        }
        self.set_watch(true);
    }

    /// #40: "Fill this form", the tray's fourth one-shot Look/Propose/Confirm/Do
    /// action. Shares `add_event_from_screen`/`review_this_email`'s pause/
    /// busy/readiness/capture steps via `begin_model_action` (#214, closing
    /// the "duplicated, not extracted" gap this doc comment used to note --
    /// #214 landed after this action was first written). Unlike calendar,
    /// this needs no today/UTC offset; it needs the foreground window's
    /// handle (for the worker's own UIA walk, run off this thread so the
    /// message loop stays responsive) and `config.forms.require_tick_for`
    /// (#40, expansion plan §15's still-owed owner decision -- see
    /// `config::RequireTickFor`), neither of which `begin_model_action`
    /// needs to know about, same as `review_this_email`'s `extra`. The
    /// model-free path (`form_fill_worker`'s call into
    /// `actions::fill_form::map_candidates_locally`, skipped entirely when
    /// every field maps from the local profile) lives inside the worker
    /// closure below, unaffected by this: `begin_model_action`'s readiness
    /// gate runs first either way, exactly as it did before this used the
    /// shared helper.
    fn fill_form_from_screen(&mut self) {
        let Some((raw, foreground_hwnd_isize, ())) = self.begin_model_action(|_| Some(())) else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let require_tick_for = self.config.forms.require_tick_for;
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Value, String> = (|| -> Result<Value> {
                let shot = capture::encode(&raw)?;
                form_fill_worker(
                    &providers,
                    mode,
                    &shot,
                    &raw,
                    foreground_hwnd_isize,
                    require_tick_for,
                )
            })()
            .map_err(|e| format!("{e:#}"));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_FORM_FILL_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    /// Handles the fill_form worker's result (#40). `value` carries one of
    /// three shapes: an empty-profile signal
    /// (`{"empty_profile": true, "toml_path": ..., "bin_path": ...}` --
    /// `form_fill_worker` returns this instead of ever building a UIA
    /// snapshot, per `actions::fill_form::load_or_import_profile`'s own
    /// "point at where to add profile data" decision), a proposal with no
    /// fillable fields at all (`actions::fill_form::has_fillable_fields`
    /// false -- nothing found on the foreground window, or every candidate
    /// was filtered out), or a real `form_fill` proposal, which is
    /// translated to the card's existing preview machinery
    /// (`actions::fill_form::build_preview_schema_and_value`) and shown --
    /// `self.pending_form_fill` is set here and consumed by
    /// `on_preview_decided`.
    fn on_form_fill_result(&mut self, result: std::result::Result<Value, String>) {
        self.busy = false;

        let value = match result {
            Ok(v) => v,
            Err(e) => {
                let headline = first_line(&e, 88);
                self.card.show_error(&headline, &e);
                self.set_watch(true);
                return;
            }
        };

        if value.get("empty_profile").and_then(Value::as_bool) == Some(true) {
            let toml_path = value.get("toml_path").and_then(Value::as_str).unwrap_or("");
            let bin_path = value.get("bin_path").and_then(Value::as_str).unwrap_or("");
            self.card.show_answer(
                actions::fill_form::EMPTY_PROFILE_HEADLINE,
                &actions::fill_form::empty_profile_detail(toml_path, bin_path),
                0,
                None,
            );
            self.set_watch(true);
            return;
        }

        if !actions::fill_form::has_fillable_fields(&value) {
            self.card.show_answer(
                "Nothing to fill",
                "No fillable fields were found on the foreground window.",
                5,
                None,
            );
            self.set_watch(true);
            return;
        }

        let (schema, preview_value) = actions::fill_form::build_preview_schema_and_value(&value);
        self.pending_form_fill = Some(value);
        self.card
            .show_preview("Fill this form", &schema, &preview_value, false);
    }

    /// Runs the `fill_form` executor and shows the result card (rule 5:
    /// what happened, not what was intended -- `Undo.summary`, built by
    /// `executors::fill_form::format_outcomes`, already lists filled,
    /// skipped and refused fields by name). On success, stashes the `Undo`
    /// for "Restore last form" (tray, `restore_last_form`).
    fn run_form_fill_executor(
        &mut self,
        executor: &dyn executors::Executor,
        confirmed: confirm::Confirmed<Value>,
    ) {
        match executor.execute(confirmed) {
            Ok(undo) => {
                self.card.show_answer("Form filled", &undo.summary, 0, None);
                self.last_form_undo = Some(undo);
            }
            Err(e) => {
                self.card
                    .show_error("Couldn't fill the form", &format!("{e:#}"));
            }
        }
        self.set_watch(true);
    }

    /// Tray "Restore last form" (#40's Undo requirement). Takes
    /// `self.last_form_undo` (an `Undo::undo` is `FnOnce`, consumed on use)
    /// so a second click after a successful restore reports "nothing to
    /// restore" rather than attempting a stale undo twice. Always shown in
    /// the tray (no dynamic enable/disable state) -- clicking it with
    /// nothing to restore is a plain informational card, never a dialog
    /// box (rule 7).
    fn restore_last_form(&mut self) {
        let Some(undo) = self.last_form_undo.take() else {
            self.card.show_answer(
                "Nothing to restore",
                "No form has been filled since Wingman started, or it was already restored.",
                5,
                None,
            );
            self.set_watch(true);
            return;
        };
        match undo.undo() {
            Ok(()) => {
                self.card.show_answer(
                    "Form restored",
                    "The fields this filled have been put back.",
                    5,
                    None,
                );
            }
            Err(e) => {
                self.card
                    .show_error("Couldn't fully restore the form", &format!("{e:#}"));
            }
        }
        self.set_watch(true);
    }

    fn on_result(&mut self, result: std::result::Result<Answer, String>) {
        let is_err = result.is_err();
        let answer = self.record_last(result);
        if is_err {
            self.card.show_error(&answer.headline, &answer.detail);
        } else {
            self.card.show_answer(
                &answer.headline,
                &answer.detail,
                self.config.ui.card_seconds,
                answer.difficulty,
            );
        }
        self.set_watch(true);
    }

    /// The part of `on_result` that doesn't touch the card: clears `busy`
    /// (the request really did finish, whether or not anything shows it) and
    /// records `self.last` so "Copy last answer" works, converting an `Err`
    /// into the same `Answer` shape `on_result` would have shown. Split out
    /// for issue #152: `open_settings` needs this half on its own, for the
    /// rare double-fault where a deferred answer arrives in the same
    /// Settings session as a `Config::save` failure -- the save error gets
    /// the card (the user just caused it directly), and the answer is not
    /// lost, just not shown as a card until the user checks "Copy last
    /// answer" or asks again.
    fn record_last(&mut self, result: std::result::Result<Answer, String>) -> Answer {
        self.busy = false;
        let answer = match result {
            Ok(answer) => answer,
            Err(e) => {
                let headline = first_line(&e, 88);
                Answer {
                    headline,
                    detail: e,
                    // An error has no difficulty to report.
                    difficulty: None,
                }
            }
        };
        self.last = Some(answer.clone());
        answer
    }

    fn copy_last(&mut self) {
        let Some(answer) = &self.last else {
            self.card.show_answer("Nothing to copy yet", "", 4, None);
            return;
        };
        let text = if answer.detail.is_empty() {
            answer.headline.clone()
        } else {
            format!("{}\n\n{}", answer.headline, answer.detail)
        };
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => self.card.show_answer("Copied", "", 3, None),
            Err(e) => self.card.show_error("Couldn't copy", &format!("{e}")),
        }
    }

    /// Issue #124: "Copy diagnostics" tray item. Builds a plain-text report
    /// (`diagnostics::render_report`) and puts it on the clipboard -- no
    /// file writes, no network. See `diagnostics.rs`'s module doc for the
    /// redaction guarantee.
    fn copy_diagnostics(&mut self) {
        let report = crate::diagnostics::render_report(&crate::diagnostics::collect(&self.config));
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(report)) {
            Ok(()) => self.card.show_answer(
                "Diagnostics copied",
                "Paste them into a bug report.",
                6,
                None,
            ),
            Err(e) => self
                .card
                .show_error("Couldn't copy diagnostics", &format!("{e}")),
        }
    }

    /// Issue #115: "Calculate selection" tray item. Model-free: reads the
    /// current selection (UIA, falling back to a clipboard-safe Ctrl+C --
    /// see `inputs::selection`) and evaluates it locally via `calc`, with no
    /// provider involved at all. Still honors Pause (the tray item is
    /// greyed out while paused, `ui::tray::build_menu`; this guard covers
    /// every other entry point the same way `ask`'s does) and still runs on
    /// a worker thread, because `get_selection_foreground` can block on a
    /// slow UIA provider or another app's own clipboard handling -- the
    /// message loop must stay responsive regardless.
    fn calculate_selection(&mut self) {
        if self.busy {
            return;
        }
        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to calculate.", 3, None);
            return;
        }

        self.busy = true;
        self.set_watch(false);
        self.card.show_pending();

        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Answer, String> = match crate::calc::run_on_selection(
                &crate::calc::ForegroundSelection,
            ) {
                crate::calc::SelectionCalcOutcome::Result { headline } => Ok(Answer {
                    headline,
                    detail: String::new(),
                    difficulty: None,
                }),
                crate::calc::SelectionCalcOutcome::NoSelection => Err(
                    "Nothing selected. Select an expression or a \"<number> <unit> in <unit>\" query first."
                        .to_string(),
                ),
                crate::calc::SelectionCalcOutcome::Error(e) => Err(e.to_string()),
            };
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                );
            }
        });
    }

    fn start_learning(&mut self, which: usize) {
        let Some(hook) = &self.hook else {
            self.card
                .show_error("Hotkeys unavailable", "The keyboard hook is not installed.");
            return;
        };
        hook.start_learning(which);
        let slot = if which == HK_PRIMARY {
            "Copilot key"
        } else {
            "secondary key"
        };
        self.card.show_answer(
            &format!("Press the {slot} now"),
            "Whatever you press next becomes the binding. Modifiers on their own are ignored. Times out in 10 seconds.",
            11,
            None,
        );
    }

    fn on_learned(&mut self, which: usize, chord: Chord) {
        // Issue #112: a known system/app shortcut warns and keeps the
        // previous binding, unless the user just confirmed by capturing the
        // exact same chord again for this slot -- see
        // `hotkey_conflicts`'s module doc comment for the full flow.
        match crate::hotkey_conflicts::decide(self.pending_conflict, which, chord) {
            crate::hotkey_conflicts::LearnDecision::Warn(conflict) => {
                self.pending_conflict = Some((which, chord));
                self.card.show_answer(
                    &format!(
                        "{} is already used by {}",
                        chord_to_string(&chord),
                        conflict.owner
                    ),
                    "Press the same key combo again to bind it anyway, or press a different one. The previous binding is unchanged.",
                    8,
                    None,
                );
                return;
            }
            crate::hotkey_conflicts::LearnDecision::Apply => {
                self.pending_conflict = None;
            }
        }

        if which == HK_PRIMARY {
            self.config.hotkeys.primary = chord;
        } else {
            self.config.hotkeys.secondary = chord;
        }
        if let Some(hook) = &self.hook {
            hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
        }
        let saved = self.config.save();
        self.refresh_tray_labels();

        let name = chord_to_string(&chord);
        match saved {
            Ok(()) => self
                .card
                .show_answer(&format!("Bound to {name}"), "", 4, None),
            Err(e) => self.card.show_error(
                &format!("Bound to {name}: not saved"),
                &format!("It will work until you quit.\n\n{e:#}"),
            ),
        }
    }

    fn reload(&mut self) {
        match Config::load() {
            Ok(config) => {
                // #175: report before self.config is overwritten, since the
                // freshly loaded config is the one whose hydrate ran.
                let unreadable = config.unreadable_secrets.clone();
                self.config = config;
                self.apply_config();
                if unreadable.is_empty() {
                    self.card.show_answer("Settings reloaded", "", 3, None);
                } else {
                    let (headline, detail) = unreadable_secrets_card(&unreadable);
                    self.card.show_error(&headline, &detail);
                }
            }
            Err(e) => self
                .card
                .show_error("Couldn't reload settings", &format!("{e:#}")),
        }
    }

    /// Open the GUI settings window. Modal: it runs its own message loop, so
    /// the hotkey is inert until it closes. On Save the new config is applied
    /// in full — the same path `reload` takes — so nothing gets half-applied.
    ///
    /// Issue #152: for the whole duration of `settings::show_modal` below,
    /// `wnd_proc` can be reentered -- that call never receives `self`, but it
    /// pumps every thread message on this thread, including ones addressed
    /// to *this* window. `open_settings` holds `&mut self` across that call,
    /// so nothing reachable from `self` may be written anywhere else while
    /// it runs (see `SETTINGS_OPEN`'s doc comment for why that is a real
    /// aliasing hazard, not just a logic bug). Every piece of state that a
    /// reentrant call needs to read or write therefore lives in a
    /// thread-local, not on `App`: `SETTINGS_OPEN` itself, `PENDING_MESSAGES`
    /// (issue #213: every worker result or preview decision that arrived
    /// mid-edit, oldest first), `TASKBAR_RECREATED_WHILE_SETTINGS`, and
    /// `PAUSE_REEVALUATE_PENDING` (issue #20). All four are only touched
    /// here, immediately before and after `show_modal`, when no reentrant
    /// call can possibly be in flight.
    ///
    /// Issue #178: the card has a single slot, and every branch below can
    /// show one -- a pending answer, a save error, or "Settings saved". If a
    /// tray-icon restore failure (`TASKBAR_RECREATED_WHILE_SETTINGS`) showed
    /// its card immediately, right here, every one of those branches would
    /// silently replace it, leaving the user with no tray icon and no
    /// explanation (rule 7). So the restore is still performed immediately
    /// (`try_restore_tray_icon` re-adds the real icon and refreshes its
    /// labels), but its error card, if any, is captured in
    /// `tray_restore_error` and shown LAST, after every other branch below
    /// has already shown whatever card it was going to show -- see
    /// `final_settings_card` (test-only) for a pure model of this ordering.
    ///
    /// Issue #213: unlike the old single-slot `WM_APP_RESULT`-only version,
    /// every deferred message is now delivered through its real handler
    /// (`deliver_deferred`) on every one of the three paths below --
    /// cancelled, save failed, and save succeeded -- rather than only being
    /// silently recorded (`record_last`, no card) on the save-failed path.
    /// A deferred `WM_APP_PREVIEW_DECIDED` "Do it" must actually run its
    /// executor regardless of whether the unrelated Settings save
    /// succeeded -- Look/Propose/Confirm/Do's contract, and rule 7, both
    /// outrank leaving the save-error card undisturbed. On the save-failed
    /// path specifically, `final_settings_card`'s `SaveError` invariant
    /// (unchanged by #213: the save error the user just directly caused
    /// always wins the single card slot) still holds -- each deferred
    /// message's own card is shown, and its effects genuinely happen, but
    /// the save-error card is re-shown immediately after so it is still the
    /// one left on screen.
    fn open_settings(&mut self) {
        // The card would sit on top of the settings window, and a click in
        // that window would dismiss it anyway.
        self.card.hide();
        self.set_watch(false);

        SETTINGS_OPEN.with(|c| c.set(true));
        let edited = settings::show_modal(self.instance, &self.config);
        SETTINGS_OPEN.with(|c| c.set(false));

        let pending: VecDeque<DeferredMessage> =
            PENDING_MESSAGES.with(|c| std::mem::take(&mut *c.borrow_mut()));
        let taskbar_recreated = TASKBAR_RECREATED_WHILE_SETTINGS.with(|c| c.replace(false));
        let tray_restore_error = if taskbar_recreated {
            self.try_restore_tray_icon()
        } else {
            None
        };
        if PAUSE_REEVALUATE_PENDING.with(|c| c.replace(false)) {
            self.reevaluate_pause(self.hwnd());
        }

        let Some(edited) = edited else {
            // Cancelled/closed without saving. Anything that finished
            // mid-edit still gets delivered.
            self.deliver_deferred(pending);
            self.show_tray_restore_error(tray_restore_error);
            return;
        };

        self.config = edited;
        if let Err(e) = self.config.save() {
            // Rule 7: neither failure may be silently dropped. Every
            // deferred message is delivered for real (its executor runs,
            // its own card shows) even though the unrelated config save
            // just failed -- but the save error is the problem the user
            // just directly caused, so it is re-shown last, restoring it as
            // the single card slot's final content (`final_settings_card`'s
            // `SaveError` case, unchanged by #213).
            let had_pending = !pending.is_empty();
            self.card
                .show_error("Couldn't save settings", &format!("{e:#}"));
            self.deliver_deferred(pending);
            if had_pending {
                self.card
                    .show_error("Couldn't save settings", &format!("{e:#}"));
            }
            self.show_tray_restore_error(tray_restore_error);
            return;
        }
        self.apply_config();

        if pending.is_empty() {
            self.card.show_answer("Settings saved", "", 3, None);
            self.set_watch(true);
        } else {
            self.deliver_deferred(pending);
        }
        self.show_tray_restore_error(tray_restore_error);
    }

    /// Issue #213: delivers every message `wnd_proc` deferred while Settings
    /// was open, in arrival (push) order, through the exact handler it would
    /// have reached had Settings not been open. Each handler shows its own
    /// card (and, for `PreviewDecided`, actually runs the confirmed
    /// executor); a later item's card visibly supersedes an earlier one's,
    /// same as if they had arrived that close together with no Settings
    /// window involved at all. A no-op on an empty queue.
    fn deliver_deferred(&mut self, pending: VecDeque<DeferredMessage>) {
        for message in pending {
            match message {
                DeferredMessage::Result(result) => self.on_result(result),
                DeferredMessage::CalendarResult(result) => self.on_calendar_result(result),
                DeferredMessage::ReviewResult(result) => self.on_review_result(result),
                DeferredMessage::FormFillResult(result) => self.on_form_fill_result(result),
                DeferredMessage::PreviewDecided => self.on_preview_decided(),
            }
        }
    }

    /// Issue #178: shows the tray-restore error card if `error` is `Some`,
    /// overwriting whatever card `open_settings` just showed -- called last,
    /// on every `open_settings` path, so this failure (the app's only
    /// persistent UI, per rule 7) is never the one silently dropped.
    fn show_tray_restore_error(&mut self, error: Option<String>) {
        if let Some(e) = error {
            self.card.show_error("Couldn't restore the tray icon", &e);
        }
    }

    /// Push `self.config` into everything that caches a piece of it.
    fn apply_config(&mut self) {
        self.chain = Arc::new(self.config.build_chain());
        self.card.set_text_scale(self.config.ui.text_scale);
        if let Some(hook) = &self.hook {
            hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
            // #181: a Reload/Settings-save always re-syncs the pause chord
            // too, the same way it already does for primary/secondary --
            // otherwise a hand-edited `[hotkeys.pause]` in config.toml would
            // only take effect after a full app restart.
            hook.set_pause_chord(self.config.hotkeys.pause);
        }
        self.refresh_tray_labels();
    }

    /// #174: `edit_settings` only needs to save when the file did not exist
    /// yet, and must open the shell only when that save (if attempted)
    /// actually succeeded -- otherwise `ShellExecuteW` opens a path that
    /// still does not exist, or exists with stale defaults, with nothing on
    /// screen to say so. Pure so this one-branch decision is unit-tested
    /// directly (CLAUDE.md rule 8) rather than only through a live
    /// Credential-Manager failure, which `edit_settings` itself cannot be
    /// unit-tested against (it owns a real `Card`/`HWND`).
    fn should_open_config_after_ensuring_it_exists(
        needed_create: bool,
        save_result: &Result<()>,
    ) -> bool {
        !needed_create || save_result.is_ok()
    }

    fn edit_settings(&mut self) {
        let Ok(path) = Config::path() else {
            self.card
                .show_error("Couldn't locate config.toml", "%APPDATA% is not readable.");
            return;
        };
        // Make sure the file exists before asking the shell to open it.
        let needed_create = !path.exists();
        let save_result = if needed_create {
            self.config.save()
        } else {
            Ok(())
        };
        if !Self::should_open_config_after_ensuring_it_exists(needed_create, &save_result) {
            // Rule 7: the save the user's click just caused must not fail
            // silently -- before this fix, `let _ = self.config.save();`
            // dropped the error and still tried to open a file that might
            // not exist.
            if let Err(e) = &save_result {
                self.card
                    .show_error("Couldn't save settings", &format!("{e:#}"));
            }
            return;
        }
        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            );
        }
    }

    /// Apply a model chosen from a tray submenu. The index addresses the
    /// tray's own list, which is not necessarily the config's — it appends a
    /// hand-edited current model that is missing from `models` — so the name
    /// is resolved from the tray rather than re-indexed here.
    fn pick_model(&mut self, openai: bool, index: usize) {
        let picked = if openai {
            self.tray.openai_model_at(index)
        } else {
            self.tray.anthropic_model_at(index)
        }
        .map(str::to_owned);

        let Some(model) = picked else { return };

        let slot = if openai {
            &mut self.config.providers.openai.model
        } else {
            &mut self.config.providers.anthropic.model
        };
        if *slot == model {
            return;
        }
        *slot = model.clone();

        self.chain = Arc::new(self.config.build_chain());
        let saved = self.config.save();
        self.refresh_tray_labels();

        match saved {
            Ok(()) => self.card.show_answer(&model, "", 3, None),
            Err(e) => self.card.show_error(
                &format!("Using {model}: not saved"),
                &format!(
                    "It will revert when you quit.

{e:#}"
                ),
            ),
        }
        self.set_watch(true);
    }

    /// Move a provider to the front of `providers.order`, making it the one
    /// that answers. The other stays in the list as the fallback rather than
    /// being dropped, so switching never costs you the second provider.
    fn set_provider(&mut self, openai: bool) {
        let want = if openai { "openai" } else { "anthropic" };
        let order = &mut self.config.providers.order;
        if order.first().map(String::as_str) == Some(want) {
            return;
        }
        order.retain(|p| p != want);
        order.insert(0, want.to_string());

        self.chain = Arc::new(self.config.build_chain());
        let saved = self.config.save();
        self.refresh_tray_labels();

        let name = if openai { "ChatGPT" } else { "Claude" };
        match saved {
            Ok(()) => self.card.show_answer(name, "", 3, None),
            Err(e) => self.card.show_error(
                &format!("Using {name}: not saved"),
                &format!(
                    "It will revert when you quit.

{e:#}"
                ),
            ),
        }
        self.set_watch(true);
    }

    /// Issue #19: switch `Mode`, publish it to the process-wide atomic the
    /// Offline guard reads (`mode::set_current`), radio-check it in the
    /// tray and update the icon (`Tray::set_mode`), and persist it --
    /// mirrors `set_provider`'s shape exactly. Unlike Pause, Mode IS
    /// persisted (`Config.mode`), so unlike `pause_for`/`resume` this
    /// writes to disk, same as every other tray setting change.
    fn set_mode(&mut self, mode: Mode) {
        if self.config.mode == mode {
            return;
        }
        self.config.mode = mode;
        mode::set_current(mode);
        self.tray.set_mode(mode);

        let saved = self.config.save();
        match saved {
            Ok(()) => self.card.show_answer(mode.label(), "", 3, None),
            Err(e) => self.card.show_error(
                &format!("Using {}: not saved", mode.label()),
                &format!(
                    "It will revert when you quit.

{e:#}"
                ),
            ),
        }
        self.set_watch(true);
    }

    fn refresh_tray_labels(&mut self) {
        let primary = chord_to_string(&self.config.hotkeys.primary);
        let secondary = chord_to_string(&self.config.hotkeys.secondary);
        self.tray.set_key_bindings(&primary, &secondary);

        let p = &self.config.providers;
        let openai_first = p.order.first().map(String::as_str) != Some("anthropic");
        self.tray.set_active_provider(openai_first);
        self.tray.set_models(
            &p.openai.models,
            &p.openai.model,
            &p.anthropic.models,
            &p.anthropic.model,
        );

        self.update_tooltip();
    }

    /// Set the tray tooltip for the current state: the pause text while
    /// paused (issue #20), otherwise the normal "ready providers" summary
    /// `refresh_tray_labels` always showed. Split out so pausing/resuming
    /// can update just the tooltip without touching the key-binding labels
    /// or model submenus, which haven't changed.
    fn update_tooltip(&mut self) {
        if let PauseState::Paused { choice, until } = self.pause {
            let hour_min = until.and_then(local_hour_min);
            self.tray
                .set_tooltip(&pause::tooltip_text(choice, hour_min));
            return;
        }

        let primary = chord_to_string(&self.config.hotkeys.primary);
        let ready = self.chain.ready_provider_names();
        let tip = if ready.is_empty() {
            "Wingman: no API key configured".to_string()
        } else {
            format!("Wingman: {} · {primary}", ready.join(", "))
        };
        self.tray.set_tooltip(&tip);
    }

    /// Enter Pause for `choice` (issue #20): compute the deadline, publish
    /// it to the lock-free flag the hook reads (`pause::set_paused`), arm
    /// the one-shot expiry timer if there is a deadline, grey the tray icon,
    /// and switch the tooltip.
    ///
    /// Deliberately does not cancel an in-flight request: the guard in
    /// `ask` stops the *next* request from starting, it does not abort one
    /// already running.
    fn pause_for(&mut self, choice: PauseChoice) {
        let now = SystemTime::now();
        let until = match choice {
            PauseChoice::OneHour => Some(pause::deadline_one_hour(now)),
            PauseChoice::UntilTomorrow => match deadline_until_tomorrow() {
                Ok(t) => Some(t),
                Err(e) => {
                    self.card.show_error(
                        "Couldn't compute tomorrow's pause deadline",
                        &format!("{e:#}"),
                    );
                    return;
                }
            },
            PauseChoice::UntilResumed => None,
        };

        self.pause = PauseState::Paused { choice, until };
        pause::set_paused(until);

        let hwnd = self.hwnd();
        unsafe {
            let _ = KillTimer(Some(hwnd), PAUSE_TIMER_ID);
        }
        if let Some(t) = until {
            arm_pause_timer(hwnd, t, now);
        }

        self.tray.set_paused(true);
        self.card.hide();
        self.set_watch(false);
        self.update_tooltip();
    }

    /// Leave Pause (issue #20), whether from the "Resume" menu item or
    /// because a deadline was reached ([`App::reevaluate_pause`]).
    fn resume(&mut self) {
        self.pause = PauseState::Running;
        pause::set_running();
        let hwnd = self.hwnd();
        unsafe {
            let _ = KillTimer(Some(hwnd), PAUSE_TIMER_ID);
        }
        self.tray.set_paused(false);
        self.update_tooltip();
    }

    /// #181: flips Pause on a pause-toggle-chord press -- resume if
    /// currently paused (any choice, any deadline), otherwise pause "until
    /// resumed", the one direction a bare keypress can express (1h /
    /// until-tomorrow remain tray-menu-only, same as before). Mirrors the
    /// `cmd::RESUME` / `cmd::PAUSE_UNTIL_RESUMED` tray commands exactly --
    /// see [`pause_toggle_action`] for the pure paused/not-paused decision
    /// this dispatches on.
    fn toggle_pause(&mut self) {
        let now = SystemTime::now();
        match pause_toggle_action(self.pause.is_paused(now)) {
            PauseToggleAction::Resume => self.resume(),
            PauseToggleAction::PauseUntilResumed => self.pause_for(PauseChoice::UntilResumed),
        }
    }

    /// `WM_TIMER` fired for [`PAUSE_TIMER_ID`]. `SetTimer` without a
    /// `TIMERPROC` keeps re-posting `WM_TIMER` at the same interval until
    /// `KillTimer` is called (rule 5: never a polling timer), so the very
    /// first thing this does is kill it -- turning it into a genuine
    /// one-shot -- before re-checking the deadline against the wall clock.
    fn on_pause_timer(&mut self, hwnd: HWND) {
        unsafe {
            let _ = KillTimer(Some(hwnd), PAUSE_TIMER_ID);
        }
        self.reevaluate_pause(hwnd);
    }

    /// Re-check the pause deadline against the wall clock. Called when the
    /// timer fires, and once on wake (`WM_POWERBROADCAST` /
    /// `PBT_APMRESUMEAUTOMATIC`) and on `WM_TIMECHANGE`: a `SetTimer` does
    /// not run during sleep, so the deadline may already have passed by the
    /// time the machine wakes, and a manual clock change can move the
    /// deadline without ever posting `WM_TIMER` at all. If the deadline
    /// has passed, resume; otherwise re-arm the timer for the corrected
    /// remaining delay (guards against both of those cases leaving a stale
    /// timer behind).
    fn reevaluate_pause(&mut self, hwnd: HWND) {
        if let PauseState::Paused { until: Some(t), .. } = self.pause {
            let now = SystemTime::now();
            if self.pause.is_paused(now) {
                arm_pause_timer(hwnd, t, now);
            } else {
                self.resume();
            }
        }
    }

    fn hwnd(&self) -> HWND {
        HWND(self.hwnd_isize() as *mut _)
    }

    /// Handle the shell's `TaskbarCreated` broadcast (Explorer crashed or
    /// was restarted): re-add the tray icon and restore its tooltip, which
    /// `NIM_ADD` resets to the default. Per rule 7, a failure here still
    /// ends in a card rather than silently leaving the tray empty. Thin
    /// wrapper around [`App::try_restore_tray_icon`] that shows the card
    /// immediately -- correct for this method's only other caller
    /// (`wnd_proc`'s direct, non-Settings path, where nothing else is about
    /// to show a card right after). `open_settings` calls
    /// `try_restore_tray_icon` directly instead, so it can defer the card
    /// (issue #178 -- see that method's doc comment).
    fn on_taskbar_created(&mut self) {
        if let Some(e) = self.try_restore_tray_icon() {
            self.card.show_error("Couldn't restore the tray icon", &e);
        }
    }

    /// Re-adds the tray icon and refreshes its labels, WITHOUT touching the
    /// card. `Some(message)` on failure (never shown here -- the caller
    /// decides when); `None` on success (labels already refreshed).
    fn try_restore_tray_icon(&mut self) -> Option<String> {
        match self.tray.readd() {
            Ok(()) => {
                self.refresh_tray_labels();
                None
            }
            Err(e) => Some(format!("{e:#}")),
        }
    }

    fn set_watch(&self, on: bool) {
        if let Some(w) = &self.watcher {
            if on {
                w.arm();
            } else {
                w.disarm();
            }
        }
    }

    /// A click reported by the global watcher. Clicks that land on the card
    /// itself belong to the card (they expand it); everything else closes it.
    fn on_global_click(&mut self, x: i32, y: i32) {
        use crate::ui::card::CardState;
        if matches!(self.card.state(), CardState::Hidden) {
            self.set_watch(false);
            return;
        }
        if self.point_in_card(x, y) {
            return;
        }
        self.card.hide();
        self.set_watch(false);
    }

    fn point_in_card(&self, x: i32, y: i32) -> bool {
        let mut rc = windows::Win32::Foundation::RECT::default();
        let ok = unsafe {
            windows::Win32::UI::WindowsAndMessaging::GetWindowRect(self.card.hwnd(), &mut rc)
        }
        .is_ok();
        ok && x >= rc.left && x < rc.right && y >= rc.top && y < rc.bottom
    }

    fn hwnd_isize(&self) -> isize {
        OWNER_HWND.with(|h| h.get())
    }
}

/// Issue #19: builds the mode-aware chain HERE, on the worker thread, from
/// `providers` + `mode` -- not on the main thread before spawning -- because
/// Auto mode's Ollama reachability probe (`mode::probe_ollama_ready`) is a
/// real network call, and the main thread must stay off the network
/// entirely (capture already runs there; nothing after it may block on I/O,
/// the "Instant" constraint in the expansion plan). Cloud, Local and
/// Offline need no probe at all and pay nothing extra --
/// `mode::should_probe_ollama` gates it, and it only ever runs for Auto.
///
/// #23: also loads and resolves the action catalog (built-ins merged with
/// `%APPDATA%\Wingman\actions.toml`) and runs the hotkey's one default
/// action (`actions::DEFAULT_ACTION_ID`, "Check my work") through it,
/// instead of building the physics request straight from config. `ui_prompt`
/// and `ui_show_difficulty` are `config.ui.prompt` /
/// `config.ui.show_difficulty` exactly as before -- see the design spec's
/// "Origin tracking" section for why they are only the FALLBACK now, not
/// the only source: an `actions.toml` override of the default action's
/// `prompt` wins outright (Origin::User), while `ui_show_difficulty` and
/// the action's own `rate_difficulty` OR together, so the existing global
/// toggle keeps working as an override for existing users (#197 part 2).
///
/// A malformed `actions.toml`, or the default action being disabled/missing,
/// surfaces as `Err` here exactly like any other worker failure -- `ask()`'s
/// caller already turns that into an error card (rule 7), so no new UI code
/// is needed for #23's "reported in a card, not a crash".
fn worker(
    providers: &Providers,
    mode: Mode,
    shot: &Shot,
    raw: &capture::RawShot,
    foreground_hwnd: isize,
    ui_prompt: &str,
    ui_show_difficulty: bool,
) -> Result<Answer> {
    let resolved = actions::load_actions().context("failed to load actions")?;
    let default = actions::default_action(&resolved).with_context(|| {
        format!(
            "the default action (\"{}\") is disabled or missing; check actions.toml",
            actions::DEFAULT_ACTION_ID
        )
    })?;

    let (prompt, want_difficulty) =
        resolve_prompt_and_difficulty(default, ui_prompt, ui_show_difficulty);

    let ollama_ready = mode == Mode::Auto
        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
        && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
    let chain = providers.build_chain_for_mode(mode, ollama_ready);

    // #12: the trait moved from `Provider::ask(shot, prompt, want_difficulty)
    // -> Answer` to `Provider::complete(&Request) -> Completion`, so the
    // physics-check schema is now built here (via `physics_request`) instead
    // of inside each provider.
    //
    // #176: `parse_answer` has to run *inside* the chain's fallback loop
    // (via `complete_parsed`), not after `complete` returns, so a
    // schema-invalid 200 from one provider falls through to the next ready
    // provider instead of failing the whole request.
    //
    // #18/#206: `complete_parsed_with_fallback` (not the plain
    // `complete_parsed`) so that a provider in `chain` reporting
    // `Caps.vision == false` gets OCR text plus the compact UIA field
    // snapshot instead of `shot`'s image -- decided per provider, inside
    // the chain's own fallback loop, never by rewriting `req` up front here.
    // `non_vision_inputs` (below) is the lazy source: it only actually runs
    // OCR/UIA the first time some provider in `chain` needs it, never for a
    // chain where every ready provider has vision.
    let req = physics_request(shot, prompt, want_difficulty);
    chain.complete_parsed_with_fallback(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| parse_answer(&c.text),
    )
}

/// #39: the calendar-flow analogue of [`worker`] -- loads and resolves the
/// `add-to-calendar` action (respecting an `actions.toml` override of its
/// `prompt`, the same precedence every action gets), builds the calendar
/// request with today's local date and UTC offset baked into the prompt
/// (`actions::calendar::build_prompt`), and runs it through the identical
/// mode-aware provider chain, retry/repair and non-vision fallback
/// [`worker`] already uses. The only real difference: `parse` is
/// `actions::calendar::parse_calendar_proposal` (a raw `Value`) instead of
/// [`parse_answer`] (a typed `Answer`), since the `calendar_event`
/// proposal has no dedicated Rust struct -- see that function's doc
/// comment.
fn calendar_worker(
    providers: &Providers,
    mode: Mode,
    shot: &Shot,
    raw: &capture::RawShot,
    foreground_hwnd: isize,
    today: CivilDate,
    utc_offset_minutes: i32,
) -> Result<Value> {
    let resolved = actions::load_actions().context("failed to load actions")?;
    let action = resolved
        .iter()
        .find(|r| r.action.id == actions::calendar::ACTION_ID)
        .map(|r| &r.action)
        .with_context(|| {
            format!(
                "the \"{}\" action is disabled or missing; check actions.toml",
                actions::calendar::ACTION_ID
            )
        })?;

    let prompt = actions::calendar::build_prompt(&action.prompt, today, utc_offset_minutes);

    let ollama_ready = mode == Mode::Auto
        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
        && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
    let chain = providers.build_chain_for_mode(mode, ollama_ready);

    let req = calendar_request(shot, &prompt);
    chain.complete_parsed_with_fallback(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| actions::calendar::parse_calendar_proposal(&c.text),
    )
}

/// #38: the review-email flow's analogue of [`worker`]/[`calendar_worker`].
/// Unlike those two, the "what does the model actually see" decision is not
/// fixed to a screenshot: [`actions::review_email::capture_input`] runs
/// FIRST, on this worker thread (never the main thread -- see
/// `App::review_this_email`'s doc comment), and its result decides whether
/// the request carries the captured text (`review_request_from_text`) or
/// falls back to `shot` (`review_request_from_screen`). The provider chain,
/// retry/repair and non-vision fallback machinery are otherwise identical
/// to `worker`/`calendar_worker`.
fn review_worker(
    providers: &Providers,
    mode: Mode,
    shot: &Shot,
    raw: &capture::RawShot,
    foreground_hwnd: isize,
) -> Result<actions::review_email::ReviewOutcome> {
    let resolved = actions::load_actions().context("failed to load actions")?;
    let action = resolved
        .iter()
        .find(|r| r.action.id == actions::review_email::ACTION_ID)
        .map(|r| &r.action)
        .with_context(|| {
            format!(
                "the \"{}\" action is disabled or missing; check actions.toml",
                actions::review_email::ACTION_ID
            )
        })?;

    let captured = actions::review_email::capture_input(foreground_hwnd);
    let source = captured.source();
    let original_text = captured.text().unwrap_or_default().to_string();
    let target = captured.target().cloned();

    let req = match captured.text() {
        Some(text) => review_request_from_text(&action.prompt, text),
        None => review_request_from_screen(&action.prompt, shot),
    };

    let ollama_ready = mode == Mode::Auto
        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
        && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
    let chain = providers.build_chain_for_mode(mode, ollama_ready);

    let proposal = chain.complete_parsed_with_fallback(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| actions::review_email::parse_text_review_proposal(&c.text),
    )?;

    Ok(actions::review_email::ReviewOutcome {
        proposal,
        original_text,
        target,
        source,
    })
}

/// #40: the "Fill this form" worker. Loads (and, per
/// `actions::fill_form::load_or_import_profile`, possibly one-shot-imports)
/// the profile first -- an empty profile short-circuits to the
/// `"empty_profile"` signal `on_form_fill_result` recognizes, before ever
/// walking the UIA tree. `foreground_hwnd` was captured on the main thread
/// by `App::fill_form_from_screen` (via `GetForegroundWindow()`, before this
/// thread spawned); the actual UIA walk (`inputs::uia::snapshot_hwnd`, a
/// live COM call that can block) runs HERE, off the main thread, same
/// reasoning `inputs::uia`'s own module doc comment gives for never calling
/// it from the hook thread. The model is only ever asked about fields stage
/// 1 (`actions::fill_form::map_candidates_locally`) could not map -- if
/// `unmapped` is empty, `chain.complete_parsed_with_fallback` is never
/// called at all, which is exactly what this file's live `#[ignore]`d test
/// in `actions::fill_form` (`fill_form_live_local_mapping_fills_a_whole_form_with_zero_model_calls`)
/// asserts at the pure-logic layer this function is built from.
fn form_fill_worker(
    providers: &Providers,
    mode: Mode,
    shot: &Shot,
    raw: &capture::RawShot,
    foreground_hwnd: isize,
    require_tick_for: crate::config::RequireTickFor,
) -> Result<Value> {
    let profile = actions::fill_form::load_or_import_profile()
        .context("failed to load or import the profile")?;

    if actions::fill_form::profile_is_empty(&profile) {
        let bin_path = crate::profile::Profile::path()?;
        let toml_path = bin_path.with_file_name("profile.toml");
        return Ok(serde_json::json!({
            "empty_profile": true,
            "toml_path": toml_path.display().to_string(),
            "bin_path": bin_path.display().to_string(),
        }));
    }

    let hwnd = HWND(foreground_hwnd as *mut core::ffi::c_void);
    let snapshot = crate::inputs::uia::snapshot_hwnd(
        hwnd,
        crate::inputs::uia::DEFAULT_MAX_ELEMENTS,
        crate::inputs::uia::DEFAULT_BUDGET,
    )
    .context("failed to read the form's fields")?;
    let candidates = actions::fill_form::fillable_candidates(&snapshot.fields);

    let (mut mapped, unmapped) = actions::fill_form::map_candidates_locally(&candidates, &profile);

    if !unmapped.is_empty() {
        let ollama_ready = mode == Mode::Auto
            && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
            && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
        let chain = providers.build_chain_for_mode(mode, ollama_ready);

        let req = actions::fill_form::form_fill_request(shot, &unmapped, &profile);
        let responses = chain.complete_parsed_with_fallback(
            &req,
            || non_vision_inputs(raw, foreground_hwnd),
            |c| actions::fill_form::parse_model_response(&c.text),
        )?;
        mapped.extend(actions::fill_form::merge_model_response(
            &unmapped, &responses, &profile,
        ));
    }

    Ok(actions::fill_form::build_proposal(
        &candidates,
        &mapped,
        require_tick_for,
    ))
}

/// #24: the intent router's worker-thread body, called from
/// `App::maybe_start_router`'s spawned thread. Mirrors [`worker`]'s
/// mode-aware chain construction (same Auto-mode Ollama probe, same
/// "network I/O never happens on the main thread" reasoning), but picks the
/// router's own cheapest ready provider/model
/// ([`router::cheapest_router_target`]) instead of the user's configured
/// chain, and runs it through a single-provider [`Chain`] -- issue #99's
/// one-shot repair pass still applies (a schema-invalid response gets one
/// repair attempt), but there is no multi-provider fallback: the router
/// only ever tries the one cheapest ready provider, and if that fails, this
/// press simply gets no suggestion.
fn router_worker(
    providers: &Providers,
    mode: Mode,
    image_png: Vec<u8>,
    candidates: &[router::RouterCandidate],
) -> Result<router::RouterResult> {
    let ollama_ready = mode == Mode::Auto
        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
        && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
    let ready: Vec<String> = providers
        .build_chain_for_mode(mode, ollama_ready)
        .ready_provider_names()
        .into_iter()
        .map(|s| s.to_string())
        .collect();

    let target = router::cheapest_router_target(&ready, |name| router_models_for(providers, name))
        .ok_or_else(|| anyhow::anyhow!("router: no ready provider"))?;
    let provider = provider_for_router(providers, &target.provider, &target.model)
        .ok_or_else(|| anyhow::anyhow!("router: unrecognized provider \"{}\"", target.provider))?;

    let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    let req = router::build_request(image_png, candidates);
    Chain::new(vec![provider]).complete_parsed(&req, |c| {
        router::parse_router_result(&c.text, &candidate_ids)
    })
}

/// #24: the router's per-provider model LIST (never the user's configured
/// "active" model alone) -- `router::cheapest_router_target` picks the
/// cheapest entry from whichever of these it's handed. Issue #222: a
/// one-line call into [`Providers::models_for`] (`config.rs`), which used
/// to be hand-mirrored here -- see that function's doc for why keeping
/// exactly one copy of this name-to-model-list mapping matters.
fn router_models_for(providers: &Providers, name: &str) -> Vec<String> {
    providers.models_for(name)
}

/// #24: constructs the ONE provider the router will call, built with
/// `model` (`router::cheapest_router_target`'s pick) instead of that
/// provider's configured "active" model. Issue #222: a one-line call into
/// [`Providers::provider_for_named_model`] (`config.rs`) with
/// `Some(model)`, which used to be a hand-mirrored match with the same five
/// arms here -- see that function's doc for the drift this closed off.
fn provider_for_router(
    providers: &Providers,
    name: &str,
    model: &str,
) -> Option<Box<dyn Provider>> {
    providers.provider_for_named_model(name, Some(model))
}

/// #39: today's local date and current local UTC offset, for the "Add
/// event from screen" prompt -- DST-correct the same way
/// `deadline_until_tomorrow` already is for Pause, via the identical
/// `TzSpecificLocalTimeToSystemTime(None, ...)` call (the `None` zone asks
/// for the machine's own currently active zone). Win32, so checked by
/// hand (rule 8): press "Add event from screen" and confirm the preview's
/// `start` field lands on the expected calendar day, per issue #166.
fn local_today_and_utc_offset() -> Result<(CivilDate, i32)> {
    let local_now = unsafe { GetLocalTime() };
    let mut utc_now = SYSTEMTIME::default();
    unsafe { TzSpecificLocalTimeToSystemTime(None, &local_now, &mut utc_now) }
        .context("TzSpecificLocalTimeToSystemTime failed")?;

    let today = CivilDate {
        year: local_now.wYear as i32,
        month: local_now.wMonth as u8,
        day: local_now.wDay as u8,
    };
    let local_dt = CivilDateTime {
        date: today,
        hour: local_now.wHour as u8,
        minute: local_now.wMinute as u8,
        second: local_now.wSecond as u8,
    };
    let utc_dt = CivilDateTime {
        date: CivilDate {
            year: utc_now.wYear as i32,
            month: utc_now.wMonth as u8,
            day: utc_now.wDay as u8,
        },
        hour: utc_now.wHour as u8,
        minute: utc_now.wMinute as u8,
        second: utc_now.wSecond as u8,
    };
    let offset_minutes = actions::calendar::utc_offset_minutes(&local_dt, &utc_dt);
    Ok((today, offset_minutes))
}

/// Issue #18/#206: computes the [`crate::provider::NonVisionInputs`] that
/// stand in for the screenshot when a provider in the chain has no vision --
/// OCR text of the captured screen (`raw`) plus a compact UIA field
/// snapshot of the foreground window at press time (`foreground_hwnd`,
/// captured on the main thread in `App::ask`, before the card is shown --
/// see that call site). Passed to [`crate::provider::Chain::complete_parsed_with_fallback`]
/// as its lazy `fallback` closure, so this only ever runs when some
/// provider in the chain actually needs it.
///
/// OCR runs on THIS thread (already a dedicated worker thread spawned by
/// `App::ask`, never the hook thread -- `ocr::recognize`'s own doc comment
/// requires that). The UIA snapshot deliberately runs on a SEPARATE, freshly
/// spawned thread rather than here: `ocr::recognize` initializes a
/// multithreaded WinRT apartment (`RO_INIT_MULTITHREADED`) on the calling
/// thread, and `uia::snapshot_hwnd` initializes an apartment-threaded COM
/// apartment (`COINIT_APARTMENTTHREADED`) on ITS calling thread -- the same
/// OS thread cannot hold both concurrency models at once (a second
/// `CoInitializeEx` call with a different model fails with
/// `RPC_E_CHANGED_MODE`), so OCR and the UIA walk must never run on the same
/// thread. Spawning a dedicated thread per press for the UIA half is cheap
/// next to the OCR/network cost already paid on this path.
///
/// An OCR failure (no language pack installed, the engine unavailable, a
/// timeout) fails this whole function -- CLAUDE.md rule 7 wants that
/// surfaced as a clear, named skip reason for every provider that needed it
/// (see `Chain::complete_parsed_with_fallback`'s doc comment), not silently
/// degraded. A UIA failure (no foreground window, a hung app UIA can't
/// reach, a COM error) degrades instead to an empty field list: OCR text
/// alone is still a useful fallback, and UIA failing is a far more ordinary
/// event than OCR being unavailable, so it must not sink an otherwise-usable
/// OCR result.
fn non_vision_inputs(
    raw: &capture::RawShot,
    foreground_hwnd: isize,
) -> Result<crate::provider::NonVisionInputs> {
    let ocr_output = crate::ocr::recognize(
        &raw.rgba,
        raw.width,
        raw.height,
        crate::ocr::DEFAULT_TIMEOUT,
    )
    .context("OCR unavailable")?;
    let ocr_text = crate::ocr::serialize_lines(&ocr_output.lines);

    let uia_fields = std::thread::spawn(move || {
        let hwnd = HWND(foreground_hwnd as *mut _);
        crate::inputs::uia::snapshot_hwnd(
            hwnd,
            crate::inputs::uia::DEFAULT_MAX_ELEMENTS,
            crate::inputs::uia::DEFAULT_BUDGET,
        )
    })
    .join()
    .ok()
    .and_then(|r| r.ok())
    .map(|snapshot| crate::inputs::uia::format_compact(&snapshot.fields))
    .unwrap_or_default();

    Ok(crate::provider::NonVisionInputs {
        ocr_text,
        uia_fields,
    })
}

/// #23: the precedence rule from the design spec's "Origin tracking"
/// section, pulled out of `worker` as a small pure function so it is
/// unit-testable without a chain, a config, or any Win32/network
/// dependency -- `worker` itself can only be exercised end to end (it
/// builds a real provider chain and makes network calls), so this is the
/// one place the resolution logic gets a direct observable.
///
/// - The prompt: an `actions.toml` override (`Origin::User`) wins outright;
///   otherwise `ui_prompt` (today's only source, `config.ui.prompt`)
///   applies unchanged.
/// - The difficulty flag: `ui_show_difficulty` (the existing global
///   override, #197 part 1) and the resolved action's own
///   `rate_difficulty` (#197 part 2) OR together -- either one being on is
///   enough.
fn resolve_prompt_and_difficulty<'a>(
    default: &'a actions::Resolved,
    ui_prompt: &'a str,
    ui_show_difficulty: bool,
) -> (&'a str, bool) {
    let prompt = match default.origin {
        actions::Origin::User => default.action.prompt.as_str(),
        actions::Origin::Builtin => ui_prompt,
    };
    let want_difficulty = ui_show_difficulty || default.action.rate_difficulty;
    (prompt, want_difficulty)
}

/// Issue #181: which action a pause-toggle-chord press should take. Pure
/// (just a bool in, an enum out) so the toggle direction is unit-tested
/// directly (CLAUDE.md rule 8) without a real `App` -- `App::toggle_pause`
/// is the thin Win32-touching wrapper (checked by hand: press the
/// configured chord while running, confirm the tray greys and the card
/// shows "Paused"; press it again, confirm it un-greys, per issue #166).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PauseToggleAction {
    Resume,
    PauseUntilResumed,
}

/// `paused` is `self.pause.is_paused(now)` -- true for any [`PauseState::Paused`]
/// whose deadline (if any) has not passed. A single keypress can only ever
/// express one pause duration (there is no way to choose 1h vs. until-tomorrow
/// from a bare hotkey press), so [`PauseChoice::UntilResumed`] is it; the
/// tray's `Pause ▸` submenu remains the only way to reach the other two.
fn pause_toggle_action(paused: bool) -> PauseToggleAction {
    if paused {
        PauseToggleAction::Resume
    } else {
        PauseToggleAction::PauseUntilResumed
    }
}

/// FILETIME's epoch (1601-01-01 UTC) precedes the Unix epoch (1970-01-01
/// UTC) by this many seconds -- the well-known constant for converting
/// between the two.
const FILETIME_TO_UNIX_EPOCH_SECS: i64 = 11_644_473_600;

/// Compute the deadline for [`PauseChoice::UntilTomorrow`] (issue #20):
/// today's local date, plus one day ([`pause::next_day`], the pure part),
/// at local midnight, converted to a wall-clock instant via Win32's
/// timezone-aware `TzSpecificLocalTimeToSystemTime` -- which is what makes
/// this DST-correct (a fixed 24h offset would land an hour off across a
/// spring-forward or fall-back transition). Chosen as local midnight rather
/// than a fixed hour like 06:00 because it is the least surprising reading
/// of "until tomorrow" and needs no extra config.
fn deadline_until_tomorrow() -> Result<SystemTime> {
    let local_now = unsafe { GetLocalTime() };
    let today = pause::LocalDate {
        year: local_now.wYear as i32,
        month: local_now.wMonth as u8,
        day: local_now.wDay as u8,
    };
    let tomorrow = pause::next_day(today);

    let local_midnight = SYSTEMTIME {
        wYear: tomorrow.year as u16,
        wMonth: tomorrow.month as u16,
        wDay: tomorrow.day as u16,
        wHour: 0,
        wMinute: 0,
        wSecond: 0,
        wMilliseconds: 0,
        wDayOfWeek: 0,
    };
    let mut utc = SYSTEMTIME::default();
    unsafe { TzSpecificLocalTimeToSystemTime(None, &local_midnight, &mut utc) }
        .context("TzSpecificLocalTimeToSystemTime failed")?;
    systemtime_utc_to_std(&utc)
}

/// The local hour/minute of `t`, for the "Paused until HH:MM" tooltip.
/// Best-effort (`None` on any conversion failure): the tooltip degrades to
/// plain "Paused" rather than that being treated as a rule-7 failure --
/// pausing itself is unaffected either way, only the tooltip's wording.
fn local_hour_min(t: SystemTime) -> Option<(u8, u8)> {
    let utc = std_to_systemtime_utc(t).ok()?;
    let mut local = SYSTEMTIME::default();
    unsafe { SystemTimeToTzSpecificLocalTime(None, &utc, &mut local) }.ok()?;
    Some((local.wHour as u8, local.wMinute as u8))
}

/// Re-arm (kill then re-`SetTimer`) the one-shot pause-expiry timer for the
/// delay remaining between `now` and `until`, clamped to at least 1ms and to
/// `SetTimer`'s `u32` millisecond range (comfortably wide enough for both
/// "1 hour" and "until tomorrow").
fn arm_pause_timer(hwnd: HWND, until: SystemTime, now: SystemTime) {
    let delay = until
        .duration_since(now)
        .unwrap_or(Duration::from_millis(1));
    let delay_ms = delay.as_millis().clamp(1, u32::MAX as u128) as u32;
    unsafe {
        let _ = KillTimer(Some(hwnd), PAUSE_TIMER_ID);
        SetTimer(Some(hwnd), PAUSE_TIMER_ID, delay_ms, None);
    }
}

/// Convert a UTC [`SYSTEMTIME`] to [`SystemTime`] via `SystemTimeToFileTime`
/// plus pure epoch arithmetic (FILETIME's 100ns ticks since 1601 -> a
/// [`Duration`] since the Unix epoch).
fn systemtime_utc_to_std(utc: &SYSTEMTIME) -> Result<SystemTime> {
    let mut ft = FILETIME::default();
    unsafe { SystemTimeToFileTime(utc, &mut ft) }.context("SystemTimeToFileTime failed")?;
    let ticks = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    let unix_100ns = ticks.saturating_sub((FILETIME_TO_UNIX_EPOCH_SECS as u64) * 10_000_000);
    Ok(UNIX_EPOCH + Duration::from_nanos(unix_100ns * 100))
}

/// The inverse of [`systemtime_utc_to_std`]: [`SystemTime`] -> FILETIME
/// ticks (pure arithmetic) -> UTC [`SYSTEMTIME`] via `FileTimeToSystemTime`.
/// Assumes `t` is on or after the Unix epoch, true for every deadline this
/// module ever builds (never before 1970).
fn std_to_systemtime_utc(t: SystemTime) -> Result<SYSTEMTIME> {
    let dur = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let unix_100ns = dur.as_secs() as i64 * 10_000_000 + (dur.subsec_nanos() / 100) as i64;
    let ticks = unix_100ns + FILETIME_TO_UNIX_EPOCH_SECS * 10_000_000;
    let ft = FILETIME {
        dwLowDateTime: (ticks as u64 & 0xFFFF_FFFF) as u32,
        dwHighDateTime: ((ticks as u64) >> 32) as u32,
    };
    let mut st = SYSTEMTIME::default();
    unsafe { FileTimeToSystemTime(&ft, &mut st) }.context("FileTimeToSystemTime failed")?;
    Ok(st)
}

/// First line of an error, truncated on a char boundary, for the headline.
/// Issue #178: the single card slot's final content after `open_settings`'s
/// whole post-modal sequence, as a pure model (no `Card`, no `Tray`, no
/// `HWND`) -- see that method's doc comment for the real, imperative
/// version this mirrors. Each variant is one of the cards `open_settings`
/// can show; `TrayRestoreError` is what proves the fix: it is the outcome
/// whenever `tray_restore_failed` is true, regardless of every other input,
/// because `open_settings` always shows that card last (every one of its
/// branches ends with `self.show_tray_restore_error(tray_restore_error)`).
/// Test-only (`#[cfg(test)]`, same pattern `secrets.rs`'s `InMemoryStore`
/// uses for a test-only type living outside `mod tests`): this function is
/// a verification model of the invariant, not itself called by
/// `open_settings` -- the real enforcement is the unconditional final call
/// on every path, checked directly in that method's source.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsFinalCard {
    /// Cancelled/closed with nothing pending: no card shown at all.
    None,
    /// A worker answer that arrived mid-edit, shown via `on_result` (its own
    /// success/error split does not matter for this ordering, only that
    /// SOME card was shown).
    PendingAnswer,
    /// `Config::save` failed.
    SaveError,
    /// Saved successfully with nothing pending.
    SettingsSaved,
    /// The tray-icon restore failed. Always wins when `tray_restore_failed`
    /// is true -- see this enum's doc comment.
    TrayRestoreError,
}

/// Pure re-derivation of which card is left on screen after
/// `open_settings`'s branches, in the exact order that method executes
/// them. `edited` is whether Settings was saved (`Some`) vs.
/// cancelled/closed (`None`); `save_ok` is only meaningful when `edited` is
/// true.
#[cfg(test)]
fn final_settings_card(
    edited: bool,
    save_ok: bool,
    pending_present: bool,
    tray_restore_failed: bool,
) -> SettingsFinalCard {
    let without_tray_restore = if !edited {
        if pending_present {
            SettingsFinalCard::PendingAnswer
        } else {
            SettingsFinalCard::None
        }
    } else if !save_ok {
        SettingsFinalCard::SaveError
    } else if pending_present {
        SettingsFinalCard::PendingAnswer
    } else {
        SettingsFinalCard::SettingsSaved
    };

    if tray_restore_failed {
        SettingsFinalCard::TrayRestoreError
    } else {
        without_tray_restore
    }
}

fn first_line(text: &str, max: usize) -> String {
    let line = text.lines().next().unwrap_or(text).trim();
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

thread_local! {
    static OWNER_HWND: std::cell::Cell<isize> = const { std::cell::Cell::new(0) };

    /// True for the whole duration `App::open_settings` is suspended inside
    /// `settings::show_modal`'s own message loop (issue #152).
    ///
    /// This -- and every other piece of state a reentrant `wnd_proc` call
    /// needs while it is true -- deliberately lives outside `App`, behind a
    /// thread-local `Cell`/`RefCell`, not as a field on `App` guarded by a
    /// bool. The earlier version of this fix used an `App` field, which is
    /// unsound: `open_settings(&mut self)` holds a unique `&mut self` across
    /// the entire `show_modal` call. Rust's aliasing model (and the LLVM
    /// `noalias` it lowers to) licenses the compiler to assume nothing
    /// reachable from that `&mut self` is read or written by an opaque call
    /// that is never handed `self` -- `show_modal` is exactly that, since
    /// the reentrant path reaches `App` only through the raw `GWLP_USERDATA`
    /// pointer, a completely different provenance. Concretely: the store to
    /// an `App` field made just before the call and the store made just
    /// after it are, as far as the optimizer can see, two writes to the same
    /// location with nothing in between that reads or aliases it -- a
    /// textbook dead-store-elimination candidate, which in release/LTO could
    /// make the guard silently never take effect. A `Cell` reached only
    /// through its own thread-local accessor (never through `&App`/`&mut
    /// App`) has no aliasing relationship with `&mut self` at all, so none
    /// of this applies: both the read in `wnd_proc` and the writes in
    /// `open_settings` are plain, unoptimizable-away side effects through an
    /// opaque TLS accessor call.
    static SETTINGS_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// Every `WM_APP_RESULT`/`WM_APP_CALENDAR_RESULT`/`WM_APP_REVIEW_RESULT`/
    /// `WM_APP_PREVIEW_DECIDED` message that arrived while `SETTINGS_OPEN`
    /// was true, oldest first. `open_settings` drains and delivers all of
    /// them, in this same arrival order, once `show_modal` returns, so an
    /// in-flight request or a preview's "Do it"/Cancel decision still ends
    /// in a card (rule 7) -- and, for a confirmed preview, still actually
    /// runs its executor -- instead of being dropped while Settings
    /// happened to be open (issue #213, generalizing the single-slot fix
    /// #152 already built for `WM_APP_RESULT` alone). See `SETTINGS_OPEN`
    /// for why this can't be a field on `App`.
    static PENDING_MESSAGES: std::cell::RefCell<VecDeque<DeferredMessage>> =
        const { std::cell::RefCell::new(VecDeque::new()) };

    /// The shell's `TaskbarCreated` broadcast arrived while `SETTINGS_OPEN`
    /// was true. `open_settings` re-adds the tray icon (`on_taskbar_created`)
    /// once `show_modal` returns, rather than never doing so.
    static TASKBAR_RECREATED_WHILE_SETTINGS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };

    /// The runtime id of the shell's `TaskbarCreated` message, mirrored from
    /// `App::taskbar_created_msg` at startup (see that field's doc comment).
    /// `wnd_proc` needs it to recognise the broadcast *before* `&mut App`
    /// may be formed at all, i.e. before `App` -- and its own copy of this
    /// id -- can be reached. `0` (never a value `RegisterWindowMessageW`
    /// returns) means "not set yet".
    static TASKBAR_CREATED_MSG: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };

    /// A pause-relevant event (the `PAUSE_TIMER_ID` timer firing, a wake
    /// from sleep, or a clock change) arrived while `SETTINGS_OPEN` was
    /// true. `open_settings` re-evaluates the pause deadline
    /// (`App::reevaluate_pause`) once `show_modal` returns, the same way
    /// `TASKBAR_RECREATED_WHILE_SETTINGS` defers the taskbar re-add. See
    /// `SETTINGS_OPEN` for why this can't be a field on `App`.
    ///
    /// The `WM_TIMER` case specifically must still be killed immediately,
    /// not merely deferred -- see the `wnd_proc` guard that sets this flag.
    static PAUSE_REEVALUATE_PENDING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

use std::os::windows::ffi::OsStrExt;

/// One message deferred by [`SettingsReentrancy::Defer`] while Settings was
/// open (issue #213), holding whatever payload its `WM_APP_*` counterpart
/// boxed into `lparam` -- already taken out of that box, so `PENDING_MESSAGES`
/// never stores a raw pointer.
#[derive(Debug)]
enum DeferredMessage {
    /// `WM_APP_RESULT`.
    Result(std::result::Result<Answer, String>),
    /// `WM_APP_CALENDAR_RESULT`.
    CalendarResult(std::result::Result<Value, String>),
    /// `WM_APP_REVIEW_RESULT`.
    ReviewResult(std::result::Result<actions::review_email::ReviewOutcome, String>),
    /// `WM_APP_FORM_FILL_RESULT` (#40, joining the Defer group in #213's
    /// style rather than the plain free-and-drop `WM_APP_ROUTER_RESULT`
    /// still gets).
    FormFillResult(std::result::Result<Value, String>),
    /// `WM_APP_PREVIEW_DECIDED` carries no payload of its own -- the
    /// decision lives on `Card`/`App` (`take_confirmed`/`pending_review`),
    /// read fresh when this is finally delivered by `on_preview_decided`.
    PreviewDecided,
}

/// What `wnd_proc` should do with a message addressed to the owner window
/// while `SETTINGS_OPEN` is true (issue #152): every arm here must be
/// answerable without forming `&mut App` -- see that thread-local's doc
/// comment for why. Extracted as pure logic (no `HWND`, no thread-local, no
/// `App`) so the policy is unit-tested directly rather than only exercised
/// by clicking through a live modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SettingsReentrancy {
    /// Swallow it: return `LRESULT(0)` without touching `App`. Covers the
    /// hotkey, a second Copilot-key launch, the tray callback itself (so its
    /// menu does not pop up, and none of its commands can fire, over the
    /// modal), a dismiss click (the card is already hidden and its watcher
    /// disarmed before `show_modal` runs, so there is nothing to do),
    /// `WM_APP_LEARNED` (unreachable here in practice: every path that opens
    /// Settings cancels learn mode first, see `open_settings`'s call sites --
    /// but the boxed `Chord` payload is still freed rather than leaked, in
    /// case that invariant ever changes), `WM_APP_PAUSE_TOGGLE` (#181: same
    /// treatment as the hotkey -- a chord press while Settings is open is
    /// dropped, not queued), and the two palette messages (#25: the palette
    /// cannot be shown while Settings is modal-open anyway; `WM_APP_PALETTE_RUN`'s
    /// boxed `String` payload is freed rather than leaked, same as
    /// `WM_APP_LEARNED`'s), and `WM_APP_ROUTER_RESULT` (#24: the palette a
    /// router suggestion belongs to cannot be visible while Settings is
    /// modal-open either, so there is nothing left to apply it to -- a
    /// router suggestion is advisory, unlike a finished action's own
    /// result, so rule 7 does not require it to survive Settings the way
    /// `Defer`'s ids must; its boxed `(u64, Result<...>)` payload is freed
    /// rather than leaked, same as `WM_APP_LEARNED`'s).
    Ignore,
    /// Push the message's payload onto `PENDING_MESSAGES`, oldest-last;
    /// `open_settings` drains and delivers every one of them, in arrival
    /// order, after `show_modal` returns, so a result that finished (or a
    /// preview decided) mid-edit still ends in a card (rule 7) -- and, for
    /// a confirmed preview, still actually runs its executor -- instead of
    /// being silently overwritten or dropped. Issue #213: generalizes the
    /// single-slot `WM_APP_RESULT`-only fix #152 built to every `WM_APP_*`
    /// result/decision message in the crate (`WM_APP_RESULT`,
    /// `WM_APP_CALENDAR_RESULT`, `WM_APP_REVIEW_RESULT`,
    /// `WM_APP_PREVIEW_DECIDED`); #40's `WM_APP_FORM_FILL_RESULT` joins the
    /// same group for the same reason -- a finished "Fill this form" run
    /// must not silently lose its card just because Settings happened to be
    /// open -- add any FUTURE one here too, never to `Ignore`, and to
    /// `DeferredMessage`/the `Defer` arm in `wnd_proc`.
    Defer,
    /// Set `TASKBAR_RECREATED_WHILE_SETTINGS`; `open_settings` re-adds the
    /// tray icon after `show_modal` returns.
    DeferTaskbarCreated,
    /// Not one of ours: hand it to `DefWindowProcW`, same as the normal
    /// unmatched-message path, without ever forming `&mut App`. In practice
    /// the only message this could plausibly be is `WM_DESTROY`, and that is
    /// itself unreachable while Settings is open: its only path is the
    /// tray's Quit command, and `WM_APP_TRAY` is `Ignore`d above.
    Fallback,
}

fn settings_reentrancy_policy(msg: u32, taskbar_created_msg: u32) -> SettingsReentrancy {
    if taskbar_created_msg != 0 && msg == taskbar_created_msg {
        return SettingsReentrancy::DeferTaskbarCreated;
    }
    match msg {
        // Issue #213: every WM_APP_* message that carries a worker's result
        // or a preview's Do it/Cancel decision is deferred, never Ignored --
        // see DeferredMessage and the Defer variant's own doc comment. #40:
        // WM_APP_FORM_FILL_RESULT joins the same group for the same reason
        // -- its boxed `Result<Value, String>` payload is taken (not freed)
        // in the Defer arm below.
        WM_APP_RESULT
        | WM_APP_CALENDAR_RESULT
        | WM_APP_REVIEW_RESULT
        | WM_APP_PREVIEW_DECIDED
        | WM_APP_FORM_FILL_RESULT => SettingsReentrancy::Defer,
        WM_APP_HOTKEY
        | WM_APP_ACTIVATE
        | WM_APP_TRAY
        | WM_APP_DISMISS
        | WM_APP_LEARNED
        | WM_APP_PAUSE_TOGGLE
        // #25: the palette cannot be shown while Settings is modal-open
        // anyway (Settings takes the foreground; the hook's own chord check
        // still passes the keydown through per the Ignore branch above), so
        // both palette messages are simply dropped here, same treatment
        // WM_APP_PAUSE_TOGGLE already gets. WM_APP_PALETTE_RUN's boxed
        // `String` payload is freed explicitly below (mirroring
        // WM_APP_LEARNED's) so it never leaks.
        // #24: WM_APP_ROUTER_RESULT is dropped the same way -- a router
        // suggestion is advisory, and the palette it belongs to cannot be
        // visible while Settings is modal-open either, so there is nothing
        // left to apply it to; its boxed `(u64, Result<...>)` payload is
        // freed explicitly below too.
        | WM_APP_PALETTE_TOGGLE
        | WM_APP_PALETTE_RUN
        | WM_APP_ROUTER_RESULT => SettingsReentrancy::Ignore,
        _ => SettingsReentrancy::Fallback,
    }
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        OWNER_HWND.with(|h| h.set(hwnd.0 as isize));
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // Issue #152: apply the reentrancy policy BEFORE forming `&mut App` --
    // not merely before using it -- and using only thread-local state to do
    // so. See `SETTINGS_OPEN`'s doc comment for why even reading `app.*`
    // here, guard or no guard, would be unsound while it is true.
    if SETTINGS_OPEN.with(|c| c.get()) {
        // Issue #20: these three need `hwnd` (to kill the pause timer) and
        // so can't be folded into `settings_reentrancy_policy`'s pure,
        // `hwnd`-free signature the way the other cases are -- handled here
        // instead, before that policy even runs. WM_TIMER specifically must
        // be killed immediately, not merely deferred: SetTimer without a
        // TIMERPROC keeps re-posting at the same interval until KillTimer
        // is called, so leaving it unhandled behind the modal would turn it
        // into an actual polling timer (rule 5), not just a late check.
        if msg == WM_TIMER && wparam.0 == PAUSE_TIMER_ID {
            unsafe {
                let _ = KillTimer(Some(hwnd), PAUSE_TIMER_ID);
            }
            PAUSE_REEVALUATE_PENDING.with(|c| c.set(true));
            return LRESULT(0);
        }
        if msg == WM_POWERBROADCAST {
            if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC {
                PAUSE_REEVALUATE_PENDING.with(|c| c.set(true));
            }
            return LRESULT(1);
        }
        if msg == WM_TIMECHANGE {
            PAUSE_REEVALUATE_PENDING.with(|c| c.set(true));
            return LRESULT(0);
        }

        let taskbar_created_msg = TASKBAR_CREATED_MSG.with(|c| c.get());
        match settings_reentrancy_policy(msg, taskbar_created_msg) {
            SettingsReentrancy::Ignore => {
                // WM_APP_LEARNED, WM_APP_PALETTE_RUN and WM_APP_ROUTER_RESULT
                // are the only ignored messages carrying a boxed payload;
                // free them so none leaks. (WM_APP_CALENDAR_RESULT,
                // WM_APP_REVIEW_RESULT and WM_APP_FORM_FILL_RESULT are all
                // Defer, not Ignore -- issue #213/#40.)
                if msg == WM_APP_LEARNED {
                    drop(unsafe { Box::from_raw(lparam.0 as *mut Chord) });
                } else if msg == WM_APP_PALETTE_RUN {
                    drop(unsafe { Box::from_raw(lparam.0 as *mut String) });
                } else if msg == WM_APP_ROUTER_RESULT {
                    drop(unsafe {
                        Box::from_raw(
                            lparam.0
                                as *mut (u64, std::result::Result<router::RouterResult, String>),
                        )
                    });
                }
                return LRESULT(0);
            }
            SettingsReentrancy::Defer => {
                // Issue #213: take ownership of whatever payload this id
                // carries (none, for WM_APP_PREVIEW_DECIDED) and queue it;
                // `open_settings` delivers every queued message, in this
                // same push order, once `show_modal` returns.
                let deferred = match msg {
                    WM_APP_RESULT => DeferredMessage::Result(unsafe {
                        *Box::from_raw(lparam.0 as *mut std::result::Result<Answer, String>)
                    }),
                    WM_APP_CALENDAR_RESULT => DeferredMessage::CalendarResult(unsafe {
                        *Box::from_raw(lparam.0 as *mut std::result::Result<Value, String>)
                    }),
                    WM_APP_REVIEW_RESULT => DeferredMessage::ReviewResult(unsafe {
                        *Box::from_raw(
                            lparam.0
                                as *mut std::result::Result<
                                    actions::review_email::ReviewOutcome,
                                    String,
                                >,
                        )
                    }),
                    // #40: same treatment as WM_APP_CALENDAR_RESULT just
                    // above -- a finished "Fill this form" run must not
                    // silently lose its card just because Settings happened
                    // to be open.
                    WM_APP_FORM_FILL_RESULT => DeferredMessage::FormFillResult(unsafe {
                        *Box::from_raw(lparam.0 as *mut std::result::Result<Value, String>)
                    }),
                    WM_APP_PREVIEW_DECIDED => DeferredMessage::PreviewDecided,
                    _ => unreachable!(
                        "settings_reentrancy_policy only returns Defer for the five ids above"
                    ),
                };
                PENDING_MESSAGES.with(|c| c.borrow_mut().push_back(deferred));
                return LRESULT(0);
            }
            SettingsReentrancy::DeferTaskbarCreated => {
                TASKBAR_RECREATED_WHILE_SETTINGS.with(|c| c.set(true));
                return LRESULT(0);
            }
            SettingsReentrancy::Fallback => {
                return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
            }
        }
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    let app: &mut App = unsafe { &mut *ptr };

    match msg {
        WM_APP_TRAY => {
            if let Some(id) = app.tray.on_tray_message(lparam) {
                // Choosing anything other than a rebind disarms a learn mode
                // left armed by an earlier "Set ... key" click; otherwise it
                // would silently swallow the next key pressed anywhere.
                if !matches!(
                    decode(id),
                    MenuChoice::Command(cmd::SET_PRIMARY) | MenuChoice::Command(cmd::SET_SECONDARY)
                ) {
                    if let Some(h) = &app.hook {
                        h.cancel_learning();
                    }
                }
                match decode(id) {
                    MenuChoice::OpenAiModel(i) => app.pick_model(true, i),
                    MenuChoice::AnthropicModel(i) => app.pick_model(false, i),
                    MenuChoice::Command(cmd::ASK_NOW) => app.ask(),
                    MenuChoice::Command(cmd::EXTRACT_TEXT) => app.extract_text(),
                    MenuChoice::Command(cmd::COPY_REGION) => app.copy_region(),
                    MenuChoice::Command(cmd::COPY_LAST) => app.copy_last(),
                    MenuChoice::Command(cmd::ADD_TO_CALENDAR) => app.add_event_from_screen(),
                    MenuChoice::Command(cmd::QUICK_ASK) => app.toggle_palette(),
                    MenuChoice::Command(cmd::REVIEW_EMAIL) => app.review_this_email(),
                    MenuChoice::Command(cmd::FILL_FORM) => app.fill_form_from_screen(),
                    MenuChoice::Command(cmd::RESTORE_LAST_FORM) => app.restore_last_form(),
                    MenuChoice::Command(cmd::SET_PRIMARY) => app.start_learning(HK_PRIMARY),
                    MenuChoice::Command(cmd::SET_SECONDARY) => app.start_learning(HK_SECONDARY),
                    MenuChoice::Command(cmd::EDIT_SETTINGS) => app.edit_settings(),
                    MenuChoice::Command(cmd::RELOAD) => app.reload(),
                    MenuChoice::Command(cmd::COPY_DIAGNOSTICS) => app.copy_diagnostics(),
                    MenuChoice::Command(cmd::CALCULATE_SELECTION) => app.calculate_selection(),
                    MenuChoice::Command(cmd::USE_OPENAI) => app.set_provider(true),
                    MenuChoice::Command(cmd::USE_ANTHROPIC) => app.set_provider(false),
                    MenuChoice::Command(cmd::PAUSE_1H) => app.pause_for(PauseChoice::OneHour),
                    MenuChoice::Command(cmd::PAUSE_UNTIL_TOMORROW) => {
                        app.pause_for(PauseChoice::UntilTomorrow)
                    }
                    MenuChoice::Command(cmd::PAUSE_UNTIL_RESUMED) => {
                        app.pause_for(PauseChoice::UntilResumed)
                    }
                    MenuChoice::Command(cmd::RESUME) => app.resume(),
                    MenuChoice::Command(cmd::MODE_CLOUD) => app.set_mode(Mode::Cloud),
                    MenuChoice::Command(cmd::MODE_LOCAL) => app.set_mode(Mode::Local),
                    MenuChoice::Command(cmd::MODE_AUTO) => app.set_mode(Mode::Auto),
                    MenuChoice::Command(cmd::MODE_OFFLINE) => app.set_mode(Mode::Offline),
                    MenuChoice::Command(cmd::OPEN_SETTINGS) => app.open_settings(),
                    MenuChoice::Command(cmd::QUIT) => unsafe {
                        let _ = DestroyWindow(hwnd);
                    },
                    _ => {}
                }
            }
            LRESULT(0)
        }
        WM_APP_HOTKEY => {
            let _which = wparam.0; // both bindings do the same thing
            app.ask();
            LRESULT(0)
        }
        WM_APP_ACTIVATE => {
            // `ask` already no-ops while `busy`, so a second Copilot-key press
            // landing here while a request is in flight is harmless.
            app.ask();
            LRESULT(0)
        }
        WM_APP_RESULT => {
            // Take ownership of the box the worker leaked into the message.
            let result =
                unsafe { *Box::from_raw(lparam.0 as *mut std::result::Result<Answer, String>) };
            app.on_result(result);
            LRESULT(0)
        }
        WM_APP_CALENDAR_RESULT => {
            let result =
                unsafe { *Box::from_raw(lparam.0 as *mut std::result::Result<Value, String>) };
            app.on_calendar_result(result);
            LRESULT(0)
        }
        WM_APP_REVIEW_RESULT => {
            let result = unsafe {
                *Box::from_raw(
                    lparam.0
                        as *mut std::result::Result<actions::review_email::ReviewOutcome, String>,
                )
            };
            app.on_review_result(result);
            LRESULT(0)
        }
        WM_APP_FORM_FILL_RESULT => {
            let result =
                unsafe { *Box::from_raw(lparam.0 as *mut std::result::Result<Value, String>) };
            app.on_form_fill_result(result);
            LRESULT(0)
        }
        WM_APP_PREVIEW_DECIDED => {
            app.on_preview_decided();
            LRESULT(0)
        }
        WM_APP_DISMISS => {
            let (x, y) = unpack_point(lparam.0 as u32);
            app.on_global_click(x, y);
            LRESULT(0)
        }
        WM_APP_LEARNED => {
            let chord = unsafe { *Box::from_raw(lparam.0 as *mut Chord) };
            app.on_learned(wparam.0, chord);
            LRESULT(0)
        }
        WM_APP_PAUSE_TOGGLE => {
            // #181: the hook already decided this keydown matches the
            // configured pause chord (`hotkey::pause_hotkey_outcome`); all
            // that is left is which direction to toggle, which only `App`
            // knows (its current `PauseState`, not anything carried on the
            // message).
            app.toggle_pause();
            LRESULT(0)
        }
        WM_APP_PALETTE_TOGGLE => {
            // #25: the hook already decided this keydown matches the
            // configured `hotkeys.palette` chord; `toggle_palette` decides
            // show vs. hide from the palette's own current visibility.
            app.toggle_palette();
            LRESULT(0)
        }
        WM_APP_PALETTE_RUN => {
            // #25: Enter in the palette posted this with the selected
            // action id boxed into lparam (ui::palette's own
            // WM_APP_PALETTE_RUN doc comment). Take ownership, then run it
            // through the same dispatch table the palette's rows were built
            // from.
            let action_id = unsafe { *Box::from_raw(lparam.0 as *mut String) };
            app.dispatch_palette_action(&action_id);
            LRESULT(0)
        }
        WM_APP_ROUTER_RESULT => {
            // #24: take ownership of the (generation, result) box the
            // router's worker thread leaked into the message (this
            // constant's own doc comment).
            let (generation, result) = unsafe {
                *Box::from_raw(
                    lparam.0 as *mut (u64, std::result::Result<router::RouterResult, String>),
                )
            };
            app.on_router_result(generation, result);
            LRESULT(0)
        }
        WM_TIMER => {
            if wparam.0 == PAUSE_TIMER_ID {
                app.on_pause_timer(hwnd);
            }
            LRESULT(0)
        }
        WM_POWERBROADCAST => {
            // Sleep does not stop wall-clock time, but it does stop a
            // SetTimer from running (rule 5's flip side): re-check the
            // pause deadline once against the real clock on wake, per
            // issue #20.
            if wparam.0 as u32 == PBT_APMRESUMEAUTOMATIC {
                app.reevaluate_pause(hwnd);
            }
            // Returning TRUE grants the power-management request; every
            // PBT_* code expects a nonzero return, not just this one.
            LRESULT(1)
        }
        WM_TIMECHANGE => {
            // A manual clock change (or a timezone/DST update) can move the
            // pause deadline without ever posting WM_TIMER -- re-check once.
            app.reevaluate_pause(hwnd);
            LRESULT(0)
        }
        // Not a compile-time constant (RegisterWindowMessageW is a runtime
        // registration), so it cannot be an ordinary match arm -- see
        // ui::tray's module docs on TaskbarCreated.
        id if id == app.taskbar_created_msg => {
            app.on_taskbar_created();
            LRESULT(0)
        }
        WM_DESTROY => {
            // Drop the App (and with it the hook, tray icon and card) before
            // the loop exits, so the tray icon actually disappears.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            drop(unsafe { Box::from_raw(ptr) });
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    use super::first_line;
    use super::resolve_prompt_and_difficulty;
    use super::unreadable_secrets_card;
    use super::App;
    use super::{final_settings_card, SettingsFinalCard};
    use super::{provider_for_router, router_models_for};
    use super::{settings_reentrancy_policy, SettingsReentrancy};
    use super::{
        WM_APP_ACTIVATE, WM_APP_CALENDAR_RESULT, WM_APP_FORM_FILL_RESULT, WM_APP_RESULT,
        WM_APP_REVIEW_RESULT, WM_APP_ROUTER_RESULT,
    };
    use crate::actions::{self, Origin};
    use crate::capture;
    use crate::config::Providers;
    use crate::dismiss::WM_APP_DISMISS;
    use crate::hotkey::{
        WM_APP_HOTKEY, WM_APP_LEARNED, WM_APP_PALETTE_TOGGLE, WM_APP_PAUSE_TOGGLE,
    };
    use crate::mode::Mode;
    use crate::provider::Provider;
    use crate::router;
    use crate::ui::card::WM_APP_PREVIEW_DECIDED;
    use crate::ui::palette::WM_APP_PALETTE_RUN;
    use crate::ui::palette_model;
    use crate::ui::tray::WM_APP_TRAY;
    use windows::Win32::UI::WindowsAndMessaging::WM_DESTROY;

    // -- #23: resolve_prompt_and_difficulty (the worker's action-model wiring) --

    fn resolved(origin: Origin, prompt: &str, rate_difficulty: bool) -> actions::Resolved {
        let mut action = actions::builtin_actions()[0].clone();
        action.prompt = prompt.to_string();
        action.rate_difficulty = rate_difficulty;
        actions::Resolved { action, origin }
    }

    #[test]
    fn builtin_origin_uses_the_config_prompt_unchanged() {
        // No actions.toml override -> today's exact behaviour: whatever
        // config.ui.prompt says, regardless of the built-in's own prompt
        // text.
        let r = resolved(Origin::Builtin, "builtin default text", false);
        let (prompt, _) = resolve_prompt_and_difficulty(&r, "config.toml prompt", false);
        assert_eq!(prompt, "config.toml prompt");
    }

    #[test]
    fn user_origin_overrides_the_config_prompt() {
        // #23's Done-when: a user action overriding a built-in's prompt is
        // what runs.
        let r = resolved(Origin::User, "overridden prompt", false);
        let (prompt, _) = resolve_prompt_and_difficulty(&r, "config.toml prompt", false);
        assert_eq!(prompt, "overridden prompt");
    }

    #[test]
    fn difficulty_flags_or_together() {
        for (ui_flag, action_flag, expected) in [
            (false, false, false),
            (true, false, true),
            (false, true, true),
            (true, true, true),
        ] {
            let r = resolved(Origin::Builtin, "p", action_flag);
            let (_, want_difficulty) = resolve_prompt_and_difficulty(&r, "p", ui_flag);
            assert_eq!(
                want_difficulty, expected,
                "ui_flag={ui_flag} action_flag={action_flag}"
            );
        }
    }

    #[test]
    fn fresh_install_defaults_send_no_difficulty_rubric() {
        // Both #197 part 1 (ui.show_difficulty default false) and #197 part
        // 2 (rate_difficulty default false) must combine to "off", matching
        // today's already-shipped default (commit d88f78b).
        let r = resolved(Origin::Builtin, crate::provider::DEFAULT_PROMPT, false);
        let (prompt, want_difficulty) =
            resolve_prompt_and_difficulty(&r, crate::provider::DEFAULT_PROMPT, false);
        assert_eq!(prompt, crate::provider::DEFAULT_PROMPT);
        assert!(!want_difficulty);
    }

    // -- unreadable_secrets_card (issue #175) ------------------------------

    #[test]
    fn unreadable_secrets_card_names_every_affected_provider() {
        let (headline, detail) =
            unreadable_secrets_card(&["openai".to_string(), "anthropic".to_string()]);
        assert_eq!(headline, "Stored API key unreadable");
        assert!(detail.contains("openai"));
        assert!(detail.contains("anthropic"));
    }

    #[test]
    fn unreadable_secrets_card_never_says_deleted() {
        // Rule 7's whole point here: the key was NOT deleted (#175's fix),
        // so the card must say so, not imply data loss that did not happen.
        let (_, detail) = unreadable_secrets_card(&["openai".to_string()]);
        assert!(
            detail.contains("not"),
            "must not read as a deletion: {detail}"
        );
        assert!(!detail.contains("deleted the"));
    }

    #[test]
    fn unreadable_secrets_card_has_no_em_dash() {
        // CLAUDE.md rule 11: no em dashes in user-facing strings.
        let (headline, detail) = unreadable_secrets_card(&["openai".to_string()]);
        assert!(!headline.contains('\u{2014}'));
        assert!(!detail.contains('\u{2014}'));
    }

    // -- final_settings_card (issue #178) -----------------------------------

    #[test]
    fn tray_restore_error_wins_over_every_other_outcome() {
        // The core invariant: whatever else happened during open_settings,
        // a failed tray-icon restore is always the LAST card shown, never
        // silently replaced. Exhaustive over every other combination.
        for edited in [false, true] {
            for save_ok in [false, true] {
                for pending_present in [false, true] {
                    assert_eq!(
                        final_settings_card(edited, save_ok, pending_present, true),
                        SettingsFinalCard::TrayRestoreError,
                        "edited={edited}, save_ok={save_ok}, pending_present={pending_present}"
                    );
                }
            }
        }
    }

    #[test]
    fn cancel_with_nothing_pending_and_no_tray_failure_shows_no_card() {
        assert_eq!(
            final_settings_card(false, true, false, false),
            SettingsFinalCard::None
        );
    }

    #[test]
    fn cancel_with_a_pending_answer_shows_it() {
        assert_eq!(
            final_settings_card(false, true, true, false),
            SettingsFinalCard::PendingAnswer
        );
    }

    #[test]
    fn save_failure_shows_the_save_error() {
        assert_eq!(
            final_settings_card(true, false, false, false),
            SettingsFinalCard::SaveError
        );
        // Even with an answer pending -- the save error still wins (matches
        // the existing, unchanged behavior: the user just directly caused
        // this one).
        assert_eq!(
            final_settings_card(true, false, true, false),
            SettingsFinalCard::SaveError
        );
    }

    #[test]
    fn successful_save_with_nothing_pending_shows_settings_saved() {
        assert_eq!(
            final_settings_card(true, true, false, false),
            SettingsFinalCard::SettingsSaved
        );
    }

    #[test]
    fn successful_save_with_a_pending_answer_shows_it_not_settings_saved() {
        assert_eq!(
            final_settings_card(true, true, true, false),
            SettingsFinalCard::PendingAnswer
        );
    }

    // -- readiness_gate (issue #192) ---------------------------------------
    //
    // App::ask's pre-flight "nothing configured" gate used to check
    // self.chain, built mode-agnostically by Config::build_chain -- so in
    // e.g. Local mode with only a cloud key, the gate passed (the cloud
    // provider is ready), capture ran, and only the worker's later
    // mode-filtered chain (empty, since Local excludes cloud) failed, with a
    // less specific message. readiness_gate decides from config alone
    // (Providers::build_chain_for_mode with an optimistic ollama_ready=true,
    // matching that function's own doc comment) which card, if any, to show
    // -- no network call, so Auto's real Ollama probe never runs here.

    fn cloud_ready_providers() -> Providers {
        let mut p = Providers {
            order: vec!["openai".to_string(), "anthropic".to_string()],
            ..Providers::default()
        };
        p.openai.api_key = "sk-real".to_string();
        p
    }

    fn ollama_only_providers() -> Providers {
        let mut p = Providers {
            order: vec!["ollama".to_string()],
            ..Providers::default()
        };
        p.ollama.base_url = "http://127.0.0.1:11434".to_string();
        p
    }

    fn nothing_configured_providers() -> Providers {
        Providers {
            order: vec!["openai".to_string(), "anthropic".to_string()],
            ..Providers::default()
        }
    }

    // -- model_action_gate (issue #214) ------------------------------------

    #[test]
    fn model_action_gate_busy_wins_over_everything() {
        for paused in [false, true] {
            assert_eq!(
                super::model_action_gate(true, paused),
                super::ModelActionGate::Busy,
                "busy=true, paused={paused}"
            );
        }
    }

    #[test]
    fn model_action_gate_paused_wins_when_not_busy() {
        assert_eq!(
            super::model_action_gate(false, true),
            super::ModelActionGate::Paused
        );
    }

    #[test]
    fn model_action_gate_proceeds_when_neither_busy_nor_paused() {
        assert_eq!(
            super::model_action_gate(false, false),
            super::ModelActionGate::Proceed
        );
    }

    /// Exhaustive over all four `(busy, paused)` combinations -- the same
    /// "sweep the dimensions the bug lives in" shape as the readiness-gate
    /// matrix below, proving BOTH that busy strictly outranks paused and
    /// that `Proceed` is reached only when neither gate blocks.
    #[test]
    fn model_action_gate_matches_precedence_table_exhaustively() {
        for busy in [false, true] {
            for paused in [false, true] {
                let expected = if busy {
                    super::ModelActionGate::Busy
                } else if paused {
                    super::ModelActionGate::Paused
                } else {
                    super::ModelActionGate::Proceed
                };
                assert_eq!(
                    super::model_action_gate(busy, paused),
                    expected,
                    "busy={busy}, paused={paused}"
                );
            }
        }
    }

    #[test]
    fn readiness_gate_passes_when_a_cloud_key_is_set_in_cloud_mode() {
        assert!(
            App::readiness_gate(Mode::Cloud, &cloud_ready_providers(), "config.toml").is_none()
        );
    }

    #[test]
    fn readiness_gate_blocks_cloud_mode_with_only_ollama_configured() {
        // The exact scenario #192 reports: Cloud mode, only Ollama
        // configured -- Ollama's ready() is true (no key needed), but Cloud
        // mode never selects it, so the gate must still block.
        let (headline, _) =
            App::readiness_gate(Mode::Cloud, &ollama_only_providers(), "config.toml")
                .expect("must block: cloud mode has nothing cloud configured");
        assert_eq!(headline, "No API key: open Edit settings");
    }

    #[test]
    fn readiness_gate_passes_when_ollama_is_configured_in_local_mode() {
        assert!(
            App::readiness_gate(Mode::Local, &ollama_only_providers(), "config.toml").is_none()
        );
    }

    #[test]
    fn readiness_gate_passes_when_ollama_is_configured_in_offline_mode() {
        assert!(
            App::readiness_gate(Mode::Offline, &ollama_only_providers(), "config.toml").is_none()
        );
    }

    #[test]
    fn readiness_gate_blocks_local_mode_with_only_a_cloud_key() {
        // The other #192 scenario: Local mode with only a cloud key
        // configured must say Ollama is what's missing, not the generic
        // "No API key" message meant for Cloud mode.
        let (headline, detail) = App::readiness_gate(
            Mode::Local,
            &cloud_ready_providers(),
            "C:\\cfg\\config.toml",
        )
        .expect("must block: local mode has no ollama configured");
        assert_eq!(headline, "Local mode needs Ollama configured");
        assert!(detail.contains("ollama"), "{detail}");
        assert!(detail.contains("C:\\cfg\\config.toml"), "{detail}");
    }

    #[test]
    fn readiness_gate_blocks_offline_mode_with_only_a_cloud_key() {
        let (headline, _) =
            App::readiness_gate(Mode::Offline, &cloud_ready_providers(), "config.toml")
                .expect("must block: offline mode has no ollama configured");
        assert_eq!(headline, "Local mode needs Ollama configured");
    }

    #[test]
    fn readiness_gate_passes_in_auto_mode_when_only_ollama_is_configured() {
        // Auto mode's optimistic ollama_ready=true upper bound must count
        // Ollama as a candidate even though no real probe ran here.
        assert!(App::readiness_gate(Mode::Auto, &ollama_only_providers(), "config.toml").is_none());
    }

    #[test]
    fn readiness_gate_passes_in_auto_mode_when_only_a_cloud_key_is_configured() {
        assert!(App::readiness_gate(Mode::Auto, &cloud_ready_providers(), "config.toml").is_none());
    }

    #[test]
    fn readiness_gate_blocks_auto_mode_with_nothing_configured() {
        let (headline, _) =
            App::readiness_gate(Mode::Auto, &nothing_configured_providers(), "config.toml")
                .expect("must block: nothing is configured at all");
        assert_eq!(headline, "No provider ready: open Edit settings");
    }

    #[test]
    fn readiness_gate_blocks_cloud_mode_with_nothing_configured() {
        assert!(
            App::readiness_gate(Mode::Cloud, &nothing_configured_providers(), "config.toml")
                .is_some()
        );
    }

    #[test]
    fn readiness_gate_blocks_local_mode_with_nothing_configured() {
        assert!(
            App::readiness_gate(Mode::Local, &nothing_configured_providers(), "config.toml")
                .is_some()
        );
    }

    #[test]
    fn readiness_gate_cards_have_no_em_dash() {
        // CLAUDE.md rule 11.
        for (mode, providers) in [
            (Mode::Cloud, nothing_configured_providers()),
            (Mode::Local, nothing_configured_providers()),
            (Mode::Auto, nothing_configured_providers()),
        ] {
            let (headline, detail) =
                App::readiness_gate(mode, &providers, "config.toml").expect("must block");
            assert!(!headline.contains('\u{2014}'), "{headline}");
            assert!(!detail.contains('\u{2014}'), "{detail}");
        }
    }

    // -- should_open_config_after_ensuring_it_exists (issue #174) ---------

    #[test]
    fn opens_when_the_file_already_existed_and_no_save_was_needed() {
        // Whatever the placeholder Ok(()) carries is irrelevant here -- the
        // file already existed, so no save was attempted at all.
        assert!(App::should_open_config_after_ensuring_it_exists(
            false,
            &Ok(())
        ));
    }

    #[test]
    fn opens_when_creation_was_needed_and_the_save_succeeded() {
        assert!(App::should_open_config_after_ensuring_it_exists(
            true,
            &Ok(())
        ));
    }

    #[test]
    fn does_not_open_when_creation_was_needed_and_the_save_failed() {
        // The exact bug #174 reports: `let _ = self.config.save();` used to
        // discard this and open the shell on a file that might not exist.
        assert!(!App::should_open_config_after_ensuring_it_exists(
            true,
            &Err(anyhow::anyhow!("simulated Credential Manager failure"))
        ));
    }

    // -- pause_toggle_action (issue #181) ----------------------------------

    #[test]
    fn while_running_the_toggle_chord_pauses_until_resumed() {
        assert_eq!(
            super::pause_toggle_action(false),
            super::PauseToggleAction::PauseUntilResumed
        );
    }

    #[test]
    fn while_paused_the_toggle_chord_resumes() {
        // The task's own framing: "paused + event matches pause chord ->
        // toggle". `pause_toggle_action` is the pure "which direction"
        // half of that toggle; `hotkey::pause_hotkey_outcome` (hotkey.rs)
        // is the pure "does this event match at all" half.
        assert_eq!(
            super::pause_toggle_action(true),
            super::PauseToggleAction::Resume
        );
    }

    // -- settings_reentrancy_policy (issue #152) --------------------------

    /// An arbitrary but fixed stand-in for the runtime-registered
    /// `TaskbarCreated` id, distinct from every `WM_APP_*` constant and from
    /// `WM_DESTROY`, so tests can exercise that branch without depending on
    /// an actual `RegisterWindowMessageW` call.
    const FAKE_TASKBAR_CREATED_MSG: u32 = 0xC123;

    #[test]
    fn settings_reentrancy_defers_the_worker_result() {
        // The in-flight answer must still end in a card (rule 7) once
        // Settings closes, so it is deferred rather than dropped or handled
        // reentrantly behind the modal.
        assert_eq!(
            settings_reentrancy_policy(WM_APP_RESULT, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Defer
        );
    }

    // Issue #213: WM_APP_CALENDAR_RESULT, WM_APP_REVIEW_RESULT and
    // WM_APP_PREVIEW_DECIDED used to be Ignore'd here (their boxed payload,
    // if any, freed and thrown away) instead of deferred like WM_APP_RESULT
    // -- silently dropping a finished calendar/review flow, or a preview's
    // "Do it" decision, if Settings happened to be open. All three now get
    // exactly the same Defer treatment as WM_APP_RESULT.

    #[test]
    fn settings_reentrancy_defers_the_calendar_result() {
        assert_eq!(
            settings_reentrancy_policy(WM_APP_CALENDAR_RESULT, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Defer
        );
    }

    #[test]
    fn settings_reentrancy_defers_the_review_result() {
        assert_eq!(
            settings_reentrancy_policy(WM_APP_REVIEW_RESULT, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Defer
        );
    }

    #[test]
    fn settings_reentrancy_defers_the_form_fill_result() {
        // #40: a finished "Fill this form" run joins the same Defer
        // treatment as the calendar/review results just above -- it must
        // not silently lose its card just because Settings happened to be
        // open (rule 7).
        assert_eq!(
            settings_reentrancy_policy(WM_APP_FORM_FILL_RESULT, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Defer
        );
    }

    #[test]
    fn settings_reentrancy_defers_preview_decided() {
        // The most important of the three: a dropped WM_APP_PREVIEW_DECIDED
        // meant a confirmed "Do it" never ran its executor at all, not just
        // a missing card.
        assert_eq!(
            settings_reentrancy_policy(WM_APP_PREVIEW_DECIDED, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Defer
        );
    }

    #[test]
    fn settings_reentrancy_defers_taskbar_created() {
        // Explorer restarting while Settings is open must still get the
        // tray icon back, just after Settings closes rather than reentrantly.
        assert_eq!(
            settings_reentrancy_policy(FAKE_TASKBAR_CREATED_MSG, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::DeferTaskbarCreated
        );
    }

    #[test]
    fn settings_reentrancy_treats_unset_taskbar_id_as_never_matching() {
        // `0` means "not registered yet" (see TASKBAR_CREATED_MSG's doc
        // comment) and must never itself be treated as the broadcast, even
        // though `msg` could theoretically be 0.
        assert_eq!(
            settings_reentrancy_policy(0, 0),
            SettingsReentrancy::Fallback
        );
    }

    #[test]
    fn settings_reentrancy_ignores_hotkey_activate_tray_dismiss_learned_and_pause_toggle() {
        // Issue #213: WM_APP_CALENDAR_RESULT, WM_APP_REVIEW_RESULT,
        // WM_APP_PREVIEW_DECIDED and (#40) WM_APP_FORM_FILL_RESULT used to
        // be asserted Ignore here too, before they moved to Defer -- see
        // the `..._defers_...` tests above instead.
        for msg in [
            WM_APP_HOTKEY,
            WM_APP_ACTIVATE,
            WM_APP_TRAY,
            WM_APP_DISMISS,
            WM_APP_LEARNED,
            WM_APP_PAUSE_TOGGLE,
        ] {
            assert_eq!(
                settings_reentrancy_policy(msg, FAKE_TASKBAR_CREATED_MSG),
                SettingsReentrancy::Ignore,
                "msg {msg:#x} should be Ignore while Settings is open"
            );
        }
    }

    #[test]
    fn settings_reentrancy_ignores_palette_and_router_messages() {
        // #25/#24: these three share one match arm in
        // `settings_reentrancy_policy` -- covered together here (a gap the
        // pre-existing `..._hotkey_activate_tray_dismiss_learned_and_pause_toggle`
        // test above never closed for WM_APP_PALETTE_TOGGLE/WM_APP_PALETTE_RUN;
        // filed as a finding rather than folded into that test's name, which
        // this commit does not otherwise touch). WM_APP_ROUTER_RESULT is
        // advisory (unlike WM_APP_FORM_FILL_RESULT, a finished action's own
        // result, which is Defer instead), so it stays Ignore here rather
        // than joining the Defer group -- see `SettingsReentrancy::Ignore`'s
        // own doc comment.
        for msg in [
            WM_APP_PALETTE_TOGGLE,
            WM_APP_PALETTE_RUN,
            WM_APP_ROUTER_RESULT,
        ] {
            assert_eq!(
                settings_reentrancy_policy(msg, FAKE_TASKBAR_CREATED_MSG),
                SettingsReentrancy::Ignore,
                "msg {msg:#x} should be Ignore while Settings is open"
            );
        }
    }

    #[test]
    fn settings_reentrancy_falls_back_to_def_window_proc_for_everything_else() {
        // WM_DESTROY is the concrete example named in the doc comment: it is
        // unreachable in practice (its only path, WM_APP_TRAY, is Ignore'd
        // above), but the policy itself has no special-case for it -- an
        // unrecognised message always goes to DefWindowProcW, never to App.
        assert_eq!(
            settings_reentrancy_policy(WM_DESTROY, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::Fallback
        );
    }

    // -- WM_APP ids (issue #163) ------------------------------------------
    //
    // Six `WM_APP_*` constants are declared across app.rs, dismiss.rs,
    // hotkey.rs and ui/tray.rs. This module already imports all six above
    // (it is the one place that depends on all four), so the
    // pairwise-uniqueness check lives here rather than in each file
    // separately -- same pattern as settings.rs's
    // `control_ids_are_pairwise_unique` and tray.rs's
    // `fixed_cmd_ids_are_pairwise_unique`.

    /// Every `WM_APP_*` constant declared anywhere in the crate, paired with
    /// its name. Add a new one here when adding the constant itself --
    /// `wm_app_ids_registry_is_exhaustive` below fails loudly if this list
    /// falls out of sync with the source.
    const ALL_WM_APP_IDS: &[(&str, u32)] = &[
        ("WM_APP_TRAY", WM_APP_TRAY),
        ("WM_APP_HOTKEY", WM_APP_HOTKEY),
        ("WM_APP_RESULT", WM_APP_RESULT),
        ("WM_APP_LEARNED", WM_APP_LEARNED),
        ("WM_APP_DISMISS", WM_APP_DISMISS),
        ("WM_APP_ACTIVATE", WM_APP_ACTIVATE),
        ("WM_APP_PAUSE_TOGGLE", WM_APP_PAUSE_TOGGLE),
        ("WM_APP_CALENDAR_RESULT", WM_APP_CALENDAR_RESULT),
        ("WM_APP_PREVIEW_DECIDED", WM_APP_PREVIEW_DECIDED),
        ("WM_APP_PALETTE_TOGGLE", WM_APP_PALETTE_TOGGLE),
        ("WM_APP_PALETTE_RUN", WM_APP_PALETTE_RUN),
        ("WM_APP_REVIEW_RESULT", WM_APP_REVIEW_RESULT),
        ("WM_APP_FORM_FILL_RESULT", WM_APP_FORM_FILL_RESULT),
        ("WM_APP_ROUTER_RESULT", WM_APP_ROUTER_RESULT),
    ];

    #[test]
    fn wm_app_ids_are_pairwise_unique() {
        for (i, (name_a, id_a)) in ALL_WM_APP_IDS.iter().enumerate() {
            for (name_b, id_b) in ALL_WM_APP_IDS.iter().skip(i + 1) {
                assert_ne!(
                    id_a, id_b,
                    "{name_a} and {name_b} share WM_APP id {id_a} -- wnd_proc's match \
                     would let one handler silently steal the other's messages"
                );
            }
        }
    }

    /// Counts `pub const WM_APP_<NAME>: u32 = WM_APP + <n>;` declarations by
    /// re-reading the four source files as text, so a new constant added to
    /// any of them without a matching entry in `ALL_WM_APP_IDS` fails this
    /// test instead of silently skipping the uniqueness check above.
    #[test]
    fn wm_app_ids_registry_is_exhaustive() {
        fn count_declarations(src: &str) -> usize {
            src.lines()
                .filter(|line| {
                    let t = line.trim_start();
                    t.starts_with("pub const WM_APP_") && t.contains("= WM_APP + ")
                })
                .count()
        }

        let declared = count_declarations(include_str!("app.rs"))
            + count_declarations(include_str!("dismiss.rs"))
            + count_declarations(include_str!("hotkey.rs"))
            + count_declarations(include_str!("ui/tray.rs"))
            + count_declarations(include_str!("ui/card.rs"))
            + count_declarations(include_str!("ui/palette.rs"));

        assert_eq!(
            declared,
            ALL_WM_APP_IDS.len(),
            "found {declared} `pub const WM_APP_* = WM_APP + n;` declarations across \
             app.rs/dismiss.rs/hotkey.rs/ui/tray.rs/ui/card.rs/ui/palette.rs but \
             ALL_WM_APP_IDS lists {}; add the new constant to ALL_WM_APP_IDS too",
            ALL_WM_APP_IDS.len()
        );
    }

    // -- settings_reentrancy_policy exhaustiveness (issue #213) -----------
    //
    // #163's ALL_WM_APP_IDS/wm_app_ids_registry_is_exhaustive above already
    // guarantee every WM_APP_* constant in the crate is listed once. This
    // reuses that same list to guarantee every one of them is ALSO
    // classified by settings_reentrancy_policy -- the exact gap that let
    // WM_APP_CALENDAR_RESULT and WM_APP_PREVIEW_DECIDED quietly stay
    // Ignore'd instead of Defer'd (and let WM_APP_PALETTE_TOGGLE/
    // WM_APP_PALETTE_RUN go untested by name at all) until #213. Adding a
    // WM_APP_* id to ALL_WM_APP_IDS without adding a matching entry here
    // fails this test, so a future result/decision message cannot be
    // silently forgotten the same way again.

    /// Every id in `ALL_WM_APP_IDS`, paired with the `SettingsReentrancy`
    /// `settings_reentrancy_policy` must return for it. `TaskbarCreated`
    /// itself is not a `WM_APP_*` constant (it's a runtime-registered
    /// window message, see `TASKBAR_CREATED_MSG`), so `DeferTaskbarCreated`
    /// never appears here -- it is covered by
    /// `settings_reentrancy_defers_taskbar_created` instead.
    const REENTRANCY_POLICY_TABLE: &[(&str, u32, SettingsReentrancy)] = &[
        ("WM_APP_TRAY", WM_APP_TRAY, SettingsReentrancy::Ignore),
        ("WM_APP_HOTKEY", WM_APP_HOTKEY, SettingsReentrancy::Ignore),
        ("WM_APP_RESULT", WM_APP_RESULT, SettingsReentrancy::Defer),
        ("WM_APP_LEARNED", WM_APP_LEARNED, SettingsReentrancy::Ignore),
        ("WM_APP_DISMISS", WM_APP_DISMISS, SettingsReentrancy::Ignore),
        (
            "WM_APP_ACTIVATE",
            WM_APP_ACTIVATE,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_PAUSE_TOGGLE",
            WM_APP_PAUSE_TOGGLE,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_CALENDAR_RESULT",
            WM_APP_CALENDAR_RESULT,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_PREVIEW_DECIDED",
            WM_APP_PREVIEW_DECIDED,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_PALETTE_TOGGLE",
            WM_APP_PALETTE_TOGGLE,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_PALETTE_RUN",
            WM_APP_PALETTE_RUN,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_REVIEW_RESULT",
            WM_APP_REVIEW_RESULT,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_FORM_FILL_RESULT",
            WM_APP_FORM_FILL_RESULT,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_ROUTER_RESULT",
            WM_APP_ROUTER_RESULT,
            SettingsReentrancy::Ignore,
        ),
    ];

    #[test]
    fn reentrancy_policy_table_matches_all_wm_app_ids() {
        assert_eq!(
            REENTRANCY_POLICY_TABLE.len(),
            ALL_WM_APP_IDS.len(),
            "ALL_WM_APP_IDS has {} entries but REENTRANCY_POLICY_TABLE has {} -- a WM_APP_* \
             id was added to one without the other; classify every id in both places so a \
             new result message cannot be silently dropped while Settings is open",
            ALL_WM_APP_IDS.len(),
            REENTRANCY_POLICY_TABLE.len()
        );
        for (name, id) in ALL_WM_APP_IDS {
            assert!(
                REENTRANCY_POLICY_TABLE
                    .iter()
                    .any(|(n, i, _)| n == name && i == id),
                "{name} is in ALL_WM_APP_IDS but missing from REENTRANCY_POLICY_TABLE"
            );
        }
    }

    #[test]
    fn settings_reentrancy_policy_matches_the_table_for_every_wm_app_id() {
        for (name, id, expected) in REENTRANCY_POLICY_TABLE {
            assert_eq!(
                settings_reentrancy_policy(*id, FAKE_TASKBAR_CREATED_MSG),
                *expected,
                "{name} is classified {expected:?} in REENTRANCY_POLICY_TABLE but \
                 settings_reentrancy_policy disagrees"
            );
        }
    }

    #[test]
    fn first_line_takes_only_the_first_line() {
        assert_eq!(first_line("boom\ndetails here", 88), "boom");
    }

    #[test]
    fn first_line_truncates_with_ellipsis() {
        let out = first_line(&"x".repeat(200), 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn first_line_is_char_safe_on_multibyte() {
        // Truncating by bytes here would panic or produce invalid UTF-8.
        let out = first_line(&"é".repeat(50), 5);
        assert_eq!(out.chars().count(), 5);
    }

    #[test]
    fn first_line_leaves_short_text_alone() {
        assert_eq!(first_line("fine", 88), "fine");
    }

    // -- router/chain provider-name agreement (issue #222) -------------------
    //
    // provider_for_router/router_models_for used to hand-mirror config.rs's
    // provider_for match arms; a provider kind added to config.rs without a
    // matching arm here silently made the router skip it forever (no error
    // card by design). Both are now one-line calls into
    // Providers::provider_for_named_model/models_for, but structural dedup
    // can be undone by a future refactor without anyone noticing -- this
    // test is the guard: it walks a fixture's providers.order through BOTH
    // the real chain-building path (Providers::build_chain, config.rs) and
    // the router's own entry points (provider_for_router/router_models_for,
    // called here exactly as router_worker calls them) and asserts they
    // agree on which names resolve to a provider at all.
    //
    // Proof this guard has teeth (see the commit message for the full
    // transcript): provider_for_router was temporarily given a throwaway
    // arm recognizing "not-a-real-provider" (already in this fixture's
    // order, as the name nothing should recognize) without touching
    // config.rs's match at all. This test then failed:
    // `assertion `left == right` failed: chain path and router path
    // disagree on provider name "not-a-real-provider"` with `left: false,
    // right: true` (chain path still says unrecognized; router path now
    // says recognized). The throwaway arm was reverted before committing.

    #[test]
    fn router_and_chain_paths_agree_on_which_provider_names_resolve() {
        let mut providers = Providers::default();
        providers.compat.push(crate::config::CompatConfig {
            name: "custom".to_string(),
            models: vec!["compat-cheap".to_string(), "compat-flagship".to_string()],
            ..Default::default()
        });
        // The fixture: one entry per recognized kind, plus a compat entry
        // and a name nothing should ever recognize.
        providers.order = vec![
            "openai".to_string(),
            "anthropic".to_string(),
            "gemini".to_string(),
            "ollama".to_string(),
            "compat:custom".to_string(),
            "not-a-real-provider".to_string(),
        ];

        for name in providers.order.clone() {
            // The real chain-building path: Providers::build_chain (via the
            // private provider_for) omits an unrecognized name entirely and
            // includes every recognized one, ready or not (config.rs's own
            // doc on build_chain). Isolating `order` to just this one name
            // turns that inclusion into a yes/no per name.
            let mut isolated = providers.clone();
            isolated.order = vec![name.clone()];
            let chain_recognizes = !isolated.build_chain().provider_names().is_empty();

            // The router's own path, called exactly as router_worker calls
            // it: router_models_for for the model list, provider_for_router
            // to build the provider from a name plus one of those models
            // (or a placeholder, for the "recognized at all" question this
            // test asks -- issue #222 is explicit that this is about names,
            // not which model gets picked).
            let router_models = router_models_for(&providers, &name);
            let router_recognizes =
                provider_for_router(&providers, &name, "placeholder-model").is_some();

            assert_eq!(
                chain_recognizes, router_recognizes,
                "chain path and router path disagree on provider name {name:?}"
            );

            // And the two model-list mirrors agree too (issue #222's second
            // mirror): whatever config.rs's own Providers::models_for says
            // for this name is exactly what the router asked for.
            assert_eq!(
                router_models,
                providers.models_for(&name),
                "router and config model lists disagree for provider name {name:?}"
            );
        }
    }

    // -- live intent router measurement (issue #24) --------------------------

    /// Renders `lines` as separate black-on-white text lines via GDI into an
    /// in-memory DIB (top-aligned, one `DrawTextW` call per line) -- the
    /// synthetic "email compose window" `router_live_recognizes_email_compose`
    /// runs the real router through. Small and duplicated rather than shared
    /// with `ocr.rs`'s/`provider/mod.rs`'s own GDI-render test helpers -- an
    /// established convention in this crate's live tests (see
    /// `provider/mod.rs`'s `render_gdi_text_rgba_for_ocr` doc comment for the
    /// same reasoning). Test-only; not reachable from production code.
    unsafe fn render_gdi_lines_rgba(lines: &[&str], width: u32, height: u32) -> Vec<u8> {
        use windows::Win32::Foundation::{COLORREF, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
            DeleteObject, DrawTextW, FillRect, SelectObject, SetBkColor, SetTextColor,
            ANSI_CHARSET, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH,
            DEFAULT_QUALITY, DIB_RGB_COLORS, DT_LEFT, DT_SINGLELINE, DT_TOP, FW_NORMAL,
            OUT_DEFAULT_PRECIS,
        };

        let hdc = CreateCompatibleDC(None);
        assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // negative => top-down, row 0 first
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbitmap = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection failed");
        assert!(!bits.is_null(), "CreateDIBSection returned a null buffer");

        let old_bitmap = SelectObject(hdc, hbitmap.into());

        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let full_rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        FillRect(hdc, &full_rect, white);
        let _ = DeleteObject(white.into());

        let hfont = CreateFontW(
            -24,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            ANSI_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            DEFAULT_PITCH.0 as u32,
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, hfont.into());
        SetTextColor(hdc, COLORREF(0x0000_0000));
        SetBkColor(hdc, COLORREF(0x00FF_FFFF));

        let line_height = 28i32;
        for (i, line) in lines.iter().enumerate() {
            // MEASURED 2026-09-17: an empty (zero-length) buffer crashes
            // `DrawTextW` here (STATUS_ACCESS_VIOLATION) -- THEORY
            // (unverified): the `windows` crate's binding treats a
            // zero-length `&mut [u16]` as "scan for a null terminator"
            // rather than "draw nothing", walking off the end of the
            // `Vec`'s dangling-but-valid empty-allocation pointer. A blank
            // line in the synthetic email body is drawn as nothing by
            // simply skipping the call, which is what an empty line should
            // paint anyway.
            if line.is_empty() {
                continue;
            }
            let mut text_wide: Vec<u16> = line.encode_utf16().collect();
            let mut rect = RECT {
                left: 12,
                top: 8 + line_height * i as i32,
                right: width as i32 - 12,
                bottom: 8 + line_height * (i as i32 + 1),
            };
            DrawTextW(
                hdc,
                &mut text_wide,
                &mut rect,
                DT_LEFT | DT_TOP | DT_SINGLELINE,
            );
        }

        let pixel_count = (width as usize) * (height as usize) * 4;
        let bgra = std::slice::from_raw_parts(bits as *const u8, pixel_count).to_vec();

        SelectObject(hdc, old_font);
        let _ = DeleteObject(hfont.into());
        SelectObject(hdc, old_bitmap);
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(hdc);

        let mut rgba = Vec::with_capacity(pixel_count);
        for px in bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        rgba
    }

    /// Regression check for the `DrawTextW`-on-an-empty-line crash
    /// documented on `render_gdi_lines_rgba` and on
    /// `router_live_recognizes_email_compose` below -- no network, so this
    /// runs on every ordinary `cargo test`, unlike the live test that first
    /// caught it.
    #[test]
    fn render_gdi_lines_rgba_tolerates_a_blank_line() {
        let rgba = unsafe { render_gdi_lines_rgba(&["a", "", "b"], 200, 100) };
        assert_eq!(rgba.len(), 200 * 100 * 4);
    }

    /// #24's live measurement: a synthetic email-compose window (To:,
    /// Subject:, a typo'd body), rendered directly at
    /// `router::ROUTER_IMAGE_LONG_EDGE` (the real production size, not a
    /// larger draft downscaled afterward), through the REAL intent router
    /// (`router::build_request`/`router::parse_router_result`, the same
    /// functions `App::router_worker` calls) against local Ollama
    /// `gemma3:4b`. Run manually (CLAUDE.md build rules -- never bare
    /// `cargo test`):
    /// ```text
    /// CARGO_TARGET_DIR=... RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=2 \
    ///   cargo test router_live -- --ignored --nocapture
    /// ```
    /// Requires a local Ollama server on 127.0.0.1:11434 with `gemma3:4b`
    /// pulled. Expects `intent == Some("review-email")` when that action
    /// exists in `actions::load_actions()`'s catalogue (added by a sibling
    /// agent in parallel, per #24's task description); otherwise only
    /// requires SOME valid id was named, never "none" -- either way the
    /// chosen id is printed. Unloads the model afterward (`keep_alive:
    /// "0"`), the same convention `provider/mod.rs`'s and `ollama.rs`'s own
    /// live checks already use.
    ///
    /// MEASURED 2026-09-17 (this crate's dev profile, `gemma3:4b`, catalogue
    /// of 5 rows -- "review-email" not yet on master at measurement time):
    /// elapsed 45.71s (cold: the model was not already loaded going in --
    /// see `keep_alive`'s doc on why every OTHER live check in this crate
    /// unloads immediately after too, which keeps every measurement a cold
    /// one unless run back-to-back), 408 input tokens, 31 output tokens.
    /// `intent` came back `Some("copy-region")` (a valid, non-"none" id,
    /// satisfying this test's fallback assertion) rather than a more
    /// obviously email-shaped choice, and `summary` echoed an action id
    /// instead of describing the image -- THEORY (unverified): `gemma3:4b`
    /// at this image size and prompt does not reliably follow the
    /// `summary`-before-`intent` schema-order instruction (rule 3's
    /// "commits to a verdict before doing the arithmetic" reasoning assumes
    /// a model capable enough to use the ordering at all); filed as a
    /// finding rather than reworked here, since #24's Done-when is the
    /// router's WIRING (capture, request, threshold, staleness,
    /// pre-selection), not this one model's answer quality, and the
    /// expansion plan's own router default is `qwen3.5:4b`, not
    /// `gemma3:4b` (this test's model choice is fixed by the task that
    /// created it, not a production recommendation).
    ///
    /// Separately: an early draft of this test's GDI renderer crashed
    /// (`STATUS_ACCESS_VIOLATION`) on a blank line in the body text --
    /// MEASURED 2026-09-17: `DrawTextW` with a zero-length `&mut [u16]`
    /// buffer reliably crashes through this crate's `windows` binding;
    /// `render_gdi_lines_rgba` above now skips empty lines entirely rather
    /// than calling `DrawTextW` with nothing to draw.
    #[test]
    #[ignore = "live: a real local Ollama call against a rendered image; run manually, see this test's doc comment"]
    fn router_live_recognizes_email_compose() {
        use crate::provider::ollama::{Ollama, DEFAULT_BASE_URL};

        // Rendered directly at ROUTER_IMAGE_LONG_EDGE (not a larger canvas
        // downscaled afterward): this is the actual pixel budget
        // `App::maybe_start_router` sends in production, so a passing
        // result here is real evidence for -- and a failing one real
        // evidence against -- `router::ROUTER_IMAGE_LONG_EDGE`'s doc
        // comment's THEORY that 512 stays legible enough for this kind of
        // screen.
        let width = router::ROUTER_IMAGE_LONG_EDGE;
        let height = width * 260 / 1000; // same aspect ratio as the original 1000x260 draft
        let rgba = unsafe {
            render_gdi_lines_rgba(
                &[
                    "To: dana@example.com",
                    "Subject: Q3 numbrs",
                    "",
                    "Hi Dana, pls find atached the Q3 numbrs, let me no if",
                    "anything looks of. Thnks!",
                ],
                width,
                height,
            )
        };
        let raw = capture::RawShot {
            rgba,
            width,
            height,
        };
        let shot = capture::encode(&raw).expect("encode synthetic PNG");

        let resolved = actions::load_actions()
            .expect("load_actions should succeed against whatever actions.toml (or none) is on this machine");
        let catalogue = palette_model::catalogue(&resolved);
        let candidates = router::candidates_from_catalogue(&catalogue);
        let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();

        let req = router::build_request(shot.png, &candidates);

        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let started = std::time::Instant::now();
        let completion = provider
            .complete(&req)
            .expect("live ollama router request should succeed");
        let elapsed = started.elapsed();

        // Unload right after the measurement (mirrors `provider/mod.rs`'s /
        // `ollama.rs`'s own live-check convention) -- a trivial follow-up
        // request purely to release VRAM; its own result is not the
        // measurement, so a failure here is not fatal to the test.
        let mut unload_provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        unload_provider.keep_alive = "0".to_string();
        let _ = unload_provider.complete(&router::build_request(vec![], &[]));

        let result = router::parse_router_result(&completion.text, &candidate_ids)
            .expect("router completion should parse against its own schema");

        eprintln!(
            "MEASURED 2026-09-17: router_live: model=gemma3:4b elapsed={elapsed:?} \
             summary={:?} intent={:?} confidence={}",
            result.summary, result.intent, result.confidence
        );
        if let Some(usage) = completion.usage {
            eprintln!(
                "MEASURED 2026-09-17: router_live token usage: input={} output={}",
                usage.input_tokens, usage.output_tokens
            );
        }

        if candidate_ids.iter().any(|id| id == "review-email") {
            assert_eq!(
                result.intent.as_deref(),
                Some("review-email"),
                "the \"review-email\" action exists in this catalogue; expected the router \
                 to pick it for an email compose window"
            );
        } else {
            assert!(
                result.intent.is_some(),
                "expected the router to name some action id for an unambiguous email compose \
                 window, got none (\"review-email\" is not in this catalogue yet)"
            );
        }
    }
}
