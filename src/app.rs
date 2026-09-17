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
use crate::dismiss::{unpack_point, ClickWatcher, WM_APP_DISMISS};
use crate::hotkey::{
    chord_to_string, Chord, HotkeyHook, HK_PRIMARY, HK_SECONDARY, WM_APP_HOTKEY, WM_APP_LEARNED,
};
use crate::mode::{self, Mode};
use crate::pause::{self, PauseChoice, PauseState};
use crate::provider::{parse_answer, physics_request, Answer, Chain, Shot};
use crate::ui::card::Card;
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
        Ok(h) => app.hook = Some(h),
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
    fn readiness_gate(mode: Mode, providers: &Providers, config_path: &str) -> Option<(String, String)> {
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
        if let Some((headline, detail)) = Self::readiness_gate(self.config.mode, &self.config.providers, &path) {
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
        let raw = match capture::grab_raw(&self.config.capture.monitor, self.config.capture.max_edge) {
            Ok(r) => r,
            Err(e) => {
                self.card
                    .show_error("Couldn't capture the screen", &format!("{e:#}"));
                return;
            }
        };

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
                worker(&providers, mode, &shot, &prompt, want_difficulty)
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
            Ok(()) => self.card.show_answer(&format!("Bound to {name}"), "", 4, None),
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
        if taskbar_recreated {
            self.on_taskbar_created();
        }
        if PAUSE_REEVALUATE_PENDING.with(|c| c.replace(false)) {
            self.reevaluate_pause(self.hwnd());
        }

        let Some(edited) = edited else {
            // Cancelled/closed without saving. An answer that finished
            // mid-edit still gets its card.
            if let Some(result) = pending {
                self.on_result(result);
            }
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
    }

    /// Push `self.config` into everything that caches a piece of it.
    fn apply_config(&mut self) {
        self.chain = Arc::new(self.config.build_chain());
        self.card.set_text_scale(self.config.ui.text_scale);
        if let Some(hook) = &self.hook {
            hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
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
        let save_result = if needed_create { self.config.save() } else { Ok(()) };
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
                &format!("It will revert when you quit.

{e:#}"),
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
                &format!("It will revert when you quit.

{e:#}"),
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
                &format!("It will revert when you quit.

{e:#}"),
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
            self.tray.set_tooltip(&pause::tooltip_text(choice, hour_min));
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
    /// ends in a card rather than silently leaving the tray empty.
    fn on_taskbar_created(&mut self) {
        match self.tray.readd() {
            Ok(()) => self.refresh_tray_labels(),
            Err(e) => self
                .card
                .show_error("Couldn't restore the tray icon", &format!("{e:#}")),
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
    let req = physics_request(shot, prompt, want_difficulty);
    chain.complete_parsed(&req, |c| parse_answer(&c.text))
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
    let delay = until.duration_since(now).unwrap_or(Duration::from_millis(1));
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
    /// disarmed before `show_modal` runs, so there is nothing to do), and
    /// `WM_APP_LEARNED` (unreachable here in practice: every path that opens
    /// Settings cancels learn mode first, see `open_settings`'s call sites --
    /// but the boxed `Chord` payload is still freed rather than leaked, in
    /// case that invariant ever changes).
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
        WM_APP_HOTKEY | WM_APP_ACTIVATE | WM_APP_TRAY | WM_APP_DISMISS | WM_APP_LEARNED => {
            SettingsReentrancy::Ignore
        }
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
                // WM_APP_LEARNED is the only ignored message carrying a
                // boxed payload; free it so it doesn't leak.
                if msg == WM_APP_LEARNED {
                    drop(unsafe { Box::from_raw(lparam.0 as *mut Chord) });
                }
                return LRESULT(0);
            }
            SettingsReentrancy::DeferResult => {
                let result = unsafe {
                    *Box::from_raw(lparam.0 as *mut std::result::Result<Answer, String>)
                };
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
                    MenuChoice::Command(cmd::SET_PRIMARY)
                        | MenuChoice::Command(cmd::SET_SECONDARY)
                ) {
                    if let Some(h) = &app.hook {
                        h.cancel_learning();
                    }
                }
                match decode(id) {
                    MenuChoice::OpenAiModel(i) => app.pick_model(true, i),
                    MenuChoice::AnthropicModel(i) => app.pick_model(false, i),
                    MenuChoice::Command(cmd::ASK_NOW) => app.ask(),
                    MenuChoice::Command(cmd::COPY_LAST) => app.copy_last(),
                    MenuChoice::Command(cmd::SET_PRIMARY) => app.start_learning(HK_PRIMARY),
                    MenuChoice::Command(cmd::SET_SECONDARY) => app.start_learning(HK_SECONDARY),
                    MenuChoice::Command(cmd::EDIT_SETTINGS) => app.edit_settings(),
                    MenuChoice::Command(cmd::RELOAD) => app.reload(),
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
            let result = unsafe {
                *Box::from_raw(lparam.0 as *mut std::result::Result<Answer, String>)
            };
            app.on_result(result);
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
    use super::{settings_reentrancy_policy, SettingsReentrancy};
    use super::{WM_APP_ACTIVATE, WM_APP_RESULT};
    use crate::actions::{self, Origin};
    use crate::config::Providers;
    use crate::dismiss::WM_APP_DISMISS;
    use crate::hotkey::{WM_APP_HOTKEY, WM_APP_LEARNED};
    use crate::mode::Mode;
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
        let mut p = Providers { order: vec!["ollama".to_string()], ..Providers::default() };
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
        assert!(App::readiness_gate(Mode::Cloud, &cloud_ready_providers(), "config.toml").is_none());
    }

    #[test]
    fn readiness_gate_blocks_cloud_mode_with_only_ollama_configured() {
        // The exact scenario #192 reports: Cloud mode, only Ollama
        // configured -- Ollama's ready() is true (no key needed), but Cloud
        // mode never selects it, so the gate must still block.
        let (headline, _) = App::readiness_gate(Mode::Cloud, &ollama_only_providers(), "config.toml")
            .expect("must block: cloud mode has nothing cloud configured");
        assert_eq!(headline, "No API key: open Edit settings");
    }

    #[test]
    fn readiness_gate_passes_when_ollama_is_configured_in_local_mode() {
        assert!(App::readiness_gate(Mode::Local, &ollama_only_providers(), "config.toml").is_none());
    }

    #[test]
    fn readiness_gate_passes_when_ollama_is_configured_in_offline_mode() {
        assert!(App::readiness_gate(Mode::Offline, &ollama_only_providers(), "config.toml").is_none());
    }

    #[test]
    fn readiness_gate_blocks_local_mode_with_only_a_cloud_key() {
        // The other #192 scenario: Local mode with only a cloud key
        // configured must say Ollama is what's missing, not the generic
        // "No API key" message meant for Cloud mode.
        let (headline, detail) = App::readiness_gate(Mode::Local, &cloud_ready_providers(), "C:\\cfg\\config.toml")
            .expect("must block: local mode has no ollama configured");
        assert_eq!(headline, "Local mode needs Ollama configured");
        assert!(detail.contains("ollama"), "{detail}");
        assert!(detail.contains("C:\\cfg\\config.toml"), "{detail}");
    }

    #[test]
    fn readiness_gate_blocks_offline_mode_with_only_a_cloud_key() {
        let (headline, _) = App::readiness_gate(Mode::Offline, &cloud_ready_providers(), "config.toml")
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
        let (headline, _) = App::readiness_gate(Mode::Auto, &nothing_configured_providers(), "config.toml")
            .expect("must block: nothing is configured at all");
        assert_eq!(headline, "No provider ready: open Edit settings");
    }

    #[test]
    fn readiness_gate_blocks_cloud_mode_with_nothing_configured() {
        assert!(App::readiness_gate(Mode::Cloud, &nothing_configured_providers(), "config.toml").is_some());
    }

    #[test]
    fn readiness_gate_blocks_local_mode_with_nothing_configured() {
        assert!(App::readiness_gate(Mode::Local, &nothing_configured_providers(), "config.toml").is_some());
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
    fn settings_reentrancy_ignores_hotkey_activate_tray_dismiss_and_learned() {
        for msg in [
            WM_APP_HOTKEY,
            WM_APP_ACTIVATE,
            WM_APP_TRAY,
            WM_APP_DISMISS,
            WM_APP_LEARNED,
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
            + count_declarations(include_str!("ui/tray.rs"));

        assert_eq!(
            declared,
            ALL_WM_APP_IDS.len(),
            "found {declared} `pub const WM_APP_* = WM_APP + n;` declarations across \
             app.rs/dismiss.rs/hotkey.rs/ui/tray.rs but ALL_WM_APP_IDS lists {}; add the \
             new constant to ALL_WM_APP_IDS too",
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
