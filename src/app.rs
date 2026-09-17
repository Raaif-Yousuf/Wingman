//! Orchestration: the hidden owner window, the single message loop, and the
//! state machine that ties hotkeys, capture, providers and the card together.
//!
//! This module is wiring only — every piece of real logic lives in the module
//! that owns it. The one rule that matters here: the main thread owns every
//! `HWND` and runs the only message loop. Work that can block (capture is
//! quick, the API call is not) happens on a worker thread, which reports back
//! exclusively by `PostMessageW`.

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
    WM_APP_PAUSE_TOGGLE,
};
use crate::mode::{self, Mode};
use crate::pause::{self, PauseChoice, PauseState};
use crate::provider::{
    calendar_request, parse_answer, physics_request, review_request_from_screen,
    review_request_from_text, Answer, Chain, Shot,
};
use crate::ui::card::{Card, WM_APP_PREVIEW_DECIDED};
use crate::ui::confirm;
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
pub const WM_APP_REVIEW_RESULT: u32 = WM_APP + 10;

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

    let mut app = Box::new(App {
        instance,
        taskbar_created_msg,
        config,
        chain,
        card,
        tray,
        hook: None,
        watcher: None,
        busy: false,
        last: None,
        pause: PauseState::Running,
        pending_conflict: None,
        pending_review: None,
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

    /// The whole flow: hide any stale card, check a provider is actually
    /// ready, grab the screen, then hand the bytes to a worker so the
    /// message loop stays responsive during the call.
    fn ask(&mut self) {
        if self.busy {
            return;
        }

        // Pause (issue #20): no network request may start while paused.
        // The hotkey path never reaches here at all while paused (the hook
        // in hotkey.rs passes the chord through before ever posting
        // WM_APP_HOTKEY), so this guard exists for the other entry points --
        // the tray's "Ask now" and a second Copilot-key launch
        // (WM_APP_ACTIVATE) -- which don't go through the hook.
        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
            return;
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
            return;
        }

        // Capture (grab the pixels and downscale) runs here, on the main
        // thread, and must happen before the pending card is shown --
        // otherwise the card is in its own screenshot. Encoding those pixels
        // to PNG does NOT happen here: issue #177 measured
        // `CompressionType::Best` PNG encoding at up to ~1s in a release
        // build on a 1402x876 image, which froze the message loop for that
        // whole time with nothing on screen after the key press. `encode`
        // now runs on the worker thread below, after `show_pending`.
        //
        // Issue #169: the downscale target comes from the FIRST provider
        // `worker` (below) will actually try, not a provider-agnostic
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
                return;
            }
        };
        // Issue #18/#206: the foreground window's HWND, captured HERE on the
        // main thread, right alongside the pixel capture -- both are "what
        // was actually on screen at press time", and both must be read
        // before `show_pending()` below puts Wingman's own card on top.
        // Only the isize is carried into the worker closure (an `HWND`
        // wraps a raw pointer and is not `Send`; `hwnd_isize`/`target`
        // below already use the same pattern for the owner window). This
        // HWND is used only lazily, inside the non-vision fallback -- see
        // `non_vision_inputs` -- so capturing it costs nothing when every
        // provider in the chain turns out to have vision.
        let foreground_hwnd_isize =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;

        self.busy = true;
        // Disarmed for the whole in-flight window: a click while the spinner
        // is up must not touch the card.
        self.set_watch(false);
        self.card.show_pending();

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
    /// true`) instead of a fixed request shape. Mirrors `ask()`'s pause/
    /// busy/readiness/capture steps (duplicated, not extracted into a
    /// shared helper, precisely so `ask()`'s own code is untouched -- see
    /// #214, filed for the de-duplication follow-up).
    fn add_event_from_screen(&mut self) {
        if self.busy {
            return;
        }

        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "config.toml".into());
        if let Some((headline, detail)) =
            Self::readiness_gate(self.config.mode, &self.config.providers, &path)
        {
            self.card.show_error(&headline, &detail);
            return;
        }

        let (today, offset_minutes) = match local_today_and_utc_offset() {
            Ok(v) => v,
            Err(e) => {
                self.card
                    .show_error("Couldn't read the local date", &format!("{e:#}"));
                return;
            }
        };

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
                return;
            }
        };
        let foreground_hwnd_isize =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;

        self.busy = true;
        self.set_watch(false);
        self.card.show_pending();

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
    /// #38: `self.pending_review` is `Some` exactly when the preview
    /// currently closing was "Review this email"'s, not "Add event from
    /// screen"'s (only one preview is ever on screen at a time, so this is
    /// an unambiguous branch, not a guess) -- taken and cleared
    /// unconditionally here, on Cancel/Esc as much as on "Do it", so a
    /// cancelled review preview never leaves a stale target behind for the
    /// next one. When it is `None`, this is calendar's own flow: the
    /// executor is resolved fresh by the fixed `"calendar_add"` name, the
    /// same "fixed until a second confirm-required action exists" status
    /// `CalendarAddExecutor::new`'s own fixed `"ics"` connector choice had
    /// before #38 landed.
    fn on_preview_decided(&mut self) {
        let pending_review = self.pending_review.take();
        let Some(confirmed) = self.card.take_confirmed() else {
            return;
        };
        if let Some(ctx) = pending_review {
            self.run_review_executor(ctx);
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
    /// method touches). Mirrors `add_event_from_screen`'s pause/busy/
    /// readiness/capture steps, with one difference: the (potentially slow)
    /// UIA compose-body/selection capture attempts run on the SPAWNED
    /// worker thread, never here, per `inputs::uia`'s and
    /// `inputs::selection`'s own module docs ("call from a dedicated worker
    /// thread"); only the screenshot -- needed only as the last-resort
    /// fallback, but cheap, and must happen before any card change per
    /// `capture::grab_raw`'s existing "no stale card in the shot" rule --
    /// is still grabbed here, eagerly, exactly like `add_event_from_screen`
    /// already does.
    fn review_this_email(&mut self) {
        if self.busy {
            return;
        }

        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "config.toml".into());
        if let Some((headline, detail)) =
            Self::readiness_gate(self.config.mode, &self.config.providers, &path)
        {
            self.card.show_error(&headline, &detail);
            return;
        }

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
                return;
            }
        };
        let foreground_hwnd_isize =
            unsafe { windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow() }.0 as isize;

        self.busy = true;
        self.set_watch(false);
        self.card.show_pending();

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
    /// thread-local, not on `App`: `SETTINGS_OPEN` itself, `PENDING_RESULT`
    /// (a worker's answer that arrived mid-edit),
    /// `TASKBAR_RECREATED_WHILE_SETTINGS`, and `PAUSE_REEVALUATE_PENDING`
    /// (issue #20). All four are only touched here, immediately before and
    /// after `show_modal`, when no reentrant call can possibly be in flight.
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
    fn open_settings(&mut self) {
        // The card would sit on top of the settings window, and a click in
        // that window would dismiss it anyway.
        self.card.hide();
        self.set_watch(false);

        SETTINGS_OPEN.with(|c| c.set(true));
        let edited = settings::show_modal(self.instance, &self.config);
        SETTINGS_OPEN.with(|c| c.set(false));

        let pending = PENDING_RESULT.with(|c| c.borrow_mut().take());
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
            // Cancelled/closed without saving. An answer that finished
            // mid-edit still gets its card.
            if let Some(result) = pending {
                self.on_result(result);
            }
            self.show_tray_restore_error(tray_restore_error);
            return;
        };

        self.config = edited;
        if let Err(e) = self.config.save() {
            // Rule 7: neither failure may be silently dropped. The save
            // error is the one the user just directly caused (they clicked
            // Save), so it gets the card; the deferred answer is not lost
            // either, just not shown as a card here -- `record_last` still
            // makes it available via "Copy last answer" until the next ask.
            self.card
                .show_error("Couldn't save settings", &format!("{e:#}"));
            if let Some(result) = pending {
                self.record_last(result);
            }
            self.show_tray_restore_error(tray_restore_error);
            return;
        }
        self.apply_config();

        match pending {
            Some(result) => self.on_result(result),
            None => {
                self.card.show_answer("Settings saved", "", 3, None);
                self.set_watch(true);
            }
        }
        self.show_tray_restore_error(tray_restore_error);
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

    /// A `WM_APP_RESULT` payload that arrived while `SETTINGS_OPEN` was
    /// true. `open_settings` delivers it once `show_modal` returns, so the
    /// in-flight request still ends in a card (rule 7) instead of being
    /// dropped or clobbered by "Settings saved". See `SETTINGS_OPEN` for why
    /// this can't be a field on `App`.
    static PENDING_RESULT: std::cell::RefCell<Option<std::result::Result<Answer, String>>> =
        const { std::cell::RefCell::new(None) };

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
    /// case that invariant ever changes), and `WM_APP_PAUSE_TOGGLE` (#181:
    /// same treatment as the hotkey -- a chord press while Settings is open
    /// is dropped, not queued).
    Ignore,
    /// Stash the worker's payload in `PENDING_RESULT`; `open_settings`
    /// delivers it after `show_modal` returns, so an answer that finished
    /// mid-edit still ends in a card (rule 7) instead of being silently
    /// overwritten by "Settings saved".
    DeferResult,
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
        WM_APP_RESULT => SettingsReentrancy::DeferResult,
        // Issue #39: WM_APP_CALENDAR_RESULT/WM_APP_PREVIEW_DECIDED are
        // Ignored, not deferred like WM_APP_RESULT -- Settings being open
        // while the "Add event from screen" flow is mid-flight is a corner
        // case this task does not build full deferral plumbing for (a
        // second PENDING_RESULT-shaped thread-local per message type).
        // WM_APP_CALENDAR_RESULT still carries a boxed payload, freed
        // explicitly in the Ignore arm below (mirroring WM_APP_LEARNED) so
        // it never leaks; WM_APP_PREVIEW_DECIDED carries none. Filed as
        // #213 for the same Defer*-shaped fix #152 already built for
        // WM_APP_RESULT.
        WM_APP_HOTKEY
        | WM_APP_ACTIVATE
        | WM_APP_TRAY
        | WM_APP_DISMISS
        | WM_APP_LEARNED
        | WM_APP_PAUSE_TOGGLE
        | WM_APP_CALENDAR_RESULT
        | WM_APP_REVIEW_RESULT
        | WM_APP_PREVIEW_DECIDED => SettingsReentrancy::Ignore,
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
                // WM_APP_LEARNED and WM_APP_CALENDAR_RESULT are the only
                // ignored messages carrying a boxed payload; free them so
                // neither leaks.
                if msg == WM_APP_LEARNED {
                    drop(unsafe { Box::from_raw(lparam.0 as *mut Chord) });
                } else if msg == WM_APP_CALENDAR_RESULT {
                    drop(unsafe {
                        Box::from_raw(lparam.0 as *mut std::result::Result<Value, String>)
                    });
                } else if msg == WM_APP_REVIEW_RESULT {
                    drop(unsafe {
                        Box::from_raw(
                            lparam.0
                                as *mut std::result::Result<
                                    actions::review_email::ReviewOutcome,
                                    String,
                                >,
                        )
                    });
                }
                return LRESULT(0);
            }
            SettingsReentrancy::DeferResult => {
                let result =
                    unsafe { *Box::from_raw(lparam.0 as *mut std::result::Result<Answer, String>) };
                PENDING_RESULT.with(|c| *c.borrow_mut() = Some(result));
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
                    MenuChoice::Command(cmd::REVIEW_EMAIL) => app.review_this_email(),
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
    use super::{settings_reentrancy_policy, SettingsReentrancy};
    use super::{WM_APP_ACTIVATE, WM_APP_CALENDAR_RESULT, WM_APP_RESULT, WM_APP_REVIEW_RESULT};
    use crate::actions::{self, Origin};
    use crate::config::Providers;
    use crate::dismiss::WM_APP_DISMISS;
    use crate::hotkey::{WM_APP_HOTKEY, WM_APP_LEARNED, WM_APP_PAUSE_TOGGLE};
    use crate::mode::Mode;
    use crate::ui::card::WM_APP_PREVIEW_DECIDED;
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
            SettingsReentrancy::DeferResult
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
        for msg in [
            WM_APP_HOTKEY,
            WM_APP_ACTIVATE,
            WM_APP_TRAY,
            WM_APP_DISMISS,
            WM_APP_LEARNED,
            WM_APP_PAUSE_TOGGLE,
            WM_APP_CALENDAR_RESULT,
            WM_APP_PREVIEW_DECIDED,
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
        ("WM_APP_REVIEW_RESULT", WM_APP_REVIEW_RESULT),
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
            + count_declarations(include_str!("ui/card.rs"));

        assert_eq!(
            declared,
            ALL_WM_APP_IDS.len(),
            "found {declared} `pub const WM_APP_* = WM_APP + n;` declarations across \
             app.rs/dismiss.rs/hotkey.rs/ui/tray.rs/ui/card.rs but ALL_WM_APP_IDS lists {}; \
             add the new constant to ALL_WM_APP_IDS too",
            ALL_WM_APP_IDS.len()
        );
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
}
