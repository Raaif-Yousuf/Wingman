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
    GetWindowLongPtrW, KillTimer, PeekMessageW, PostQuitMessage, RegisterClassExW, SetTimer,
    SetWindowLongPtrW, TranslateMessage, CW_USEDEFAULT, GWLP_USERDATA, MSG, PBT_APMRESUMEAUTOMATIC,
    PM_REMOVE, SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_DESTROY, WM_NCCREATE,
    WM_POWERBROADCAST, WM_TIMECHANGE, WM_TIMER, WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::actions;
use crate::activation::Trigger;
use crate::capture;
use crate::config::{ActiveProvider, Config, Providers};
use crate::connectors::civil_time::{CivilDate, CivilDateTime};
use crate::dismiss::{unpack_point, ClickWatcher, WM_APP_DISMISS};
use crate::executors;
use crate::hotkey::{
    apply_learned, chord_to_string, pause_binding_label, slot_name, Chord, HotkeyHook,
    LearnedVerdict, HK_PAUSE, HK_PRIMARY, HK_SECONDARY, WM_APP_HOTKEY, WM_APP_LEARNED,
    WM_APP_PALETTE_TOGGLE, WM_APP_PAUSE_TOGGLE,
};
use crate::keypress;
use crate::mode::{self, Mode};
use crate::options_keys::{self, Choice, WM_APP_OPTIONS_CHOICE};
use crate::pause::{self, PauseChoice, PauseState};
use crate::provider::ollama_admin::{self, FirstRun};
use crate::provider::{
    calendar_request, generic_action_request, parse_answer, physics_request,
    review_request_from_screen, review_request_from_text, Answer, Chain, Provider, Shot,
};
use crate::router;
use crate::ui::card::{
    Card, OptionRow, OptionsBehavior, WM_APP_CARD_COPY_DETAILS, WM_APP_CARD_OPEN_SETTINGS,
    WM_APP_PREVIEW_DECIDED,
};
use crate::ui::confirm;
use crate::ui::palette::{Palette, WM_APP_PALETTE_RUN};
use crate::ui::palette_model::{self, DispatchTarget};
use crate::ui::settings;
use crate::ui::tray::{
    cmd, decode, register_taskbar_created, MenuChoice, Tray, TrayEvent, WM_APP_TRAY,
};

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
/// the `u64` is the generation of the options card (#307) the request was
/// started for, so the handler can tell a result for a card that is gone or
/// replaced from one for the card still on screen (`App::on_router_result`).
/// The receiver takes ownership and must reconstruct the `Box` to free it. `WM_APP + 14`: `+9` through
/// `+13` are already `WM_APP_PREVIEW_DECIDED`/`WM_APP_PALETTE_TOGGLE`/
/// `WM_APP_PALETTE_RUN`/`WM_APP_REVIEW_RESULT`/`WM_APP_FORM_FILL_RESULT`.
pub const WM_APP_ROUTER_RESULT: u32 = WM_APP + 14;

/// #242: posted by `App::run_generic_action`'s worker thread when the
/// provider chain finishes for a non-built-in `actions.toml` action:
/// `lparam` is `Box::into_raw(Box::new((Action, Value, GenericResult)))`,
/// the generic-dispatch analogue of [`WM_APP_CALENDAR_RESULT`] -- the
/// `Action` is carried along because (unlike the calendar
/// flow) there is no single fixed action this result can belong to.
/// `WM_APP + 16`: `+15` is already `crate::ui::card::WM_APP_CARD_OPEN_SETTINGS`.
pub const WM_APP_GENERIC_ACTION_RESULT: u32 = WM_APP + 16;

/// #105: posted by a worker thread (`ui::send_gate::WindowSendGate`) that is
/// about to send a request off the machine while "Show me what you're
/// sending" is on. `lparam` is `Box::into_raw(Box::new(ui::send_gate::SendRequest))`;
/// the handler takes ownership and shows the preview card. `WM_APP + 18`:
/// `+17` is already `crate::ui::card::WM_APP_CARD_COPY_DETAILS`. Dropping the
/// request (as the Settings-open `Ignore` path does) cancels the send.
pub const WM_APP_SEND_PREVIEW: u32 = WM_APP + 18;

/// #551: posted by a worker thread to move the pending card to its next
/// stage ("Reading the page" to "Asking a model"). `wparam` is the run
/// number the worker was started for (`App::run_generation`), `lparam` is
/// `Box::into_raw(Box::new(PendingStage))` that the handler takes back. A
/// stage for an older run, or one arriving after a result card is showing,
/// is dropped. `WM_APP + 19`: `+18` is `WM_APP_SEND_PREVIEW`.
pub const WM_APP_PENDING_STAGE: u32 = WM_APP + 19;

/// #133: posted by the first-run probe's worker thread when it has looked for
/// a local Ollama. `lparam` is `Box::into_raw(Box::new(FirstRun))`; the
/// handler takes ownership. The probe waits on the network, so it never runs
/// on the message loop (which is also the keyboard hook's thread).
/// `WM_APP + 20`: `+19` is `WM_APP_PENDING_STAGE`.
pub const WM_APP_FIRST_RUN: u32 = WM_APP + 20;

/// #712: posted by the worker that ran a confirmed action's executor (UIA
/// walk and write, typed input, clipboard, calendar file). `lparam` is
/// `Box::into_raw(Box::new(Result<ExecDone, String>))`; the handler takes
/// ownership and shows the result card. The executors can take seconds, so
/// they never run on the message loop, which is also the keyboard hook's
/// thread. `WM_APP + 22`: `+21` is `WM_APP_MACHINE_CHECK_RESULT`.
pub const WM_APP_EXECUTOR_DONE: u32 = WM_APP + 22;

/// #328: posted by the "Check this PC for local models" worker thread when
/// its reads finish. `lparam` is
/// `Box::into_raw(Box::new(machine::MachineFacts))`; the receiver takes
/// ownership. The reads (including a loopback `/api/ps` call) run off the
/// message thread because that thread also hosts the low-level keyboard
/// hook, which Windows drops if it stalls. `WM_APP + 21`: `+20` is
/// `WM_APP_FIRST_RUN`.
pub const WM_APP_MACHINE_CHECK_RESULT: u32 = WM_APP + 21;

/// Set while a machine check is gathering, so a second click does not start
/// a second worker. Cleared by the worker just before it posts, so an
/// ignored (Settings open) or dropped result can never leave it stuck.
static MACHINE_CHECK_RUNNING: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// #567: what Ollama reported for one base URL, as (name, supports vision).
type PulledOllamaModels = (String, Vec<(String, bool)>);

/// #567: whether the tray should ask Ollama for its models at all. Only when
/// Ollama is in `providers.order` or the base URL is a loopback address, so a
/// config that points at some other host never gets a request from a menu
/// open.
fn ollama_menu_refresh_wanted(order: &[String], base_url: &str) -> bool {
    mode::should_probe_ollama(order, base_url)
        || matches!(mode::classify_host(base_url), mode::HostClass::Loopback)
}

/// #567: the models Ollama has pulled, for the tray's Ollama submenu. The
/// tray menu is built on the UI thread the moment it is right-clicked, so it
/// must never wait on Ollama: [`OllamaModelCache::refresh`] asks on a worker
/// thread and parks the answer here, and the next menu open picks it up.
/// Event-driven (a menu open, a reload, startup), never a timer.
#[derive(Default)]
struct OllamaModelCache {
    state: Arc<std::sync::Mutex<CacheState>>,
}

#[derive(Default)]
struct CacheState {
    /// The base URL a fetch is running for, if any.
    in_flight: Option<String>,
    /// The last finished answer. An unreachable server stores an empty list,
    /// so the menu falls back to the configured model only.
    result: Option<PulledOllamaModels>,
}

impl OllamaModelCache {
    /// Starts a background fetch from Ollama's `/api/tags`. The whole request,
    /// body included, is capped at 3 seconds (a global timeout, not just the
    /// connect).
    fn refresh(&self, base_url: &str) {
        let _ = self.refresh_with(base_url, |url| {
            crate::provider::ollama_admin::list_tags_with_timeout(url, Duration::from_secs(3))
                .map(|models| {
                    models
                        .iter()
                        .map(|m| {
                            (
                                m.name.clone(),
                                crate::provider::ollama_admin::vision_from_tags_entry(m),
                            )
                        })
                        .collect()
                })
                .unwrap_or_default()
        });
    }

    /// [`OllamaModelCache::refresh`] with the fetch passed in, so tests need
    /// no network. Does nothing while a fetch for the same URL is running; a
    /// different URL starts its own fetch, and only the one for the URL most
    /// recently asked for may store its answer.
    fn refresh_with(
        &self,
        base_url: &str,
        fetch: impl FnOnce(&str) -> Vec<(String, bool)> + Send + 'static,
    ) -> Option<std::thread::JoinHandle<()>> {
        {
            let Ok(mut state) = self.state.lock() else {
                return None;
            };
            if state.in_flight.as_deref() == Some(base_url) {
                return None;
            }
            state.in_flight = Some(base_url.to_string());
        }
        let url = base_url.to_string();
        let state = Arc::clone(&self.state);
        let spawned = std::thread::Builder::new()
            .name("tray-ollama-models".into())
            .spawn({
                let state = Arc::clone(&state);
                let url = url.clone();
                move || {
                    let pulled = fetch(&url);
                    if let Ok(mut state) = state.lock() {
                        if state.in_flight.as_deref() == Some(url.as_str()) {
                            state.in_flight = None;
                            state.result = Some((url, pulled));
                        }
                    }
                }
            });
        match spawned {
            Ok(handle) => Some(handle),
            Err(_) => {
                if let Ok(mut state) = state.lock() {
                    if state.in_flight.as_deref() == Some(url.as_str()) {
                        state.in_flight = None;
                    }
                }
                None
            }
        }
    }

    /// The finished list, if there is one for `base_url` (a list fetched for
    /// an address the config has since left is dropped).
    fn take_for(&self, base_url: &str) -> Option<Vec<(String, bool)>> {
        let taken = self.state.lock().ok()?.result.take()?;
        (taken.0 == base_url).then_some(taken.1)
    }
}

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
/// #723: one-shot, armed only while a run is in flight (rule 5), fires when
/// the run has outlasted `provider::worst_case_run_secs` for its chain.
const STUCK_TIMER_ID: usize = 4;

/// #307: the options card on screen, as the app side knows it. The card holds
/// the buttons; this holds what each one runs. Its `generation` must equal the
/// card's own before anything the card says is acted on.
struct OptionsSession {
    generation: u32,
    /// The action id each button runs, in order. Empty while the router works.
    ids: Vec<String>,
    /// The catalogue the router's answer is checked against and topped up from.
    /// Only held while the router works.
    catalogue: Vec<palette_model::PaletteAction>,
    /// Per button, whether the Wingman key may run it (#308, #859): decided by
    /// the action's own `effect`, never by the router's confidence. Empty while
    /// the router works.
    key_pickable: Vec<bool>,
}

/// What the Wingman key sees on the options card, from the app's session and
/// the card's own generation. A session the card is no longer showing is no
/// options card at all. Pure, so the key table is tested over every state.
fn key_phase_of(session: Option<&OptionsSession>, card_generation: Option<u32>) -> keypress::Phase {
    match (session, card_generation) {
        (Some(s), Some(g)) if s.generation == g => {
            if s.ids.is_empty() {
                keypress::Phase::Looking
            } else {
                keypress::Phase::Options {
                    first_key_pickable: s.key_pickable.first().copied().unwrap_or(false),
                }
            }
        }
        _ => keypress::Phase::Other,
    }
}

/// Whether one press of the key runs option 1 (so it is drawn as the default).
fn keypress_runs_first(key_pickable: &[bool]) -> bool {
    key_pickable.first().copied().unwrap_or(false)
}

/// #308, #859: for each shown option, whether the key may run it. An id the
/// catalogue does not know is not key-pickable.
fn option_key_pickable(
    options: &[router::RouterOption],
    catalogue: &[palette_model::PaletteAction],
) -> Vec<bool> {
    options
        .iter()
        .take(options_keys::MAX_OPTIONS)
        .map(|o| {
            catalogue
                .iter()
                .find(|a| a.id == o.intent)
                .is_some_and(|a| palette_model::key_pickable(a.effect))
        })
        .collect()
}

/// What an options-card choice message asks for, once checked against the card
/// and the session it belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
enum OptionsChoiceAction {
    /// Stale, malformed, or nothing to run yet: do nothing.
    Ignore,
    /// Esc: close the card, run nothing.
    Close,
    /// Run this action id.
    Run(String),
}

/// Decides what a `WM_APP_OPTIONS_CHOICE` (`wparam` choice, `lparam`
/// generation) does. Pure, so the stale cases are tested without a window:
/// a choice runs only when the card on screen and the session both carry the
/// generation the choice was made on, and a pick names a button that exists.
fn options_choice_action(
    wparam: usize,
    lparam: isize,
    card_generation: Option<u32>,
    session: Option<&OptionsSession>,
) -> OptionsChoiceAction {
    let generation = (lparam as u32) & options_keys::GENERATION_MASK;
    let Some(choice) = Choice::from_wparam(wparam) else {
        return OptionsChoiceAction::Ignore;
    };
    let Some(session) = session else {
        return OptionsChoiceAction::Ignore;
    };
    if card_generation != Some(generation) || session.generation != generation {
        return OptionsChoiceAction::Ignore;
    }
    match choice {
        Choice::Dismiss => OptionsChoiceAction::Close,
        Choice::Pick(n) => match session.ids.get(n.saturating_sub(1)) {
            Some(id) => OptionsChoiceAction::Run(id.clone()),
            // No buttons yet: the router is still working.
            None => OptionsChoiceAction::Ignore,
        },
    }
}

/// The keyboard layout (`HKL`) of the thread that owns the foreground window,
/// as an integer; `0` when there is no foreground window (not a CJK layout, so
/// the claim stays as configured).
fn foreground_keyboard_layout() -> usize {
    use windows::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayout;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return 0;
    }
    let thread = unsafe { GetWindowThreadProcessId(fg, None) };
    unsafe { GetKeyboardLayout(thread) }.0 as usize
}

/// The foreground window as an integer (`0` = none) and whether it belongs to
/// this process (the card, the palette, Settings): the claim cannot follow
/// either of those.
fn foreground_window_and_ownership() -> (isize, bool) {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return (0, false);
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut pid)) };
    (fg.0 as isize, pid == unsafe { GetCurrentProcessId() })
}

/// Whether a choice leaves the card up with its claim spent, so the claim must
/// be put back. A choice that closes the card or runs an action ends the card
/// and needs nothing.
fn rearm_after(decided: &OptionsChoiceAction) -> bool {
    matches!(decided, OptionsChoiceAction::Ignore)
}

/// #307: the card's buttons for a ranked shortlist: the action id each runs and
/// the label it shows.
fn option_rows(options: &[router::RouterOption]) -> Vec<OptionRow> {
    options
        .iter()
        .take(options_keys::MAX_OPTIONS)
        .map(|o| OptionRow {
            id: o.intent.clone(),
            label: o.label.clone(),
        })
        .collect()
}

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
    /// #339: whether a password manager was in front before Wingman's own UI
    /// (the tray menu, the palette) took the foreground. Set only around a
    /// dispatch from that UI and cleared right after, so it never leaks into
    /// a later hotkey press. `refuse_if_password_manager` honours it as well
    /// as the live check.
    password_manager_hint: bool,
    /// #567: the pulled Ollama models, refreshed off the UI thread.
    ollama_models: OllamaModelCache,
    /// #339: the picker's own answer, taken in `toggle_palette` before the
    /// palette showed. Read only when a picker (the palette, the options card)
    /// runs an action, so a stale value from an earlier session can never
    /// apply: every open overwrites it, and the options card clears it.
    palette_password_manager: bool,
    /// #307: the options card on screen, if any. Checked against the card's
    /// own generation before anything it says is acted on.
    options: Option<OptionsSession>,
    /// #307: the generation of the latest options card, bumped on every show.
    options_generation: u32,
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

    /// Issue #225: the one action awaiting a preview decision, if any.
    ///
    /// This was two independent `Option`s (`pending_review`,
    /// `pending_form_fill`) that "could never both be `Some`, because only
    /// one preview is on screen at a time". They could. `Card::hide` tore a
    /// preview down without reporting a decision, so a slot outlived its
    /// preview, and `on_preview_decided` checked review before form-fill, so
    /// pressing "Do it" on a form-fill preview ran the abandoned email
    /// replace-text instead and dropped the confirmed fill with no card.
    ///
    /// One slot holding a kind makes that unrepresentable rather than
    /// re-checked: showing a second preview overwrites the first, and the
    /// compiler finds every site that has to handle a new kind. `None` means
    /// the calendar flow, which needs no carried context.
    pending_preview: Option<PendingPreview>,
    /// Issue #225: the generation of the preview whose decision this `App`
    /// is still waiting for. `WM_APP_PREVIEW_DECIDED` carries a generation
    /// in its `WPARAM`; anything that does not match is a late-delivered
    /// abandonment for a preview that has already been replaced, and acting
    /// on it would clear the state belonging to the preview now on screen.
    pending_preview_generation: u32,
    /// #551: counts runs (bumped each time `busy` turns on). A worker's
    /// `WM_APP_PENDING_STAGE` carries the number it started under, so a late
    /// stage cannot relabel the card of a newer run.
    run_generation: u64,
    /// #133: the number of the latest first-run probe. A probe's answer
    /// carries the number it started under; an older one is ignored, so two
    /// quick presses cannot make the first answer clobber the second's card.
    first_run_probe: u64,
    /// #228: the footer line for the answer now being shown, set by each
    /// action result handler from the worker's attribution and `None` unless
    /// a fallback answered.
    fallback_footer: Option<String>,
    /// #40: the `Undo` the most recent successful `fill_form` run returned,
    /// so "Restore last form" (tray) can put its fields back. `FnOnce`
    /// (`executors::Undo::undo` consumes it), so this is `take()`n on use --
    /// a second "Restore" click after a successful restore reports "nothing
    /// to restore" rather than attempting a stale undo twice.
    last_form_undo: Option<executors::Undo>,
    /// #712: an executor worker is writing into another window. Quit waits
    /// for it, so a write is never killed half way.
    exec_in_flight: bool,
    /// Review of #349/#426: the full (redacted) chain behind the most
    /// recent error card, so "use Copy diagnostics for details" points at
    /// something real instead of nothing -- `human_error_detail` on its own
    /// discards the raw chain entirely. In memory only, never written to
    /// disk (Hard Rule 5 and privacy): overwritten by the next error,
    /// cleared on nothing else, and never itself the source of a card's
    /// display text (`track_error` returns the human sentence separately).
    last_error: Option<LastError>,
}

/// See `App::last_error`'s doc comment.
struct LastError {
    /// Whatever headline the card that reported this error also used, e.g.
    /// "Couldn't add the event" -- not a separate taxonomy to keep in sync.
    action: String,
    occurred_at: SystemTime,
    /// Already scrubbed (`diagnostics::scrub_error_chain`: key tokens, the
    /// text redaction and the account name, #253 and #733) -- this is the
    /// only place the non-humanized chain survives at all.
    chain: String,
}

/// Issue #425: the exact text "Copy details" puts on the clipboard.
/// `last_error.chain` is already scrubbed (#253, #733) at the time it is
/// stored -- this is a plain accessor, not a second pass -- but it
/// is factored out (rather than inlined in `copy_error_details`) so the
/// "the redacted chain, and only the redacted chain, is what reaches the
/// clipboard" guarantee is unit-tested directly, without touching the real
/// OS clipboard.
fn error_details_clipboard_text(last_error: &LastError) -> &str {
    &last_error.chain
}

pub fn run() -> Result<()> {
    // #700: the thread that runs this hosts the keyboard hook, and a `--settings`
    // launch runs the Settings loop before `pump_messages`, so mark it first.
    crate::ui_thread::mark_current_thread();
    // Before any window exists, so the card's metrics are right on a mixed-DPI
    // setup (a laptop panel next to an external monitor).
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // #237: the owner window comes BEFORE the single-instance name. Then
    // "the name is held" implies "a window of our class exists", because
    // whoever holds the name created its window first, and a duplicate that
    // finds no window knows the holder is not Wingman.
    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)?.into() };
    let mut hwnd = create_owner_window(instance)?;

    // Claim the single-instance name before anything else is created. A
    // duplicate must not register a tray icon or install a keyboard hook,
    // because both would double every keypress -- and the second API call is
    // billed just like the first.
    let _instance = match crate::single_instance::acquire() {
        crate::single_instance::Instance::First(lock) => Some(lock),
        crate::single_instance::Instance::Already => {
            // A duplicate's own window has no App behind it, so it goes
            // before anyone looks for "the" owner window.
            unsafe {
                let _ = DestroyWindow(hwnd);
            }
            // The shell launches this app by AUMID with no way to pass
            // arguments, so the Copilot key always arrives here bare; only a
            // launch explicitly carrying `--settings` should open Settings
            // instead of asking.
            let activation = crate::single_instance::activation_from_args(std::env::args());
            match crate::single_instance::settle_duplicate(activation) {
                crate::single_instance::Startup::Delivered => return Ok(()),
                // The old holder exited while we waited: a normal start.
                crate::single_instance::Startup::Locked(lock) => {
                    hwnd = create_owner_window(instance)?;
                    Some(lock)
                }
                // Decision: a process squatting the name must not stop the
                // app from starting. No instance answered, so start without
                // the lock and say so (shown on a card below).
                crate::single_instance::Startup::Squatted => {
                    crate::single_instance::mark_lock_squatted();
                    hwnd = create_owner_window(instance)?;
                    None
                }
            }
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

    // #769: delete old calendar files once at startup, on a short-lived
    // thread (one directory read), so a lone event's file does not wait for
    // the next "Add to calendar". No timer: nothing runs while idle.
    let _ = std::thread::Builder::new()
        .name("ics-startup-sweep".into())
        .spawn(|| crate::connectors::IcsConnector::new().sweep_stale());

    // #831: read before `Config::load`, which creates the file on a first
    // run. If the path cannot be found, say it existed: no welcome is the
    // safe side.
    let config_existed = Config::path().map(|p| p.exists()).unwrap_or(true);
    let config = Config::load().unwrap_or_default();
    // Issue #19: publish the loaded mode to the process-wide atomic BEFORE
    // anything below can start a request (the hotkey hook isn't installed
    // yet, but `--settings`'s `open_settings` path below can still save,
    // and `provider::common`'s Offline guard must be correctly configured
    // from the very first request, not just from the first tray click).
    mode::set_current(config.mode);
    // #105: same reason as the mode above. `provider::common`'s send
    // guard reads this process-wide mirror, not `Config`, so it has to
    // be published before anything can reach the HTTP boundary. Without
    // this line the toggle is permanently off and the preview never
    // appears, whatever config.toml says.
    crate::config::set_egress_preview_enabled(config.egress_preview.enabled);
    // #505: same reason: text is redacted at the HTTP boundary, which has no
    // `Config` in reach.
    crate::config::set_redact_text_enabled(config.capture.redact);
    // #101: same reason: the send guard has no `Config` in reach.
    crate::usage::set_daily_budget(config.budget.daily_usd, config.budget.warn_percent);
    // #111: `ocr::recognize` runs on worker threads with no Config in reach.
    crate::ocr::set_preferred_language(&config.capture.ocr_language);
    let chain = Arc::new(config.build_chain());

    // #105: every `Chain` built from here on asks the owner window to show
    // the send preview before a request leaves the machine (when the toggle
    // is on). Installed before anything can build a chain.
    crate::provider::install_send_gate(Arc::new(crate::ui::send_gate::WindowSendGate::new(
        hwnd.0 as isize,
    )));

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
    card.set_placement(config.ui.card_corner, &config.ui.card_monitor);
    // Issue #39: so the preview card can PostMessageW WM_APP_PREVIEW_DECIDED
    // here when "Do it"/Cancel closes it -- see Card::set_owner's doc
    // comment.
    card.set_owner(hwnd);
    // #175: a stored key that could not be read is never silently dropped
    // (rule 7) -- the card is the first thing to exist that can show it.
    // #237 shares the card: a lockless start must be visible too (rule 7).
    if let Some((headline, detail)) = startup_card(
        &config.unreadable_secrets,
        crate::single_instance::lock_squatted(),
        config.load_problem.as_deref(),
    ) {
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
        password_manager_hint: false,
        ollama_models: OllamaModelCache::default(),
        palette_password_manager: false,
        options: None,
        options_generation: 0,
        busy: false,
        last: None,
        pause: PauseState::Running,
        pending_conflict: None,
        pending_preview: None,
        pending_preview_generation: 0,
        run_generation: 0,
        first_run_probe: 0,
        fallback_footer: None,
        last_form_undo: None,
        exec_in_flight: false,
        last_error: None,
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
        Err(e) => {
            let detail = app.track_error("Hotkeys unavailable", &format!("{e:#}"));
            app.card.show_error_with_details(
                "Hotkeys unavailable",
                &format!("{detail}\n\nUse Ask now from the tray menu instead."),
            );
        }
    }

    // Issue #227: the last run ended in a panic. Shown after the hotkey
    // install so its error card cannot replace this one. No model call, no dialog:
    // a card, and the note rides along as the last error so "Copy details"
    // and "Copy diagnostics" include it. Read once, then the note is gone.
    if let Some(note) = crate::crash_note::note_path()
        .ok()
        .and_then(|p| crate::crash_note::take_note(&p))
    {
        let (headline, detail) = crate::crash_note::card_text(&note);
        // A startup error recorded above (e.g. hotkeys unavailable) is kept
        // after the note, so "Copy details" carries both.
        let chain = match app.last_error.take() {
            Some(earlier) => format!(
                "{note}
{}",
                earlier.chain
            ),
            None => note,
        };
        app.last_error = Some(LastError {
            action: headline.clone(),
            occurred_at: SystemTime::now(),
            // #733: the note is text from a previous run, scrubbed like any
            // other error before it can be pasted into a bug report.
            chain: crate::diagnostics::scrub_error_chain_here(&chain),
        });
        app.card.show_error_with_details(&headline, &detail);
    }

    // #831: after every startup problem card above, so it never replaces one.
    // No model call: this is text only.
    let opens_settings = crate::single_instance::first_launch_action(std::env::args())
        == crate::single_instance::FirstLaunchAction::OpenSettings;
    let other_card = !matches!(app.card.state(), crate::ui::card::CardState::Hidden);
    let welcome = match (Config::path(), Config::old_path()) {
        (Ok(path), Ok(old)) => {
            should_show_welcome(&path, &old, config_existed, opens_settings, other_card)
        }
        _ => false,
    };
    if welcome {
        let (headline, detail) = welcome_card(&chord_to_string(&app.config.hotkeys.secondary));
        app.card.show_settings_needed(&headline, &detail);
    }

    // Non-fatal: without it the card still auto-dismisses on its timer.
    app.watcher = ClickWatcher::install(hwnd).ok();

    // Hand the App to the window proc. It stays alive until WM_DESTROY.
    unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, Box::into_raw(app) as isize) };
    // Only now can this window act on an activation; a duplicate looking for
    // "the running instance" ignores any window without this mark (#237).
    crate::single_instance::mark_ready(hwnd);

    pump_messages();
    Ok(())
}

/// Card text for issue #175: one or more stored provider keys exist in
/// Credential Manager but could not be read back on this load. Pure so the
/// wording is unit-tested without a real `Card`/HWND (AGENTS.md rule 8);
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

/// The one card shown at startup, or `None`. Both startup problems (#175
/// unreadable keys, #237 lockless start) end in a card, and one must never
/// overwrite the other, so when both apply their text is joined. Pure so the
/// wording is unit-tested without a `Card`. No em dash (rule 11).
fn startup_card(
    unreadable: &[String],
    lock_squatted: bool,
    config_problem: Option<&str>,
) -> Option<(String, String)> {
    let squat = (
        "Another program holds Wingman's startup lock".to_string(),
        "Wingman started anyway. If you see two Wingman tray icons, quit one of them.".to_string(),
    );
    // Every problem that applies, in order; the first one's headline leads
    // and the rest are joined, so none is dropped.
    let mut cards = Vec::new();
    if let Some(problem) = config_problem {
        cards.push(config_problem_card(problem));
    }
    if !unreadable.is_empty() {
        cards.push(unreadable_secrets_card(unreadable));
    }
    if lock_squatted {
        cards.push(squat);
    }
    let mut cards = cards.into_iter();
    let (headline, mut detail) = cards.next()?;
    for (h, d) in cards {
        detail = format!("{detail}\n\n{h}. {d}");
    }
    Some((headline, detail))
}

/// #831: whether to show the welcome card. A bare launch must not ask a
/// model, and the login autostart is a bare launch too, so the only "first
/// time" signal is the config file: absent before this start's
/// `Config::load` AND present after it (this start created it, which is what
/// remembers it). A folder that cannot be written leaves no file, so the card
/// never repeats (#879). A leftover pre-rename `copilot-ask` config means a
/// returning user whose migration did not finish. Also skipped when
/// `--settings` is already opening Settings or a startup problem card shows.
/// It reads only paths and flags, so a check that cannot run shows no card.
fn should_show_welcome(
    config_path: &std::path::Path,
    old_path: &std::path::Path,
    existed_before: bool,
    opens_settings: bool,
    other_card: bool,
) -> bool {
    !existed_before && config_path.exists() && !old_path.exists() && !opens_settings && !other_card
}

/// #831: the first-launch welcome card. `secondary` is the display form of
/// the second hotkey. Shown through the settings-needed card, so a click
/// opens Settings. No em dash (rule 11).
pub(crate) fn welcome_card(secondary: &str) -> (String, String) {
    (
        "Wingman is running".to_string(),
        format!(
            "Press the Copilot key or {secondary}. The icon may be under the ^ arrow in the tray; \
             drag it out to keep it in view. Click here to open Settings."
        ),
    )
}

/// #793: whether `mode` may be switched on while the config file's state is
/// `load_problem`. A file that did not parse starts the run Offline with
/// default (looser) settings; anything that sends data out must wait until it
/// parses, so only Offline is allowed meanwhile. The state is a plain field
/// read, so the check cannot itself fail open.
fn mode_allowed(mode: Mode, load_problem: Option<&str>) -> bool {
    mode == Mode::Offline || load_problem.is_none()
}

/// #892: [`config_problem_card`] with where the file is, for the cards that
/// replace a setup card: the user must be told which file to open.
fn broken_file_card(problem: &str, config_path: &str) -> (String, String) {
    let (headline, detail) = config_problem_card(problem);
    (
        headline,
        format!("{detail}\n\nThe file is here (tray menu, Open config.toml):\n{config_path}"),
    )
}

/// #892: [`App::readiness_gate`], except that when config.toml did not parse
/// the blocked card names the file instead of the mode's setup text.
fn setup_gate(
    mode: Mode,
    providers: &Providers,
    config_path: &str,
    load_problem: Option<&str>,
) -> Option<(String, String)> {
    let blocked = App::readiness_gate(mode, providers, config_path)?;
    Some(match load_problem {
        Some(problem) => broken_file_card(problem, config_path),
        None => blocked,
    })
}

/// #890: applies a Settings save all or nothing. `save` runs on the edited
/// copy first; only when it succeeds does the edit replace `current`, so a
/// refused save leaves the running config exactly as it was.
fn commit_edited_config(
    current: &mut Config,
    mut edited: Config,
    save: impl FnOnce(&Config) -> Result<()>,
) -> Result<()> {
    // #793: the save is refused while the file does not parse, so an edited
    // mode must not outlive it in memory (requests read it).
    let problem = edited
        .load_problem
        .clone()
        .or_else(|| current.load_problem.clone());
    if !mode_allowed(edited.mode, problem.as_deref()) {
        edited.mode = Mode::Offline;
    }
    save(&edited)?;
    *current = edited;
    Ok(())
}

/// #890: the card text for a Settings save that was refused. The edits are
/// dropped (a key typed there is not stored either), and it says so.
fn refused_save_detail(human: &str) -> String {
    format!(
        "{human}\n\nYour changes were not applied and Wingman is still using its previous \
         settings. A key you typed was not stored, so enter it again after fixing this."
    )
}

/// #711: config.toml exists but did not parse. Names the line and column
/// (`problem` already carries them), never file text. No em dash (rule 11).
fn config_problem_card(problem: &str) -> (String, String) {
    (
        "Settings file not read".to_string(),
        format!(
            "{problem}. Wingman is using default settings in Offline mode (nothing leaves this \
             computer) and will not save over your file. Fix it, then choose Reload settings \
             from the tray menu."
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
    // A duplicate that destroyed its window and starts anyway registers the
    // class a second time in the same process; that is not a failure.
    if unsafe { RegisterClassExW(&class) } == 0
        && unsafe { windows::Win32::Foundation::GetLastError() }
            != windows::Win32::Foundation::ERROR_CLASS_ALREADY_EXISTS
    {
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
    // #700: this thread hosts the keyboard hook, so no blocking network call may run here.
    crate::ui_thread::mark_current_thread();
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

/// #132: what `App::begin_action` does beyond the pause/busy checks. A run
/// that will not use a model (no provider ready, "Fill this form" only) must
/// not require one, must not grab the screen (the form is read from the UI
/// tree, so a capture failure would block a purely local fill for nothing),
/// and must not claim on the card that a model is being asked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct BeginPlan {
    require_provider: bool,
    capture: bool,
    asks_model: bool,
}

impl BeginPlan {
    /// Every model-backed flow.
    const MODEL: BeginPlan = BeginPlan {
        require_provider: true,
        capture: true,
        asks_model: true,
    };

    fn for_fill_form(use_model: bool) -> BeginPlan {
        BeginPlan {
            require_provider: use_model,
            capture: use_model,
            asks_model: use_model,
        }
    }
}

/// #133: whether a probe's answer belongs to the latest probe. An older
/// one (an earlier press) is dropped.
fn first_run_result_is_current(run: u64, latest: u64) -> bool {
    run == latest
}

/// #133: after a probe whose answer needs no card, whether to hide the card
/// on screen. Only the working card this probe put up: a card for something
/// else (an answer, or a run in flight) is left alone.
fn should_hide_after_first_run(card_is_pending: bool, busy: bool) -> bool {
    card_is_pending && !busy
}

/// #133: the card shown once a local Ollama has been adopted. A stock tray
/// listener can be CPU only (AGENTS.md pitfall), so that case says answers may
/// be slow and where to look.
fn first_run_adopted_card(
    model: &str,
    kind: crate::provider::ollama_admin::ListenerKind,
) -> (String, String) {
    let mut detail =
        format!("Wingman will answer with {model}, which runs on this PC. Press again to ask.");
    if kind == crate::provider::ollama_admin::ListenerKind::StockTrayServer {
        detail.push_str(
            " This is the stock Ollama app, which can run on the CPU only, so answers may be slow. Settings shows what it is using.",
        );
    }
    ("Using Ollama on this PC".to_string(), detail)
}

/// #892: the detail the first-run card shows. The probe's Ollama text is only
/// for a setup that is merely empty; when config.toml did not parse, the card
/// is the broken-file one and must keep saying so.
fn first_run_card_detail(
    gate_detail: String,
    found: &FirstRun,
    load_problem: Option<&str>,
) -> String {
    if load_problem.is_some() {
        return gate_detail;
    }
    first_run_detail(found).unwrap_or(gate_detail)
}

/// #133: the readiness card's detail when Ollama is running but has nothing
/// that can read the screen; `None` leaves the usual card alone. The card
/// already opens Settings, where the model can be downloaded.
fn first_run_detail(found: &FirstRun) -> Option<String> {
    match found {
        FirstRun::NoVisionModel => Some(
            "Ollama is running, but none of its models can read the screen. Click here to open Settings and download gemma3:4b, or paste an API key."
                .to_string(),
        ),
        FirstRun::VisionUnknown => Some(
            "Ollama is running, but Wingman could not tell which of its models can read the screen. Click here to open Settings and download gemma3:4b, or paste an API key."
                .to_string(),
        ),
        FirstRun::NotRunning | FirstRun::Found { .. } => None,
    }
}

/// #723: what a press that arrives while a request is still running does to
/// the card (rule 7: it must never end in nothing).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BusyPress {
    /// A card already explains the wait, or is waiting on the user (a send
    /// preview): leave it.
    Leave,
    /// Nothing explains the wait: show the working card again.
    ShowStillWorking,
}

fn busy_press_card(card: crate::ui::card::CardState) -> BusyPress {
    use crate::ui::card::CardState;
    match card {
        // Never cover a card the user may be reading, or one waiting on them.
        CardState::Pending
        | CardState::Preview
        | CardState::Collapsed
        | CardState::Expanded
        | CardState::Options => BusyPress::Leave,
        CardState::Hidden => BusyPress::ShowStillWorking,
    }
}

/// #723: whether a press still finds the app busy. A run flagged stuck is
/// abandoned by the press, so the press can start a new one.
fn busy_after_press(busy: bool, stuck: bool) -> bool {
    busy && !stuck
}

/// #723: whether a worker result belongs to the run now in flight. `0` is a
/// result posted without a generation, which is always kept.
fn result_is_current(posted_run: usize, current_run: u64) -> bool {
    posted_run == 0 || posted_run as u64 == current_run
}

/// #723: the messages carrying a worker's result for the run in flight.
fn is_run_result(msg: u32) -> bool {
    matches!(
        msg,
        WM_APP_RESULT
            | WM_APP_CALENDAR_RESULT
            | WM_APP_REVIEW_RESULT
            | WM_APP_FORM_FILL_RESULT
            | WM_APP_GENERIC_ACTION_RESULT
            // The worker is blocked on this preview; a stale one is freed,
            // which drops its reply channel and cancels that send.
            | WM_APP_SEND_PREVIEW
    )
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
    /// #723: whether a run is in flight for the purposes of a press. A run
    /// the stuck timer already gave up on is abandoned here, so the press
    /// starts over; its late result is dropped as stale.
    fn busy_now(&mut self) -> bool {
        let stuck = BUSY_STUCK.with(|c| c.get()) && !self.exec_in_flight;
        if busy_after_press(self.busy, stuck) != self.busy {
            self.set_busy(false);
            // Advance now, not only when the next run starts: if this press
            // returns early (paused, a readiness card, ...) the abandoned
            // run's late result must still be stale.
            self.run_generation = self.run_generation.wrapping_add(1);
            CURRENT_RUN.with(|c| c.set(self.run_generation));
        }
        self.busy
    }

    /// #723: the stuck timer fired. Says so on a card; the next press
    /// starts over.
    fn on_busy_stuck(&mut self) {
        let hwnd = self.hwnd();
        unsafe {
            let _ = KillTimer(Some(hwnd), STUCK_TIMER_ID);
        }
        // An executor is a confirmed write that is really happening: it is
        // never called stuck, and its done message is never treated as stale.
        if !self.busy || self.exec_in_flight {
            return;
        }
        BUSY_STUCK.with(|c| c.set(true));
        // A send preview is waiting on the user: leave it. The next press
        // still abandons the run.
        if matches!(self.card.state(), crate::ui::card::CardState::Preview) {
            return;
        }
        self.card
            .show_error("This is taking too long", "Press again to start over.");
    }

    /// #723: a press while a request is running. Puts the working card back
    /// if the user cannot see one, so the key never seems dead.
    fn show_busy_card(&mut self) {
        if busy_press_card(self.card.state()) == BusyPress::ShowStillWorking {
            self.card
                .show_pending(crate::ui::card::PendingStage::StillWorking);
        }
    }

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
                "No AI model set up yet".to_string(),
                "Click here to open Settings and paste an API key, or install Ollama to run models on this PC for free.".to_string(),
            ),
            Mode::Auto => (
                "No AI model set up yet".to_string(),
                "Click here to open Settings and paste an API key, or install Ollama to run models on this PC for free.".to_string(),
            ),
        })
    }

    /// #133: on the press that hit the readiness gate with nothing set up,
    /// looks for a local Ollama with a vision model. The look waits on the
    /// network, so it runs on a worker thread and posts [`WM_APP_FIRST_RUN`];
    /// this only shows the pending card and starts it. Nothing is cached: a
    /// press re-runs the probe, which costs the worker a moment, never the
    /// message loop. Returns `false` (nothing started) when this setup is not
    /// eligible: a key, a chosen provider or Cloud mode.
    fn start_first_run_probe(&mut self) -> bool {
        if !self
            .config
            .providers
            .first_run_local_eligible(self.config.mode)
        {
            return false;
        }
        self.card
            .show_pending(crate::ui::card::PendingStage::Working);
        self.first_run_probe = self.first_run_probe.wrapping_add(1);
        let run = self.first_run_probe;
        let target = self.hwnd_isize();
        let base_url = self.config.providers.ollama.base_url.clone();
        let preferred = self.config.providers.ollama.model.clone();
        std::thread::spawn(move || {
            // A panic must still post a result, or the Working card never
            // goes away (dev builds; release aborts). Read as "not found".
            let found = catch_worker(|| Ok(ollama_admin::probe_first_run(&base_url, &preferred)))
                .unwrap_or(FirstRun::NotRunning);
            let payload = Box::into_raw(Box::new(found));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_FIRST_RUN,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_FIRST_RUN, LPARAM(payload as isize));
                }
            }
        });
        true
    }

    /// #133: the probe's answer. A found model becomes the setup (saved,
    /// chain rebuilt) and the card says so; the next press asks it. Anything
    /// else shows the readiness card, with a sharper detail when Ollama runs
    /// but has no vision model. If the setup changed while the probe ran
    /// (a key was pasted), there is nothing to say.
    fn on_first_run_result(&mut self, run: u64, found: FirstRun) {
        if !first_run_result_is_current(run, self.first_run_probe) {
            return;
        }
        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "config.toml".into());
        let Some((headline, mut detail)) = setup_gate(
            self.config.mode,
            &self.config.providers,
            &path,
            self.config.load_problem.as_deref(),
        ) else {
            let pending = matches!(self.card.state(), crate::ui::card::CardState::Pending);
            if should_hide_after_first_run(pending, self.busy) {
                self.card.hide();
            }
            return;
        };
        let eligible = self
            .config
            .providers
            .first_run_local_eligible(self.config.mode);
        if let (FirstRun::Found { model, kind }, true) = (&found, eligible) {
            let base_url = ollama_admin::first_run_base_url(&self.config.providers.ollama.base_url)
                .unwrap_or_else(|| self.config.providers.ollama.base_url.clone());
            self.config.providers.adopt_local_model(model, &base_url);
            self.chain = Arc::new(self.config.build_chain());
            let saved = self.config.save();
            self.refresh_tray_labels();
            let (title, text) = first_run_adopted_card(model, *kind);
            match saved {
                Ok(()) => self.card.show_answer(&title, &text, 8, None),
                Err(e) => {
                    let headline = format!("{title}: not saved");
                    let human = self.track_error(&headline, &format!("{e:#}"));
                    self.card.show_error_with_details(
                        &headline,
                        &format!("It will revert when you quit.\n\n{human}"),
                    );
                }
            }
            self.set_watch(true);
            return;
        }
        detail = first_run_card_detail(detail, &found, self.config.load_problem.as_deref());
        self.show_setup_card(&headline, &detail);
    }

    /// #806: the first press that would ask an Ollama that is not on this PC
    /// shows a card saying where the screenshot goes, instead of sending it.
    /// Returns `true` when it did (the caller must stop). The address is
    /// remembered, so the next press goes through; 127.0.0.1 never shows it.
    fn show_remote_ollama_notice(&mut self) -> bool {
        let selected = self
            .config
            .providers
            .selected_order_for_mode(self.config.mode, true);
        if !crate::remote_ollama::notice_applies(self.config.mode, &selected) {
            return false;
        }
        let Some((headline, detail)) =
            crate::remote_ollama::take_notice(&self.config.providers.ollama.base_url)
        else {
            return false;
        };
        self.show_setup_card(&headline, &detail);
        true
    }

    /// #339: a password manager is in front now, or was when Wingman's own
    /// UI (tray menu, palette) took the foreground.
    fn password_manager_now(&self) -> bool {
        self.password_manager_hint
            || crate::password_managers::foreground_is_password_manager(
                &self.config.capture.extra_password_managers,
            )
    }

    /// #339: when a password manager is the foreground window, shows the
    /// "does not read password managers" card and returns `true`; the caller
    /// must then return without capturing, reading or asking anything. Every
    /// main-thread entry point that gathers input calls this first, which
    /// `password_managers`' source-scanning tests enforce.
    fn refuse_if_password_manager(&mut self) -> bool {
        if !self.password_manager_now() {
            return false;
        }
        self.card.show_answer(
            crate::password_managers::REFUSAL_HEADLINE,
            crate::password_managers::REFUSAL_DETAIL,
            3,
            None,
        );
        true
    }

    /// Issue #214: the pause/busy/hide-stale-card/readiness/capture pipeline
    /// shared by every action that shows a pending card, captures the
    /// screen, and hands off to a worker thread which posts back one of the
    /// `WM_APP_*` result messages. Before this existed, `ask()`,
    /// `add_event_from_screen()` and `review_this_email()` each duplicated
    /// roughly the same 40 lines almost verbatim (#214's own body: "the
    /// overnight task instructions asked for app.rs edits to stay additive
    /// and minimal since another agent was editing it concurrently"). A
    /// fill-form-from-screen action (#40) goes through `begin_action`, the
    /// same pipeline with a [`BeginPlan`] that lets it skip the readiness
    /// gate and the capture when no model will be used (#132).
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
        let (raw, foreground_hwnd, value) = self.begin_action(BeginPlan::MODEL, extra)?;
        Some((raw?, foreground_hwnd, value))
    }

    /// [`Self::begin_model_action`] for an action with a model-free path
    /// (#132, "Fill this form"): `plan` says whether the readiness gate, the
    /// screen capture and the "asking a model" card apply. The returned
    /// `RawShot` is `None` exactly when `plan.capture` is false.
    fn begin_action<T>(
        &mut self,
        plan: BeginPlan,
        extra: impl FnOnce(&mut Self) -> Option<T>,
    ) -> Option<(Option<capture::RawShot>, isize, T)> {
        match model_action_gate(self.busy_now(), pause::is_paused_now()) {
            ModelActionGate::Busy => {
                self.show_busy_card();
                return None;
            }
            ModelActionGate::Paused => {
                self.card
                    .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
                return None;
            }
            ModelActionGate::Proceed => {}
        }
        actions::queue_skipped_from_disk();

        // #339: before the card is touched and before anything is read.
        if self.refuse_if_password_manager() {
            return None;
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
        let blocked = setup_gate(
            self.config.mode,
            &self.config.providers,
            &path,
            self.config.load_problem.as_deref(),
        )
        .filter(|_| plan.require_provider);
        if let Some((headline, detail)) = blocked {
            // #133: nothing is set up. Look for a local Ollama on a worker
            // (never here: this is the keyboard hook's thread), once per
            // press that reaches this card, never on a bare launch.
            if self.start_first_run_probe() {
                return None;
            }
            self.show_setup_card(&headline, &detail);
            return None;
        }

        // #806: before anything is captured or sent.
        if plan.asks_model && self.show_remote_ollama_notice() {
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
        let raw = if plan.capture {
            let optimistic_chain = self
                .config
                .providers
                .build_chain_for_mode(self.config.mode, true);
            let image_limits = optimistic_chain
                .first_ready_caps()
                .and_then(|caps| caps.image_limits);
            let (max_long_edge, max_pixels) =
                capture::resolve_limits(image_limits, self.config.capture.max_edge);
            let lookup = capture::LookupWindow::when(
                capture::needs_password_lookup(
                    self.config.capture.redact,
                    &self.config.providers,
                    self.config.mode,
                ),
                capture::LookupWindow::Foreground,
            );
            match capture::grab_raw_for(
                &self.config.capture.monitor,
                max_long_edge,
                max_pixels,
                lookup,
                &self.config.capture.extra_password_managers,
            ) {
                Ok(r) => Some(r),
                Err(e) => {
                    let detail = self.track_error("Couldn't capture the screen", &format!("{e:#}"));
                    self.card
                        .show_error_with_details("Couldn't capture the screen", &detail);
                    return None;
                }
            }
        } else {
            None
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

        self.set_busy(true);
        // Disarmed for the whole in-flight window: a click while the spinner
        // is up must not touch the card.
        self.set_watch(false);
        // Issue #354 follow-up review findings 1 and 2: every model-backed
        // flow goes through this shared method, so the pending card starts
        // directly at `AskingModel` here -- once, for all three callers --
        // instead of each flow (or just `ask()`, before this fix) setting it
        // separately after the fact. The label is resolved from the SAME
        // mode-aware chain the worker thread will actually try (finding 4),
        // known synchronously here with no network call, so this is not a
        // promise the worker reaches that exact provider (it may fail over)
        // but an honest "what Wingman is trying right now" cue, same as the
        // pre-existing reasoning this replaces.
        let stage = if plan.asks_model {
            let model_name = self.first_provider_model_label().unwrap_or_default();
            crate::ui::card::PendingStage::AskingModel { model_name }
        } else {
            crate::ui::card::PendingStage::ReadingForm
        };
        self.card.show_pending(stage);

        Some((raw, foreground_hwnd_isize, extra_value))
    }

    /// #620: the palette's own card for skipped actions, once per change.
    fn show_skipped_notice(&mut self, report: &actions::LoadReport) {
        if let Some(notice) = actions::observe_skipped(&report.skipped) {
            let headline = "Some actions in actions.toml were skipped";
            let detail = self.track_error(headline, &notice);
            self.card.show_error_with_details(headline, &detail);
        }
    }

    /// The whole flow: hide any stale card, check a provider is actually
    /// ready, grab the screen, then hand the bytes to a worker so the
    /// message loop stays responsive during the call.
    fn ask(&mut self, trigger: Trigger) {
        let Some((mut raw, foreground_hwnd_isize, ())) = self.begin_model_action(|_| Some(()))
        else {
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
        let target = self.hwnd_isize();
        let run = self.run_generation;
        let redact = self.config.capture.redact;

        // Issue #354 follow-up review: the pending card already started at
        // `AskingModel` inside `begin_model_action` (findings 1 and 2), so
        // there is nothing left to set here.
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            // #236: every egress row this thread writes names its trigger.
            crate::activation::set_worker_trigger(trigger);
            let result: std::result::Result<Answer, String> = catch_worker(|| {
                (|| -> Result<Answer> {
                    let shot = encode_for_upload(&mut raw, &providers, mode, redact)?;
                    worker(
                        &providers,
                        mode,
                        &shot,
                        &raw,
                        foreground_hwnd_isize,
                        &prompt,
                    )
                })()
                .map_err(|e| {
                    let chain_text = worker_error_chain_text(&e);
                    pack_error(
                        &human_error_detail(&chain_text),
                        &crate::egress::redact_opaque_tokens(&chain_text),
                    )
                })
            });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_RESULT, LPARAM(payload as isize));
                }
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
    ///
    /// #805: opening the palette sends nothing. No capture, no model request:
    /// the screen is read only after the user picks an action or types
    /// something, by that action's own flow.
    fn toggle_palette(&mut self) {
        if self.palette.is_visible() {
            self.palette.hide();
            return;
        }
        if pause::is_paused_now() {
            return;
        }

        let Some(catalogue) = self.load_catalogue(true) else {
            return;
        };

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

        // #339: asked BEFORE the palette takes the foreground.
        self.palette_password_manager = self.password_manager_now();
        self.palette.show(catalogue, model_configured, footer);
    }

    /// The actions a picker (the palette, the options card) chooses from: the
    /// built-ins and `actions.toml`, as `palette_model::catalogue` builds
    /// them. A broken `actions.toml` shows its own card and returns `None`.
    /// `notify` also shows the once-per-change "some actions were skipped"
    /// card; the options card passes `false` so that card is not drawn over
    /// by the options it is about to show, and the palette still shows it.
    fn load_catalogue(&mut self, notify: bool) -> Option<Vec<palette_model::PaletteAction>> {
        match actions::load_actions_report() {
            Ok(report) => {
                if notify {
                    self.show_skipped_notice(&report);
                }
                Some(palette_model::catalogue(&report.actions))
            }
            Err(e) => {
                actions::queue_skipped(&[]);
                let detail = self.track_error("Couldn't load actions.toml", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't load actions.toml", &detail);
                None
            }
        }
    }

    /// How the options card behaves, from `[options]`. `key_runs_first` is
    /// whether one press of the key will run option 1 (it is only drawn as the
    /// default, with the accent outline, when it will).
    fn options_behavior(&self, key_runs_first: bool) -> OptionsBehavior {
        // Read once: the claim follows this window for the card's whole life.
        let (fg, fg_is_own) = foreground_window_and_ownership();
        OptionsBehavior {
            dismiss_secs: self.config.options.card_dismiss_secs(),
            // Off under a CJK input method: its digits pick candidates, so
            // the card must not eat them. Read on this thread, once, while
            // the user's window is still in front.
            claim_keys: options_keys::claim_enabled(
                self.config.options.claim_keys,
                foreground_keyboard_layout(),
                options_keys::foreground_usable(fg, fg_is_own),
            ),
            key_runs_first,
            foreground: fg,
        }
    }

    /// #307: the options card, from the tray item "What can I do here?". The
    /// screen is captured first (on this thread, small, before the card
    /// exists, so the card is never in it), then the card shows at once as a
    /// shell ("Looking at your screen") while the router works, and its
    /// buttons drop in when the answer arrives. With no model ready the router
    /// never runs and the card is the static list of actions that need none.
    ///
    /// Gates, in order: busy, paused, a password manager in front (the
    /// capture-gate card is shown instead, and nothing is captured). A router
    /// failure of any kind is not an error card: the card shows the floor.
    /// Nothing runs until a row is picked ([`App::on_options_choice`]).
    ///
    /// `use_router` is false for a duplicate launch (the Start menu, the
    /// Copilot-key picker): that shows the static card, the model-free options
    /// only, and sends nothing, so it can never cost money (#308).
    fn show_options_card(&mut self, trigger: Trigger, use_router: bool) {
        if self.busy_now() {
            self.show_busy_card();
            return;
        }
        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to ask.", 3, None);
            return;
        }
        // #339: before the card is touched and before anything is read.
        if self.refuse_if_password_manager() {
            return;
        }
        let Some(catalogue) = self.load_catalogue(false) else {
            return;
        };

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            self.set_watch(false);
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        // The same optimistic, network-free "is anything configured" check
        // `ask`'s readiness gate uses: configured, not reachable right now.
        let model_ready = use_router
            && !providers
                .build_chain_for_mode(mode, true)
                .ready_provider_names()
                .is_empty();
        self.options_generation = options_keys::next_generation(self.options_generation);
        let generation = self.options_generation;

        if !model_ready {
            let content = router::options_content(None, &catalogue, false);
            let rows = option_rows(&content.options);
            let key_pickable = option_key_pickable(&content.options, &catalogue);
            let behavior = self.options_behavior(keypress_runs_first(&key_pickable));
            self.options = Some(OptionsSession {
                generation,
                ids: rows.iter().map(|r| r.id.clone()).collect(),
                catalogue: Vec::new(),
                key_pickable,
            });
            self.card
                .show_options(generation, &content.summary, &rows, behavior);
            self.set_watch(true);
            return;
        }

        // #918: the router sends the screenshot too, and does not go through
        // `begin_action`, so it asks the same remote-Ollama question: the
        // first time for an address that is not on this PC, show the card
        // and send nothing.
        if self.show_remote_ollama_notice() {
            return;
        }

        // A heavily downscaled capture, see `router::ROUTER_IMAGE_LONG_EDGE`.
        // Grabbed on the main thread (Win32 capture must not run on a worker);
        // the PNG encode happens on the worker.
        let raw = match capture::grab_raw_for(
            &self.config.capture.monitor,
            router::ROUTER_IMAGE_LONG_EDGE,
            router::ROUTER_IMAGE_MAX_PIXELS,
            capture::LookupWindow::when(
                capture::needs_password_lookup(self.config.capture.redact, &providers, mode),
                capture::LookupWindow::Foreground,
            ),
            &self.config.capture.extra_password_managers,
        ) {
            Ok(r) => r,
            Err(e) => {
                let detail = self.track_error("Couldn't capture the screen", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't capture the screen", &detail);
                return;
            }
        };

        let candidates = router::candidates_from_catalogue(&catalogue);
        self.options = Some(OptionsSession {
            generation,
            ids: Vec::new(),
            catalogue,
            key_pickable: Vec::new(),
        });
        // No buttons yet, so nothing for the key to run: no default outline.
        let behavior = self.options_behavior(false);
        self.card
            .show_options(generation, router::LOOKING_SUMMARY, &[], behavior);
        self.set_watch(true);
        self.start_options_router(raw, candidates, generation, trigger);
    }

    /// Starts the router for the options card showing `generation`. The call
    /// runs on its own thread so the wait can be capped: past
    /// `ROUTER_WALL_CLOCK` the card gets the floor and a late answer is
    /// dropped with the channel. The result always arrives as
    /// `WM_APP_ROUTER_RESULT`, a failure included.
    fn start_options_router(
        &mut self,
        mut raw: capture::RawShot,
        candidates: Vec<router::RouterCandidate>,
        generation: u32,
        trigger: Trigger,
    ) {
        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let target = self.hwnd_isize();
        let redact = self.config.capture.redact;
        let limits = self.config.options.router_limits();

        std::thread::spawn(move || {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                // #236: the egress row this call writes names its trigger.
                crate::activation::set_worker_trigger(trigger);
                let result: std::result::Result<router::RouterResult, String> =
                    catch_worker(|| {
                        router_worker(&providers, mode, &mut raw, redact, &candidates, limits)
                            .map_err(|e| {
                                let chain_text = worker_error_chain_text(&e);
                                pack_error(
                                    &human_error_detail(&chain_text),
                                    &crate::egress::redact_opaque_tokens(&chain_text),
                                )
                            })
                    });
                let _ = tx.send(result);
            });
            let result =
                router::await_result(&rx, router::ROUTER_WALL_CLOCK).unwrap_or_else(|| {
                    Err(pack_error(
                        "The router took too long.",
                        "router: no answer within the wall-clock cap",
                    ))
                });
            let payload = Box::into_raw(Box::new((u64::from(generation), result)));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_ROUTER_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_ROUTER_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    /// The router answered for the options card showing `generation`: the
    /// shell becomes the real card. A result for a card that is gone (hidden,
    /// replaced, or a newer one showing) is dropped. A failure is not a card
    /// of its own (the router is advisory): the card shows the floor, and the
    /// reason goes to the diagnostics log.
    fn on_router_result(
        &mut self,
        generation: u64,
        result: std::result::Result<router::RouterResult, String>,
    ) {
        let Ok(generation) = u32::try_from(generation) else {
            return;
        };
        let looking = self
            .options
            .as_ref()
            .is_some_and(|s| s.generation == generation && s.ids.is_empty());
        if !looking || self.card.options_generation() != Some(generation) {
            return;
        }
        let Some(session) = self.options.take() else {
            return;
        };
        let routed = match result {
            Ok(r) => Some(r),
            Err(packed) => {
                // The human half only: it carries no screen content or key.
                crate::diagnostics::note_warning(&format!(
                    "Options card: the router failed, showing the fallback options ({})",
                    unpack_error(&packed).0
                ));
                None
            }
        };
        let content = router::options_content(routed.as_ref(), &session.catalogue, true);
        let rows = option_rows(&content.options);
        let key_pickable = option_key_pickable(&content.options, &session.catalogue);
        self.card
            .set_options_key_runs_first(generation, keypress_runs_first(&key_pickable));
        if self.card.set_options(generation, &content.summary, &rows) {
            self.options = Some(OptionsSession {
                generation,
                ids: rows.iter().map(|r| r.id.clone()).collect(),
                catalogue: Vec::new(),
                key_pickable,
            });
        }
    }

    /// The user chose on the options card: a row picked by click or by a
    /// claimed digit, or Esc. `lparam` is the generation of the card the
    /// choice was made on; a choice for any other card runs nothing, so a
    /// click that outlived its card (or a digit typed as the router's answer
    /// replaced the buttons) cannot run the wrong thing.
    ///
    /// A pick hides the card first and lets it settle, exactly as the palette
    /// does, so the card is never in the screenshot or the OCR input of the
    /// action it chose, then runs the action through the one dispatch
    /// ([`App::dispatch_palette_action`]). Everything that writes goes through
    /// that action's own preview and waits for Enter.
    fn on_options_choice(&mut self, wparam: usize, lparam: isize) {
        let decided = options_choice_action(
            wparam,
            lparam,
            self.card.options_generation(),
            self.options.as_ref(),
        );
        match decided {
            // The hook spent the claim taking this key, but the card is still
            // up (stale generation, or the buttons were not there yet): put
            // the claim back so the hint on the card stays true.
            OptionsChoiceAction::Ignore => {
                if rearm_after(&decided) {
                    self.card.rearm_options_claim();
                }
            }
            OptionsChoiceAction::Close => self.close_options_card(),
            OptionsChoiceAction::Run(id) => self.run_option(&id),
        }
    }

    /// Hides the options card, lets it settle, and runs the chosen action
    /// through the one dispatch. Shared by a click, a digit and the key.
    fn run_option(&mut self, id: &str) {
        self.close_options_card();
        std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        self.palette_password_manager = false;
        self.dispatch_palette_action(id);
    }

    /// #308: one press of the Wingman key. What it does follows the options
    /// card ([`keypress::on_key`]): while the router works it cancels, on the
    /// card it runs option 1 when the key may, otherwise it closes the card.
    /// With no options card it does the primary action: show the options card
    /// (the default), or run the action `hotkeys.primary_action` names, with
    /// no router call (`"check-my-work"` is the old one-press flow).
    fn on_primary_key(&mut self, trigger: Trigger) {
        let phase = key_phase_of(self.options.as_ref(), self.card.options_generation());
        match keypress::on_key(phase) {
            keypress::KeyAction::Close => self.close_options_card(),
            keypress::KeyAction::RunFirst => {
                let first = self.options.as_ref().and_then(|s| s.ids.first().cloned());
                match first {
                    Some(id) => self.run_option(&id),
                    None => self.close_options_card(),
                }
            }
            keypress::KeyAction::Proceed => match self.config.hotkeys.primary_target() {
                crate::config::PrimaryAction::Options => self.show_options_card(trigger, true),
                crate::config::PrimaryAction::Action(id) => self.run_pinned_action(&id, trigger),
            },
        }
    }

    /// A pinned `primary_action`: the action runs at once. Check my work keeps
    /// its own trigger (the egress log still says what started the call); every
    /// other id goes through the palette's dispatch, which ends an id it does
    /// not know in that action's own error card.
    fn run_pinned_action(&mut self, id: &str, trigger: Trigger) {
        if matches!(
            palette_model::dispatch_target_for(id),
            Some(DispatchTarget::CheckMyWork)
        ) {
            self.ask(trigger);
        } else {
            self.palette_password_manager = false;
            self.dispatch_palette_action(id);
        }
    }

    /// Hides the options card and forgets it. The card's own `hide` stops the
    /// key claim and the auto-dismiss.
    fn close_options_card(&mut self) {
        self.options = None;
        self.card.hide();
        self.set_watch(false);
    }

    /// `"<name>:<model>"` for the first entry `self.config.mode` would
    /// actually try -- `Providers::selected_order_for_mode` applied to the
    /// current mode, the SAME mode-aware filtering/reordering
    /// `build_chain_for_mode` (and so the real worker thread) uses -- or
    /// just `"<name>"` when that provider has no distinct model field set to
    /// a non-empty value. `None` when nothing survives mode filtering --
    /// the palette footer then shows just the mode label (see
    /// `palette_model::footer_line`).
    ///
    /// Issue #354 follow-up review, finding 4: this used to read
    /// `providers.order.first()` directly, ignoring `mode` entirely -- so
    /// e.g. `order = ["openai", "ollama"]` with `mode = Local` named
    /// "openai" even though Local mode would never try it. `ollama_ready:
    /// true` here is the same optimistic-upper-bound argument
    /// `readiness_gate`'s doc comment already explains: this only decides
    /// whether Auto mode COUNTS Ollama as a candidate (no network probe),
    /// never a claim it is reachable right now.
    fn first_provider_model_label(&self) -> Option<String> {
        Self::provider_model_label_for(self.config.mode, &self.config.providers)
    }

    /// Thin associated-fn alias over the free `first_provider_model_label`
    /// (issue #243's split, so it is testable without a live `HWND`), kept
    /// so the #354 mode-filtering tests read as `App::...`.
    fn provider_model_label_for(mode: Mode, providers: &Providers) -> Option<String> {
        first_provider_model_label(mode, providers)
    }

    /// #25: Enter in the palette routes here through the SAME dispatch table
    /// its rows were built from (`palette_model::dispatch_target_for`),
    /// which is unit tested to cover every built-in action id -- see that
    /// function's doc comment. Each arm calls the EXACT method the
    /// corresponding tray item already calls; there is no second,
    /// palette-only code path. A `None` target (an id the palette itself
    /// never produces) is a no-op, not a panic (rule 7).
    fn dispatch_palette_action(&mut self, action_id: &str) {
        // #339: the palette hid itself before this runs, so the live check
        // sees whatever Windows re-activated. Honour what was in front when
        // it opened as well, for the length of this one dispatch.
        self.password_manager_hint = self.palette_password_manager;
        self.dispatch_palette_target(action_id);
        self.password_manager_hint = false;
    }

    fn dispatch_palette_target(&mut self, action_id: &str) {
        match palette_model::dispatch_target_for(action_id) {
            Some(DispatchTarget::CheckMyWork) => self.ask(Trigger::Palette),
            Some(DispatchTarget::ExtractText) => self.extract_text(),
            Some(DispatchTarget::ReadQr) => self.read_qr(),
            Some(DispatchTarget::ResolveDates) => self.resolve_dates(),
            Some(DispatchTarget::AddToCalendar) => self.add_event_from_screen(),
            Some(DispatchTarget::ReviewEmail) => self.review_this_email(),
            Some(DispatchTarget::FillForm) => self.fill_form_from_screen(),
            Some(DispatchTarget::CalculateSelection) => self.calculate_selection(),
            Some(DispatchTarget::CopyRegion) => self.copy_region(),
            // #242: any other action id -- a user-authored `actions.toml`
            // entry -- runs through the generic Look/Propose/Confirm/Do
            // path instead of silently doing nothing.
            Some(DispatchTarget::Generic(action_id)) => self.run_generic_action(&action_id),
            // `dispatch_target_for` never returns `None` (#242); kept only
            // because the match is over an `Option`, not because this arm
            // is expected to run.
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
        self.run_model_free_screen_action(actions::extract_text::recognize_and_copy);
    }

    /// "Read QR code from screen" (#313): decode every QR code on the active
    /// monitor and copy the text. Same flow, gates and card path as
    /// `extract_text` (no provider, no `Mode`, no network); a link in a code
    /// is shown on the card and never opened.
    fn read_qr(&mut self) {
        self.run_model_free_screen_action(actions::read_qr::read_and_copy);
    }

    /// "Relative dates to real dates" (#336): list what phrases like "next
    /// Tuesday" in the selection (else on screen) work out to. Same flow,
    /// gates and card path as `extract_text` (no provider, no `Mode`, no
    /// network). Today's local date is read here, on the main thread, and
    /// handed to the worker.
    fn resolve_dates(&mut self) {
        if self.busy_now() {
            self.show_busy_card();
            return;
        }
        let today = match local_today_and_utc_offset() {
            Ok((today, _)) => today,
            Err(e) => {
                let detail = self.track_error("Couldn't read the local date", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't read the local date", &detail);
                return;
            }
        };
        let week = local_week_start();
        self.run_model_free_screen_action(move |raw| {
            actions::relative_dates::run(raw, today, week)
        });
    }

    /// The shared body of the model-free screen actions: gate on Paused,
    /// capture on the main thread, then run `work` on a worker thread and
    /// report through `WM_APP_RESULT`. See `extract_text` for the ordering.
    fn run_model_free_screen_action(
        &mut self,
        work: impl FnOnce(&crate::capture::RawShot) -> anyhow::Result<Answer> + Send + 'static,
    ) {
        if self.busy_now() {
            self.show_busy_card();
            return;
        }

        if let Err((headline, detail)) = actions::extract_text::gate(pause::is_paused_now()) {
            self.card.show_answer(headline, detail, 3, None);
            return;
        }

        if self.refuse_if_password_manager() {
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        let raw = match actions::extract_text::capture_screen() {
            Ok(r) => r,
            Err(e) => {
                let detail = self.track_error("Couldn't capture the screen", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't capture the screen", &detail);
                return;
            }
        };

        self.set_busy(true);
        self.set_watch(false);
        // No model in this flow (see the doc comment above): `Working` is
        // the honest generic placeholder, not `AskingModel`.
        self.card
            .show_pending(crate::ui::card::PendingStage::Working);

        let target = self.hwnd_isize();
        let run = self.run_generation;
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let result: std::result::Result<Answer, String> = catch_worker(|| {
                work(&raw).map_err(|e| {
                    let chain_text = worker_error_chain_text(&e);
                    pack_error(
                        &human_error_detail(&chain_text),
                        &crate::egress::redact_opaque_tokens(&chain_text),
                    )
                })
            });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_RESULT, LPARAM(payload as isize));
                }
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
        if self.busy_now() {
            self.show_busy_card();
            return;
        }

        if self.refuse_if_password_manager() {
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
                let detail =
                    self.track_error("Couldn't open the region selector", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't open the region selector", &detail);
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
            Err(e) => {
                let detail = self.track_error("Couldn't copy the region", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't copy the region", &detail);
            }
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
        let Some((mut raw, foreground_hwnd_isize, (today, offset_minutes))) = self
            .begin_model_action(|app| match local_today_and_utc_offset() {
                Ok(v) => Some(v),
                Err(e) => {
                    let detail = app.track_error("Couldn't read the local date", &format!("{e:#}"));
                    app.card
                        .show_error_with_details("Couldn't read the local date", &detail);
                    None
                }
            })
        else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let target = self.hwnd_isize();
        let run = self.run_generation;
        let redact = self.config.capture.redact;
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let result: std::result::Result<Value, String> = catch_worker(|| {
                (|| -> Result<Value> {
                    let shot = encode_for_upload(&mut raw, &providers, mode, redact)?;
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
                .map_err(|e| {
                    let chain_text = worker_error_chain_text(&e);
                    pack_error(
                        &human_error_detail(&chain_text),
                        &crate::egress::redact_opaque_tokens(&chain_text),
                    )
                })
            });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_CALENDAR_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_CALENDAR_RESULT, LPARAM(payload as isize));
                }
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
        self.take_fallback_footer();
        self.set_busy(false);

        let proposal = match result {
            Ok(p) => p,
            Err(packed) => {
                if self.quiet_send_cancel(&packed) {
                    return;
                }
                if self.show_empty_chain_failure(&packed) {
                    return;
                }
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                self.card
                    .show_error_with_details(&headline, &with_pending_notices(human));
                self.set_watch(true);
                return;
            }
        };

        if actions::calendar::is_no_event(&proposal) {
            self.card.show_answer(
                "No event found on screen",
                &with_pending_notices(
                    "Point the Copilot key at something with a date and time, then try again.",
                ),
                5,
                None,
            );
            self.set_watch(true);
            return;
        }

        let resolved = match actions::load_actions() {
            Ok(r) => r,
            Err(e) => {
                let detail = self.track_error("Couldn't load actions.toml", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't load actions.toml", &detail);
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
            arm_calendar_slot(&mut self.pending_preview);
            self.pending_preview_generation = self.card.show_preview_with_note(
                "Add event from screen",
                &schema,
                &proposal,
                false,
                self.fallback_footer.as_deref(),
            );
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
                    Ok(confirmed) => self.run_calendar_executor(executor, confirmed),
                    Err(e) => {
                        let detail = self.track_error("Couldn't add the event", &format!("{e:#}"));
                        self.card
                            .show_error_with_details("Couldn't add the event", &detail);
                        self.set_watch(true);
                    }
                }
            }
            Err(e) => {
                let detail = self.track_error("Couldn't add the event", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't add the event", &detail);
                self.set_watch(true);
            }
        }
    }

    /// #242: the generic-dispatch entry point for any palette action id
    /// `palette_model::dispatch_target_for` does not special-case -- a
    /// user-authored `actions.toml` entry. Looks the action up by id (an
    /// error card if it was disabled or removed since the palette was
    /// shown, never a panic), resolves its `proposal` schema, and runs the
    /// same capture-then-model-call shape `ask`/`add_event_from_screen`
    /// already use. An action whose `proposal` names a schema
    /// `actions::schema::schema_for` does not know (e.g. a
    /// deliberately-model-free proposal like `extract_text`'s `"ocr_text"`)
    /// is reported as a load error today rather than run with no schema:
    /// the generic path only covers "reuse an existing proposal schema and
    /// executor", the case `docs/actions.md` documents as the intended
    /// shape for a new `actions.toml` action.
    fn run_generic_action(&mut self, action_id: &str) {
        self.run_generic_action_with(action_id, None);
    }

    /// [`App::run_generic_action`], with the address the user confirmed when
    /// the action reads a linked page (#127). Such an action runs twice:
    /// first with `None`, which finds the link and shows the confirmation
    /// card (nothing is fetched, no model is asked), then, after "Do it",
    /// with the confirmed address, which fetches the page and summarizes it.
    fn run_generic_action_with(&mut self, action_id: &str, confirmed_link: Option<String>) {
        let resolved = match actions::load_actions_report() {
            Ok(r) => r,
            Err(e) => {
                actions::queue_skipped(&[]);
                let detail = self.track_error("Couldn't load actions.toml", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't load actions.toml", &detail);
                return;
            }
        };
        let Some(action) = resolved
            .actions
            .iter()
            .find(|r| r.action.id == action_id)
            .map(|r| r.action.clone())
        else {
            let reason = resolved.skipped_reason(action_id).map_or_else(
                || format!("no visible action with id \"{action_id}\""),
                str::to_string,
            );
            let detail = self.track_error("Couldn't run that action", &reason);
            self.card
                .show_error_with_details("Couldn't run that action", &detail);
            return;
        };
        let Some(schema) = actions::schema::schema_for(&action.proposal, action.rate_difficulty)
        else {
            let detail = self.track_error(
                &format!("Couldn't run \"{}\"", action.name),
                &format!(
                    "\"{}\" is not a proposal schema Wingman can run generically yet",
                    action.proposal
                ),
            );
            self.card
                .show_error_with_details(&format!("Couldn't run \"{}\"", action.name), &detail);
            return;
        };

        // #127: the fetch step reads no screen, so it takes no screenshot.
        // #494: nor does an action whose inputs are all text.
        let link_step = actions::link_summary::step_for(&action, confirmed_link.as_deref());
        let plan = BeginPlan {
            capture: !matches!(link_step, actions::link_summary::Step::Fetch(_))
                && actions::generic_inputs::screen_may_be_needed(&action.inputs),
            ..BeginPlan::MODEL
        };
        let Some((mut raw, foreground_hwnd_isize, ())) = self.begin_action(plan, |_| Some(()))
        else {
            return;
        };

        // #551: the card opened on "Asking a model", but a confirmed link is
        // downloaded first. Relabel it now, before the loop paints anything;
        // the worker moves it on to the model once the page is in.
        if let actions::link_summary::Step::Fetch(url) = &link_step {
            if let Ok(t) = crate::inputs::web_page::parse_target(url) {
                self.card
                    .set_pending_stage(crate::ui::card::PendingStage::ReadingPage { host: t.host });
            }
        }
        let run = self.run_generation;
        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let redact = self.config.capture.redact;
        let prompt = action.prompt.clone();
        let inputs = action.inputs.clone();
        // #68: an action that writes the answer over the selection needs the
        // selection's UIA identity as well as its text.
        let writes_back = action.executor == "replace_text";
        let cut_selection_refused = actions::code_actions::refuses_cut_selection(&action.id);
        let target = self.hwnd_isize();
        // #242 review: the action and the schema it was dispatched with are
        // carried through the worker and back in the result payload, rather
        // than `on_generic_action_result` re-loading `actions.toml` and
        // re-resolving the schema after the model call returns. A second
        // load reading a file that changed (or vanished) between dispatch
        // and result -- a real possibility, since nothing locks
        // `actions.toml` against a concurrent edit -- must never panic the
        // main thread (rule 7); using exactly what was dispatched makes
        // that whole class of failure unreachable instead of merely rare.
        //
        // #127: the first step of a linked-page action answers with the
        // confirmation card's own action and schema instead.
        let (action_for_result, schema_for_result) =
            if link_step == actions::link_summary::Step::Confirm {
                (
                    actions::link_summary::confirm_action(&action),
                    actions::link_summary::confirm_schema(),
                )
            } else {
                (action.clone(), schema.clone())
            };
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let result: GenericResult = catch_worker(|| {
                (|| -> Result<_, GenericFailure> {
                    // #127 step 1: find the link, check it, and hand it back for
                    // confirmation. Nothing is fetched and no model is asked.
                    if link_step == actions::link_summary::Step::Confirm {
                        let target = link_to_confirm(&inputs, foreground_hwnd_isize, raw.as_ref())?;
                        return Ok((actions::link_summary::confirm_proposal(&target), None));
                    }
                    // #127 step 2: the user confirmed this address. The page is the
                    // only input: the selection and the screen are not read again.
                    let mut redirected_to: Option<(String, String)> = None;
                    let (gathered, inputs) = match &link_step {
                        actions::link_summary::Step::Fetch(url) => {
                            let page = crate::inputs::web_page::fetch_page(url)
                                .map_err(GenericFailure::UserState)?;
                            // #551: the page is in; the model is next.
                            post_pending_stage(
                                target,
                                run,
                                crate::ui::card::PendingStage::AskingModel {
                                    model_name: first_provider_model_label(mode, &providers)
                                        .unwrap_or_default(),
                                },
                            );
                            // A redirect to another site is named on the result card.
                            let confirmed = crate::inputs::web_page::parse_target(url)
                                .map(|t| t.host)
                                .unwrap_or_default();
                            redirected_to = Some((confirmed, page.host.clone()));
                            (
                                actions::link_summary::gathered_from(page),
                                vec![actions::InputKind::LinkedPage],
                            )
                        }
                        _ => (
                            // #484: gather what the action's `inputs` list asks
                            // for, then let the pure `plan` decide the user text
                            // and whether the screenshot goes along (a
                            // selection-only action sends no image).
                            gather_generic_inputs(&inputs, foreground_hwnd_isize, writes_back)?,
                            inputs,
                        ),
                    };
                    let planned = plan_generic_inputs(&inputs, &gathered)?;
                    if writes_back && actions::generic_replace::too_long(&gathered) {
                        return Err(GenericFailure::UserState(
                            actions::generic_replace::TOO_LONG_MESSAGE.to_string(),
                        ));
                    }
                    // #73: an explanation or tests for part of the code would
                    // read as if they covered all of it.
                    if cut_selection_refused && actions::generic_replace::too_long(&gathered) {
                        return Err(GenericFailure::UserState(
                            actions::code_actions::TOO_LONG_MESSAGE.to_string(),
                        ));
                    }
                    let replace = writes_back
                        .then(|| actions::generic_replace::replace_plan(&gathered))
                        .flatten();
                    // #493: gathered text is data, not instructions.
                    let system = match planned.system_note {
                        Some(note) => format!("{prompt}\n\n{note}"),
                        None => prompt.clone(),
                    };
                    let shot = if planned.screen {
                        let screen = raw
                            .as_mut()
                            .context("no screenshot was taken for this request")?;
                        Some(encode_for_upload(screen, &providers, mode, redact)?)
                    } else {
                        None
                    };
                    let ollama_ready = mode == Mode::Auto
                        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
                        && mode::probe_ollama_ready(
                            &providers.ollama.base_url,
                            &providers.ollama.model,
                        );
                    let chain = providers.build_chain_for_mode(mode, ollama_ready);
                    let req = generic_action_request(shot.as_ref(), &system, &planned.user, schema);
                    let proposal =
                        noting_attribution(chain.complete_parsed_with_fallback_attributed(
                            &req,
                            || match raw.as_ref() {
                                Some(screen) => non_vision_inputs(screen, foreground_hwnd_isize),
                                None => {
                                    Err(anyhow::anyhow!("no screenshot was taken for this request"))
                                }
                            },
                            |c| actions::parse_generic_proposal(&c.text),
                        ))?;
                    let proposal = match redirected_to {
                        Some((confirmed, read)) => {
                            actions::link_summary::note_final_host(proposal, &confirmed, &read)
                        }
                        None => proposal,
                    };
                    Ok((proposal, replace))
                })()
                .map_err(pack_generic_failure)
            });
            let payload = Box::into_raw(Box::new((action_for_result, schema_for_result, result)));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_GENERIC_ACTION_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_GENERIC_ACTION_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    /// #551: applies a worker's stage message. Ignored when it belongs to an
    /// earlier run; the card itself ignores it unless it is still pending.
    fn on_pending_stage(&mut self, run: u64, stage: crate::ui::card::PendingStage) {
        if crate::ui::pending_status::stage_is_current(run, self.run_generation) && self.busy {
            self.card.set_pending_stage(stage);
        }
    }

    /// Handles [`App::run_generic_action`]'s worker result (#242): an error
    /// shows an error card (rule 7); a success either shows the preview
    /// card (when the action's `confirm` field is `true`, the default) or,
    /// for a read-only action with `confirm = false`, auto-confirms and
    /// runs the executor immediately -- the same two-path shape
    /// `on_calendar_result` already implements. `action` and `schema` are
    /// exactly what `run_generic_action` dispatched with (carried through
    /// the worker's result payload, see that function's doc comment) --
    /// this handler never re-loads `actions.toml` or re-resolves a schema,
    /// so there is nothing here that can fail on a file that changed since
    /// dispatch, and no `.expect()` on the main thread.
    fn on_generic_action_result(
        &mut self,
        action: actions::Action,
        schema: Value,
        result: GenericResult,
    ) {
        self.take_fallback_footer();
        self.set_busy(false);

        let (proposal, replace) = match result {
            Ok(done) => done,
            Err(packed) => {
                if self.quiet_send_cancel(&packed) {
                    return;
                }
                if self.show_empty_chain_failure(&packed) {
                    return;
                }
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                self.card
                    .show_error_with_details(&headline, &with_pending_notices(human));
                self.set_watch(true);
                return;
            }
        };

        // #127 step 1 of a linked-page action: the confirmation card. Only
        // its "Do it" fetches anything (`on_preview_decided`).
        if action.proposal == actions::link_summary::CONFIRM_PROPOSAL {
            let host = proposal
                .get("url")
                .and_then(Value::as_str)
                .and_then(|url| crate::inputs::web_page::parse_target(url).ok())
                .map_or_else(|| "this link".to_string(), |t| t.host);
            self.pending_preview = Some(PendingPreview::FetchLink(action.id.clone()));
            self.pending_preview_generation = self.card.show_preview(
                &actions::link_summary::confirm_title(&host),
                &schema,
                &proposal,
                false,
            );
            return;
        }

        // #314: nothing to list or copy is an answer, not a blank preview.
        if actions::action_items::is_empty_result(&action.proposal, &proposal) {
            self.card.show_answer(
                actions::action_items::NO_ITEMS_HEADLINE,
                &with_pending_notices(""),
                0,
                None,
            );
            self.set_watch(true);
            return;
        }

        // A write-back whose text is the selection as it was has nothing to
        // write: say so instead of showing a pointless preview.
        // The text also takes the document's line endings here, so the card,
        // the write and the copy-instead all carry the same text.
        let proposal = match (action.executor == "replace_text", replace.as_deref()) {
            (true, Some(plan)) => actions::generic_replace::normalize_proposal(plan, proposal),
            _ => proposal,
        };
        if action.executor == "replace_text" {
            if let Some(plan) = replace.as_deref() {
                if actions::generic_replace::is_unchanged(&plan.selected_text, &proposal) {
                    self.card.show_answer(
                        actions::generic_replace::NO_CHANGES_HEADLINE,
                        &with_pending_notices(""),
                        0,
                        None,
                    );
                    self.set_watch(true);
                    return;
                }
            }
        }
        // #70: a receipt is one row under five fixed headers, whatever the
        // model sent.
        let proposal = if action.id == actions::receipt::ACTION_ID {
            actions::receipt::clamp_receipt(&proposal)
        } else {
            proposal
        };
        // #69: no table on screen, or a reply that is not a table, is an
        // answer, not an empty preview.
        let table_headline = if actions::table::is_unreadable(&action.proposal, &proposal) {
            Some(actions::table::NOT_READ_HEADLINE)
        } else if actions::table::is_empty_result(&action.proposal, &proposal) {
            Some(actions::table::NO_TABLE_HEADLINE)
        } else {
            None
        };
        if let Some(headline) = table_headline {
            self.card
                .show_answer(headline, &with_pending_notices(""), 0, None);
            self.set_watch(true);
            return;
        }
        // A table past the caps is cut, and the card shows the cut table and
        // says so, so what is shown is what is copied.
        let (proposal, note) = match actions::table::Table::from_value(&proposal) {
            Some(t) if action.proposal == actions::table::PROPOSAL_KIND && t.capped => {
                (t.to_value(), Some(actions::table::CAPPED_NOTE.to_string()))
            }
            _ => (proposal, self.fallback_footer.clone()),
        };
        // #710: a receipt cell that could not be checked is named on the card.
        let note = if action.id == actions::receipt::ACTION_ID {
            actions::receipt::card_note(&proposal, note.as_deref())
        } else {
            note
        };
        // #114: values read off a chart are estimates. The headers say so (and
        // so does the CSV), and the card says so under the rows.
        let (proposal, note) = if action.id == actions::chart_table::ACTION_ID {
            let (marked, card_note) = actions::chart_table::finish(&proposal, note.as_deref());
            (marked, Some(card_note))
        } else {
            (proposal, note)
        };

        if action.confirm {
            let writes_back = action.executor == "replace_text";
            let title = if writes_back {
                actions::generic_replace::preview_title(
                    &action.name,
                    replace.as_deref().is_some_and(|p| p.will_replace()),
                )
            } else {
                action.name.clone()
            };
            self.pending_preview = Some(if writes_back {
                PendingPreview::GenericReplace(action.clone(), replace)
            } else {
                PendingPreview::Generic(action.clone())
            });
            self.pending_preview_generation = self.card.show_preview_with_note(
                &title,
                &schema,
                &proposal,
                false,
                note.as_deref(),
            );
            return;
        }

        // `action.confirm == false`: only reachable for a read-only
        // executor -- `auto_confirm_read_only` refuses anything else, same
        // as `on_calendar_result`'s own `confirm == false` branch.
        match actions::resolve_executor(&action) {
            Ok(executor) => match confirm::auto_confirm_read_only(
                executor.as_ref(),
                confirm::Proposal::new(proposal),
            ) {
                Ok(confirmed) => self.run_confirmed_generic_action(&action, confirmed),
                Err(e) => {
                    let detail = self.track_error(
                        &format!("Couldn't run \"{}\"", action.name),
                        &format!("{e:#}"),
                    );
                    self.card.show_error_with_details(
                        &format!("Couldn't run \"{}\"", action.name),
                        &detail,
                    );
                    self.set_watch(true);
                }
            },
            Err(e) => {
                let detail = self.track_error(
                    &format!("Couldn't run \"{}\"", action.name),
                    &format!("{e:#}"),
                );
                self.card
                    .show_error_with_details(&format!("Couldn't run \"{}\"", action.name), &detail);
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
    /// Three actions can leave a preview on screen
    /// (`add_event_from_screen`, `review_this_email`,
    /// `fill_form_from_screen`), and which one this decision belongs to is
    /// [`App::pending_preview`]: `Review` for #38, `FormFill` for #40, and
    /// `None` for calendar's own flow, whose executor is resolved fresh by
    /// the fixed `"calendar_add"` name (the same "fixed until a second
    /// confirm-required action exists" status `CalendarAddExecutor::new`'s
    /// own fixed `"ics"` connector choice had before #38/#40 landed).
    ///
    /// It is `take()`n unconditionally, before `take_confirmed()` is even
    /// consulted, so Cancel, Esc and an abandoned preview all clear it just
    /// as "Do it" does.
    ///
    /// #225 changed two things here, and both are load-bearing:
    ///
    /// One slot, not two. It used to be a `pending_review` and a
    /// `pending_form_fill` that "could never both be `Some`". They could,
    /// and when they were, this handler checked review first and ran the
    /// abandoned email edit instead of the form fill the user had just
    /// confirmed. A single slot makes that unrepresentable.
    ///
    /// `generation` is the preview this notification is about, taken from
    /// the message's `WPARAM`. The notification is a `PostMessageW`, so
    /// replacing a live preview posts the old one's abandonment and then
    /// arms the new one before that message is delivered. Without the
    /// match, the stale abandonment would clear the state belonging to the
    /// preview now on screen, and its "Do it" would then fall through to
    /// the calendar arm.
    fn on_preview_decided(&mut self, generation: u32) {
        if generation != self.pending_preview_generation {
            return;
        }
        // Taken unconditionally, before `take_confirmed` is consulted, so a
        // Cancel or an abandonment clears it just as a "Do it" does.
        let pending = self.pending_preview.take();
        let Some(confirmed) = self.card.take_confirmed() else {
            return;
        };

        match pending {
            Some(PendingPreview::Review(ctx)) => {
                self.run_review_executor(ctx);
            }
            Some(PendingPreview::FormFill(original)) => {
                self.run_confirmed_form_fill(&original, confirmed);
            }
            Some(PendingPreview::Generic(action)) => {
                self.run_confirmed_generic_action(&action, confirmed);
            }
            Some(PendingPreview::GenericReplace(action, plan)) => {
                self.run_confirmed_generic_replace(&action, plan, confirmed);
            }
            Some(PendingPreview::FetchLink(action_id)) => {
                // #127: the user confirmed the address on the card, so this
                // is the one place a linked page is fetched.
                let url = confirmed
                    .value()
                    .get("url")
                    .and_then(Value::as_str)
                    .map(str::to_string);
                match url {
                    Some(url) => self.run_generic_action_with(&action_id, Some(url)),
                    None => {
                        let detail = self.track_error(
                            "Couldn't fetch the link",
                            "the confirmed card had no address",
                        );
                        self.card
                            .show_error_with_details("Couldn't fetch the link", &detail);
                        self.set_watch(true);
                    }
                }
            }
            Some(PendingPreview::Send(reply)) => {
                crate::ui::send_gate::decide(reply, true);
                // The preview card is gone; show the spinner again while
                // the request this Send released is in flight.
                let model_name = self.first_provider_model_label().unwrap_or_default();
                self.card
                    .show_pending(crate::ui::card::PendingStage::AskingModel { model_name });
            }
            Some(PendingPreview::Calendar) => self.run_confirmed_calendar_add(confirmed),
            Some(PendingPreview::ImportAction(prepared)) => self.commit_import(&prepared),
            // Nothing is waiting on this decision, so nothing runs.
            None => {}
        }
    }

    /// #105: the Wingman key while a send preview is waiting cancels it:
    /// the worker is blocked on it and a second press would otherwise do
    /// nothing, leaving it blocked. Returns whether it cancelled one.
    fn cancel_pending_send_preview(&mut self) -> bool {
        if !take_send_slot(&mut self.pending_preview) {
            return false;
        }
        self.card.hide();
        true
    }

    /// #105: ends a run the user cancelled at the send preview. Nothing was
    /// sent and nothing was done, so there is no card and no failure state
    /// (#392). Returns whether `packed` was such a cancel.
    fn quiet_send_cancel(&mut self, packed: &str) -> bool {
        if !is_send_cancelled(packed) {
            return false;
        }
        self.card.hide();
        self.set_watch(false);
        true
    }

    /// #105: a worker is about to send a request off the machine and is
    /// blocked on the answer. Shows what will be sent on the preview card
    /// ("Send" or Esc). It reuses the one preview slot: if another preview is
    /// already on screen the request is dropped, which cancels that send
    /// rather than stacking a second slot (#225).
    fn on_send_preview(&mut self, request: crate::ui::send_gate::SendRequest) {
        if matches!(self.card.state(), crate::ui::card::CardState::Preview) {
            return;
        }
        let (schema, value) = request.preview.to_schema_and_value();
        let title = crate::ui::send_gate::title_for(&request.provider_id);
        self.pending_preview = Some(PendingPreview::Send(request.reply));
        self.pending_preview_generation = self.card.show_send_preview(&title, &schema, &value);
    }

    /// #242's half of [`App::on_preview_decided`]: resolves `action`'s own
    /// `executor` field generically (no fixed executor name, unlike
    /// `run_confirmed_calendar_add`/`run_confirmed_form_fill`) and runs it
    /// against the card's confirmed value. The result is shown with the
    /// action's own name as the headline: there is no per-action headline
    /// logic to derive from the summary the way `calendar_headline`/
    /// `form_fill_headline` do, because a generic action's executor and
    /// wording are not known ahead of time.
    fn run_confirmed_generic_action(
        &mut self,
        action: &actions::Action,
        confirmed: confirm::Confirmed<Value>,
    ) {
        match actions::resolve_executor(action) {
            Ok(executor) => {
                let action = action.clone();
                self.spawn_executor(Box::new(move || {
                    exec_work_generic(executor, action, confirmed)
                }));
            }
            Err(e) => {
                let detail = self.track_error(
                    &format!("Couldn't run \"{}\"", action.name),
                    &format!("{e:#}"),
                );
                self.card
                    .show_error_with_details(&format!("Couldn't run \"{}\"", action.name), &detail);
                self.set_watch(true);
            }
        }
    }
    /// #68's half of [`App::on_preview_decided`]: writes the confirmed text
    /// over the selection through the action's `replace_text` executor (the
    /// same `do_review_confirmed` path "Review this email" uses, so the
    /// executor's stale-target check applies). When the selection cannot be
    /// written over, or the write fails, the confirmed text is copied to the
    /// clipboard instead and the card says so; the user's text is never
    /// dropped (`actions::generic_replace::replace_or_copy`).
    fn run_confirmed_generic_replace(
        &mut self,
        action: &actions::Action,
        plan: Option<Box<actions::generic_replace::ReplaceSelection>>,
        confirmed: confirm::Confirmed<Value>,
    ) {
        let executors = actions::resolve_executor(action)
            .and_then(|replace| Ok((replace, executors::registry::resolve("clipboard")?)));
        match executors {
            Ok((replace, copy)) => {
                let action_name = action.name.clone();
                self.spawn_executor(Box::new(move || {
                    exec_work_replace(action_name, replace, copy, plan, confirmed)
                }));
            }
            Err(e) => {
                let title = format!("Couldn't run \"{}\"", action.name);
                let detail = self.track_error(&title, &format!("{e:#}"));
                self.card.show_error_with_details(&title, &detail);
                self.set_watch(true);
            }
        }
    }
    /// #40's half of [`App::on_preview_decided`], split out so that handler
    /// is a flat match over [`PendingPreview`] with one arm per kind.
    fn run_confirmed_form_fill(&mut self, original: &Value, confirmed: confirm::Confirmed<Value>) {
        {
            let rebuilt = actions::fill_form::rebuild_after_confirm(original, confirmed.value());
            let token = confirm::user_confirmed();
            let final_confirmed = confirm::confirm(confirm::Proposal::new(rebuilt), token);
            match executors::registry::resolve("fill_form") {
                Ok(executor) => self.run_form_fill_executor(executor, final_confirmed),
                Err(e) => {
                    let detail = self.track_error("Couldn't fill the form", &format!("{e:#}"));
                    self.card
                        .show_error_with_details("Couldn't fill the form", &detail);
                    self.set_watch(true);
                }
            }
        }
    }

    /// The default arm of [`App::on_preview_decided`]: no carried context
    /// means the calendar flow.
    fn run_confirmed_calendar_add(&mut self, confirmed: confirm::Confirmed<Value>) {
        match executors::registry::resolve("calendar_add") {
            Ok(executor) => self.run_calendar_executor(executor, confirmed),
            Err(e) => {
                let detail = self.track_error("Couldn't add the event", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't add the event", &detail);
                self.set_watch(true);
            }
        }
    }

    /// #268: `run_calendar_executor`'s headline, derived from what actually
    /// happened rather than hardcoded. `CalendarAddExecutor::execute`
    /// (`src/executors/calendar_add.rs`) appends ", but it could not be
    /// opened automatically; open ... yourself" to `Undo.summary` exactly
    /// when `result.opened == false` -- `Undo` exposes only `summary:
    /// String` (no `opened` field), so that fixed substring is the only
    /// signal available here without touching `src/executors/`, which
    /// another agent owns tonight.
    fn calendar_headline(summary: &str) -> &'static str {
        if summary.contains("could not be opened automatically") {
            "Event added, not opened"
        } else {
            "Event opened in your calendar app"
        }
    }

    /// Runs `executor` against `confirmed` and shows the result card:
    /// honest per the connector design doc (`Undo.summary` already names
    /// the generated file and that undo only deletes it, never touching
    /// whatever the calendar app itself created) -- never a second action
    /// taken automatically. This is as far as "Do" goes; Wingman never
    /// presses Send, Submit, Buy or Pay. #268: the headline must agree
    /// with `summary`'s own body text instead of always claiming the
    /// event was opened (see [`Self::calendar_headline`]).
    fn run_calendar_executor(
        &mut self,
        executor: Box<dyn executors::Executor>,
        confirmed: confirm::Confirmed<Value>,
    ) {
        self.spawn_executor(Box::new(move || {
            exec_work_calendar(executor.as_ref(), confirmed)
        }));
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
        let Some((mut raw, foreground_hwnd_isize, ())) = self.begin_model_action(|_| Some(()))
        else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let target = self.hwnd_isize();
        let run = self.run_generation;
        let redact = self.config.capture.redact;
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let result: std::result::Result<actions::review_email::ReviewOutcome, String> =
                catch_worker(|| {
                    (|| -> Result<actions::review_email::ReviewOutcome> {
                        let shot = encode_for_upload(&mut raw, &providers, mode, redact)?;
                        review_worker(&providers, mode, &shot, &raw, foreground_hwnd_isize)
                    })()
                    .map_err(|e| {
                        let chain_text = worker_error_chain_text(&e);
                        pack_error(
                            &human_error_detail(&chain_text),
                            &crate::egress::redact_opaque_tokens(&chain_text),
                        )
                    })
                });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_REVIEW_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_REVIEW_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    /// Handles the review-email worker's result (#38). `verdict ==
    /// "good_to_go"` ends in a plain informational card, no preview, no "Do
    /// it" -- the task brief's own wording. Otherwise: edits are applied
    /// deterministically (`actions::review_email::apply_edits`) once, here,
    /// before the preview ever shows, so "Do it" later never re-derives
    /// anything. A `ComposeBody`-sourced review always gets a real preview
    /// with "Do it"; a `Selection`-sourced one does too, but only when the
    /// UIA path actually captured a
    /// [`actions::review_email::ReplaceTarget`] (#219) -- a `ValuePattern`-
    /// only control, a discontiguous multi-range selection, or a
    /// clipboard-fallback capture never does. `Screen`, and any
    /// `Selection` without a captured target, show the proposal as a
    /// read-only informational card instead, since there is nothing "Do it"
    /// could write back to (rule 7: never offer a button that cannot work).
    fn on_review_result(
        &mut self,
        result: std::result::Result<actions::review_email::ReviewOutcome, String>,
    ) {
        self.take_fallback_footer();
        self.set_busy(false);

        let outcome = match result {
            Ok(o) => o,
            Err(packed) => {
                if self.quiet_send_cancel(&packed) {
                    return;
                }
                if self.show_empty_chain_failure(&packed) {
                    return;
                }
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                self.card
                    .show_error_with_details(&headline, &with_pending_notices(human));
                self.set_watch(true);
                return;
            }
        };

        if actions::review_email::is_good_to_go(&outcome.proposal) {
            let detail = with_fallback_footer(
                &with_pending_notices("No changes needed."),
                self.fallback_footer.as_deref(),
            );
            self.card.show_answer("Good to go", &detail, 5, None);
            self.set_watch(true);
            return;
        }

        if !actions::review_email::source_has_target(outcome.source) || outcome.target.is_none() {
            // Screen-sourced, or a Selection/ComposeBody that (per
            // `source_has_target`'s doc comment) still ended up with no
            // captured target: informational only, no target to write back
            // through (see this method's doc comment). #219: unlike before,
            // `source_has_target(outcome.source)` alone is no longer
            // enough to guarantee `outcome.target.is_some()`, so both are
            // checked.
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
            let detail = if outcome.cut {
                format!("{detail}\n\n{}", actions::review_email::CUT_NOTICE)
            } else {
                detail
            };
            let detail = with_pending_notices(&detail);
            let detail = with_fallback_footer(&detail, self.fallback_footer.as_deref());
            self.card
                .show_answer("Review needs edits", &detail, 0, None);
            self.set_watch(true);
            return;
        }
        let target = outcome
            .target
            .clone()
            .expect("just checked outcome.target.is_some() above");

        let edits = actions::review_email::edits_from_value(&outcome.proposal);
        let applied = actions::review_email::apply_edits(&outcome.original_text, &edits);

        self.pending_preview = Some(PendingPreview::Review(
            actions::review_email::ReviewContext {
                target,
                original_text: outcome.original_text.clone(),
                new_text: applied.new_text,
            },
        ));

        let schema = actions::schema::schema_for("text_review", false)
            .expect("\"text_review\" is always registered in actions::schema");
        self.pending_preview_generation = self.card.show_preview_with_note(
            "Review this email",
            &schema,
            &outcome.proposal,
            false,
            self.fallback_footer.as_deref(),
        );
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
                self.spawn_executor(Box::new(move || exec_work_review(executor.as_ref(), &ctx)));
            }
            Err(e) => {
                let detail = self.track_error("Couldn't update the email", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't update the email", &detail);
                self.set_watch(true);
            }
        }
    }
    /// #40: "Fill this form", the tray's fourth one-shot Look/Propose/Confirm/Do
    /// action. Shares `add_event_from_screen`/`review_this_email`'s pause/
    /// busy/readiness/capture steps via `begin_action` (#214), with a
    /// [`BeginPlan`] built from whether a provider is ready (#132): with none,
    /// there is no readiness card, no screen capture and no model stage, and
    /// the pending card says "Reading the form...". Unlike calendar,
    /// this needs no today/UTC offset; it needs the foreground window's
    /// handle (for the worker's own UIA walk, run off this thread so the
    /// message loop stays responsive) and `config.forms.require_tick_for`
    /// (#40, expansion plan §15's still-owed owner decision -- see
    /// `config::RequireTickFor`), neither of which `begin_action`
    /// needs to know about, same as `review_this_email`'s `extra`. Local
    /// mapping (`actions::fill_form::map_candidates_locally`) always runs in
    /// the worker; the model stage runs only when a provider is ready and a
    /// field is left over.
    fn fill_form_from_screen(&mut self) {
        // #132: with no provider ready the form is mapped from the profile
        // alone (no model stage, nothing sent), instead of a "set up a model"
        // card.
        let use_model = Self::readiness_gate(
            self.config.mode,
            &self.config.providers,
            &Config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        )
        .is_none();
        let Some((raw, foreground_hwnd_isize, ())) =
            self.begin_action(BeginPlan::for_fill_form(use_model), |_| Some(()))
        else {
            return;
        };

        let providers = self.config.providers.clone();
        let mode = self.config.mode;
        let require_tick_for = self.config.forms.require_tick_for;
        let target = self.hwnd_isize();
        let run = self.run_generation;
        let redact = self.config.capture.redact;
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let mut raw = raw;
            let result: std::result::Result<Value, String> = catch_worker(|| {
                form_fill_worker(
                    &providers,
                    mode,
                    &mut raw,
                    redact,
                    foreground_hwnd_isize,
                    require_tick_for,
                    use_model,
                )
                .map_err(|e| {
                    let chain_text = worker_error_chain_text(&e);
                    pack_error(
                        &human_error_detail(&chain_text),
                        &crate::egress::redact_opaque_tokens(&chain_text),
                    )
                })
            });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_FORM_FILL_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_FORM_FILL_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    /// Handles the fill_form worker's result (#40). `value` carries one of
    /// three shapes: an empty-profile signal
    /// (`{"empty_profile": true, "toml_path": ..., "data_dir": ...}` --
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
        self.take_fallback_footer();
        self.set_busy(false);

        let value = match result {
            Ok(v) => v,
            Err(packed) => {
                if self.quiet_send_cancel(&packed) {
                    return;
                }
                if self.show_empty_chain_failure(&packed) {
                    return;
                }
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                self.card
                    .show_error_with_details(&headline, &with_pending_notices(human));
                self.set_watch(true);
                return;
            }
        };

        if value.get("empty_profile").and_then(Value::as_bool) == Some(true) {
            let toml_path = value.get("toml_path").and_then(Value::as_str).unwrap_or("");
            let data_dir = value.get("data_dir").and_then(Value::as_str).unwrap_or("");
            self.card.show_answer(
                actions::fill_form::EMPTY_PROFILE_HEADLINE,
                &with_pending_notices(&actions::fill_form::empty_profile_detail(
                    toml_path, data_dir,
                )),
                0,
                None,
            );
            self.set_watch(true);
            return;
        }

        if !actions::fill_form::has_fillable_fields(&value) {
            self.card.show_answer(
                "Nothing to fill",
                &with_pending_notices(&actions::fill_form::nothing_to_fill_detail(&value)),
                5,
                None,
            );
            self.set_watch(true);
            return;
        }

        let (schema, preview_value) = actions::fill_form::build_preview_schema_and_value(&value);
        self.pending_preview = Some(PendingPreview::FormFill(value));
        self.pending_preview_generation = self.card.show_preview_with_note(
            "Fill this form",
            &schema,
            &preview_value,
            false,
            self.fallback_footer.as_deref(),
        );
    }

    /// #268: `run_form_fill_executor`'s headline, derived from what
    /// actually happened rather than hardcoded. `executors::fill_form::
    /// do_fill` (see its module doc, "Refuse the field, not the whole
    /// fill") returns `Ok` even when every field ended up `Skipped` or
    /// `Refused`, and `format_outcomes` (which builds `Undo.summary`)
    /// always starts that summary with the fixed prefix `"Filled {filled}
    /// of {total} field..."` -- see `format_outcomes`'s definition and its
    /// own test `format_outcomes_counts_filled_and_lists_the_rest` in
    /// `src/executors/fill_form.rs`. `src/executors/` is owned by another
    /// agent tonight (and mid-change for #266, which will only make the
    /// zero-filled case more common), so this reads that already-stable
    /// prefix rather than adding any new field to `Undo` -- `Undo` exposes
    /// only `summary: String`.
    ///
    /// Falls back to the old "Form filled" text if the prefix cannot be
    /// parsed (should not happen given `format_outcomes`'s own coverage of
    /// that prefix; a change to it that broke this would also break that
    /// test first).
    fn form_fill_headline(summary: &str) -> &'static str {
        match Self::parse_filled_of_total(summary) {
            Some((0, _)) => "Nothing filled",
            Some((filled, total)) if filled < total => "Form partially filled",
            Some(_) => "Form filled",
            None => "Form filled",
        }
    }

    /// Parses the `"Filled {filled} of {total} field..."` prefix
    /// `executors::fill_form::format_outcomes` always writes. Returns
    /// `None` for anything else rather than guessing.
    fn parse_filled_of_total(summary: &str) -> Option<(usize, usize)> {
        let rest = summary.strip_prefix("Filled ")?;
        let (filled_str, rest) = rest.split_once(" of ")?;
        let total_str = rest.split(|c: char| !c.is_ascii_digit()).next()?;
        let filled = filled_str.parse().ok()?;
        let total = total_str.parse().ok()?;
        Some((filled, total))
    }

    /// Runs the `fill_form` executor and shows the result card (rule 5:
    /// what happened, not what was intended -- `Undo.summary`, built by
    /// `executors::fill_form::format_outcomes`, already lists filled,
    /// skipped and refused fields by name; #268: the headline above it
    /// must agree, via [`Self::form_fill_headline`]). On success, stashes
    /// the `Undo` for "Restore last form" (tray, `restore_last_form`).
    fn run_form_fill_executor(
        &mut self,
        executor: Box<dyn executors::Executor>,
        confirmed: confirm::Confirmed<Value>,
    ) {
        self.spawn_executor(Box::new(move || {
            exec_work_form_fill(executor.as_ref(), confirmed)
        }));
    }

    /// #712: runs `work` (an `exec_work_*` function: a UIA walk and write,
    /// typed input, a clipboard or file write) on its own thread and shows
    /// the working card meanwhile. These can take seconds, and the message
    /// loop is also the keyboard hook's thread, so a slow one on it makes
    /// every keystroke on the machine lag (or the hook time out). The
    /// card's own foreground restore has already happened on the UI thread,
    /// from the user's click, before this is called; the worker never
    /// touches the foreground window. The outcome comes back as
    /// `WM_APP_EXECUTOR_DONE` and ends in a card on every path.
    fn spawn_executor(&mut self, work: Box<dyn FnOnce() -> ExecDone + Send + 'static>) {
        debug_assert!(
            !self.busy,
            "an executor must not start over a running action"
        );
        self.exec_in_flight = true;
        self.set_busy(true);
        self.card
            .show_pending(crate::ui::card::PendingStage::Working);
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<ExecDone, String> = catch_worker(|| Ok(work()));
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_EXECUTOR_DONE,
                    WPARAM(0),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_EXECUTOR_DONE, LPARAM(payload as isize));
                }
            }
        });
    }

    /// #712: shows the card for a finished executor run, whichever action
    /// it was. Each arm is what that action's own handler showed when it ran
    /// the executor inline. On the form fill, stashes the `Undo` for
    /// "Restore last form" (#645: only when something can be restored).
    fn on_exec_done(&mut self, result: std::result::Result<ExecDone, String>) {
        self.exec_in_flight = false;
        self.set_busy(false);
        let done = match result {
            Ok(done) => done,
            Err(packed) => {
                if self.show_empty_chain_failure(&packed) {
                    return;
                }
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                self.card
                    .show_error_with_details(&headline, &with_pending_notices(human));
                self.set_watch(true);
                return;
            }
        };
        match done {
            ExecDone::Calendar(Ok(undo)) => {
                let headline = Self::calendar_headline(&undo.summary);
                self.card
                    .show_answer(headline, &with_pending_notices(&undo.summary), 0, None);
            }
            ExecDone::Calendar(Err(e)) => {
                let detail = self.track_error("Couldn't add the event", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't add the event", &detail);
            }
            ExecDone::Review(Ok(undo)) => {
                self.card.show_answer(
                    "Email updated",
                    &with_pending_notices(&undo.summary),
                    0,
                    None,
                );
            }
            ExecDone::Review(Err(e)) => {
                let detail = self.track_error("Couldn't update the email", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't update the email", &detail);
            }
            ExecDone::FormFill(Ok(undo)) => {
                let headline = Self::form_fill_headline(&undo.summary);
                self.card
                    .show_answer(headline, &with_pending_notices(&undo.summary), 0, None);
                self.last_form_undo = undo.is_restorable().then_some(undo);
            }
            ExecDone::FormFill(Err(e)) => {
                let detail = self.track_error("Couldn't fill the form", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't fill the form", &detail);
            }
            ExecDone::Generic {
                action,
                executor_name,
                proposal,
                result: Ok(undo),
            } => {
                // #441: a read-only `"none"` action's answer comes from the
                // proposal the card confirmed.
                let text = actions::generic_answer::result_card_text(
                    &action.name,
                    &executor_name,
                    &action.proposal,
                    &undo.summary,
                    &proposal,
                );
                let detail = with_pending_notices(&text.detail);
                let detail = with_fallback_footer(&detail, self.fallback_footer.as_deref());
                self.card
                    .show_model_answer(&text.headline, &detail, 0, None);
            }
            ExecDone::Generic {
                action,
                result: Err(e),
                ..
            } => {
                let title = format!("Couldn't run \"{}\"", action.name);
                let detail = self.track_error(&title, &format!("{e:#}"));
                self.card.show_error_with_details(&title, &detail);
            }
            ExecDone::Replace {
                action_name,
                report,
            } => {
                use actions::generic_replace::ReplaceReport;
                match report {
                    ReplaceReport::Replaced(summary) => {
                        self.card.show_answer(
                            "Selection replaced",
                            &with_pending_notices(&summary),
                            0,
                            None,
                        );
                    }
                    ReplaceReport::Copied(detail) => {
                        self.card.show_answer(
                            "Copied to the clipboard",
                            &with_pending_notices(&detail),
                            0,
                            None,
                        );
                    }
                    ReplaceReport::NothingToReplace => {
                        self.card.show_answer(
                            "Nothing to replace",
                            &with_pending_notices(""),
                            0,
                            None,
                        );
                    }
                    ReplaceReport::Failed(reason) => {
                        let title = format!("Couldn't run \"{action_name}\"");
                        let detail = self.track_error(&title, &reason);
                        self.card.show_error_with_details(&title, &detail);
                    }
                }
            }
            ExecDone::RestoreForm(Ok(())) => {
                self.card.show_answer(
                    "Form restored",
                    "The fields this filled have been put back.",
                    5,
                    None,
                );
            }
            ExecDone::RestoreForm(Err(e)) => {
                let detail = self.track_error("Couldn't fully restore the form", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't fully restore the form", &detail);
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
        // #712: the restore runs off the UI thread, so it needs the same
        // "one thing at a time" as every other run. Keep the undo for later.
        if self.busy {
            self.card.show_answer(
                "Still working",
                "Try Restore last form again once the current action has finished.",
                5,
                None,
            );
            self.set_watch(true);
            return;
        }
        let Some(undo) = self.last_form_undo.take() else {
            self.card.show_answer(
                "Nothing to restore",
                "No form fill that can be restored since Wingman started, or it was already restored.",
                5,
                None,
            );
            self.set_watch(true);
            return;
        };
        self.spawn_executor(Box::new(move || exec_work_restore(undo)));
    }
    /// #228: takes the worker's attribution into [`App::fallback_footer`].
    /// Called first in every action result handler, so a run that never
    /// reached a model (or failed) cannot show the previous run's footer.
    fn take_fallback_footer(&mut self) {
        self.fallback_footer = fallback_footer_for(crate::provider::take_attribution().as_ref());
    }

    fn on_result(&mut self, result: std::result::Result<Answer, String>) {
        self.take_fallback_footer();
        if let Err(packed) = &result {
            if self.quiet_send_cancel(packed) {
                self.set_busy(false);
                return;
            }
        }
        // #832: an empty chain is a setup problem, not a fault: the setup
        // card, no failed dot, no "Try again" text.
        if let Err(packed) = &result {
            if is_no_providers(packed) {
                let (headline, detail) = self.empty_chain_card_now();
                let chain = unpack_error(packed).1.to_string();
                let _ = self.record_last(Err(pack_error(&headline, &chain)));
                self.show_setup_card(&headline, &detail);
                return;
            }
        }
        let is_err = result.is_err();
        let mut answer = self.record_last(result);
        // A store failure never fails the request (#46); it rides on the
        // next card instead, once per run.
        // #101: the daily budget warning rides the same way.
        answer.detail = with_pending_notices(&answer.detail);
        if is_err {
            self.card
                .show_error_with_details(&answer.headline, &answer.detail);
        } else {
            let detail = with_fallback_footer(&answer.detail, self.fallback_footer.as_deref());
            self.card.show_model_answer(
                &answer.headline,
                &detail,
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
        self.set_busy(false);
        // The request that just finished may have spent money (#21).
        self.update_tooltip();
        let answer = match result {
            Ok(answer) => answer,
            Err(packed) => {
                // Review of #349/#426: the worker packed the human, redacted
                // detail together with the full, redacted chain so it
                // survives the thread boundary -- see `pack_error`'s doc
                // comment for why a tuple isn't used instead.
                let (human, chain) = unpack_error(&packed);
                let headline = first_line(human, 88);
                self.record_run_failure(&headline, human, chain);
                Answer {
                    headline,
                    detail: human.to_string(),
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
            Err(e) => {
                let detail = self.track_error("Couldn't copy", &format!("{e}"));
                self.card.show_error_with_details("Couldn't copy", &detail);
            }
        }
    }

    /// Issue #327: the one place `busy` changes, so the tray's working badge
    /// can never disagree with it. Starting a run also clears the previous
    /// run's failure badge ("until the next press").
    fn set_busy(&mut self, busy: bool) {
        self.busy = busy;
        BUSY_STUCK.with(|c| c.set(false));
        let hwnd = self.hwnd();
        unsafe {
            let _ = KillTimer(Some(hwnd), STUCK_TIMER_ID);
        }
        if busy {
            self.run_generation = self.run_generation.wrapping_add(1);
            CURRENT_RUN.with(|c| c.set(self.run_generation));
            unsafe {
                let _ = SetTimer(
                    Some(hwnd),
                    STUCK_TIMER_ID,
                    (crate::provider::worst_case_run_secs(self.chain.provider_count()) as u32)
                        .saturating_mul(1000),
                    None,
                );
            }
            forget_last_attribution();
            self.tray.set_failed(false);
        }
        self.tray.set_busy(busy);
        self.update_tooltip();
    }

    /// Review of #349/#426: records `raw_chain` (redacted, #253) as
    /// [`App::last_error`] and returns [`human_error_detail`]'s display
    /// text for it -- a drop-in replacement for calling
    /// `human_error_detail` directly at every call site that shows an
    /// error card, so "use Copy diagnostics for details" (the sentence
    /// every card detail ends with) is never a pointer at nothing: the very
    /// next "Copy diagnostics" click includes this chain, in memory only,
    /// never written to disk (Hard Rule 5) and gone the moment the process
    /// exits or another error overwrites it.
    fn record_last_error(&mut self, action: &str, raw_chain: &str) {
        self.last_error = Some(LastError {
            action: action.to_string(),
            occurred_at: SystemTime::now(),
            // #733: also text redaction and the account name, because this
            // chain is pasted into public bug reports. When the profile
            // folder cannot be found the name step is skipped, nothing else.
            chain: crate::diagnostics::scrub_error_chain_here(raw_chain),
        });
    }

    /// Issue #327: records a failed worker run (model or action) and, only
    /// if it was a real fault (see [`is_run_fault`]), raises the tray's red
    /// dot. A user-state message ("Nothing is selected") is recorded the same
    /// way but does not raise it.
    /// #832: the setup card for the current mode and providers.
    fn empty_chain_card_now(&self) -> (String, String) {
        let path = Config::path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "config.toml".into());
        empty_chain_card(
            self.config.mode,
            &self.config.providers,
            &path,
            self.config.load_problem.as_deref(),
        )
    }

    /// #832: for the action result handlers. When the run failed only
    /// because the chain was empty, shows the setup card (not a fault: no
    /// failed dot, no "Try again") and returns true. The Quick Ask palette's
    /// router hint already drops any failure quietly.
    fn show_empty_chain_failure(&mut self, packed: &str) -> bool {
        if !is_no_providers(packed) {
            return false;
        }
        let (headline, detail) = self.empty_chain_card_now();
        self.record_run_failure(&headline, &headline, unpack_error(packed).1);
        self.show_setup_card(&headline, &detail);
        true
    }

    /// #893: the one way to show a setup card. It persists until dismissed, so
    /// it also arms the click-away hook; a call site that showed it directly
    /// could forget, and the card would then only go away by a click on it.
    fn show_setup_card(&mut self, headline: &str, detail: &str) {
        self.card.show_settings_needed(headline, detail);
        self.set_watch(true);
    }

    fn record_run_failure(&mut self, action: &str, human: &str, raw_chain: &str) {
        self.record_last_error(action, raw_chain);
        if is_run_fault(human) {
            self.tray.set_failed(true);
            self.update_tooltip();
        }
    }

    fn track_error(&mut self, action: &str, raw_chain: &str) -> String {
        self.record_last_error(action, raw_chain);
        human_error_detail(raw_chain)
    }

    fn copy_diagnostics(&mut self) {
        let last_error = self
            .last_error
            .as_ref()
            .map(|e| crate::diagnostics::LastError {
                action: e.action.clone(),
                seconds_ago: SystemTime::now()
                    .duration_since(e.occurred_at)
                    .unwrap_or_default()
                    .as_secs(),
                chain: e.chain.clone(),
            });
        let report = crate::diagnostics::render_report(&crate::diagnostics::collect(
            &self.config,
            last_error,
        ));
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(report)) {
            Ok(()) => self.card.show_answer(
                "Diagnostics copied",
                "Paste them into a bug report.",
                6,
                None,
            ),
            Err(e) => {
                let detail = self.track_error("Couldn't copy diagnostics", &format!("{e}"));
                self.card
                    .show_error_with_details("Couldn't copy diagnostics", &detail);
            }
        }
    }

    /// Issue #425: "Copy details" on an expanded error card. Reuses
    /// [`App::last_error`] -- the same store #426's `copy_diagnostics`
    /// above reads -- rather than a second copy of the error, so the two
    /// "copy the raw chain" affordances can never disagree about what the
    /// most recent error's chain even was. `WM_APP_CARD_COPY_DETAILS`
    /// carries no payload (see that constant's doc comment): `Card` never
    /// holds the full raw chain itself, only the already-humanized
    /// `detail` text, so there is nothing to read off the message.
    fn copy_error_details(&mut self) {
        let Some(last_error) = self.last_error.as_ref() else {
            // No error recorded yet (e.g. a stale click/Enter reaching here
            // after `last_error` was somehow never set) -- nothing to copy,
            // say so plainly rather than copying nothing silently.
            self.card.show_answer(
                "Nothing to copy",
                "No error details are available.",
                4,
                None,
            );
            return;
        };
        let text = error_details_clipboard_text(last_error).to_string();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => {
                self.card
                    .show_answer("Details copied", "Paste them into a bug report.", 6, None)
            }
            Err(e) => {
                let detail = self.track_error("Couldn't copy details", &format!("{e}"));
                self.card
                    .show_error_with_details("Couldn't copy details", &detail);
            }
        }
    }

    /// #106: puts the whole local egress log on the clipboard, human
    /// readable. The on-disk format stays JSON lines (machine readable,
    /// ready for #46's SQLite import); nobody should have to read raw JSON
    /// pasted into a bug report, so `egress::read_all_human` renders it.
    ///
    /// This is the "see it" half of #106. The log has no page of its own
    /// because the WebView2 settings host does not exist yet (#44/#51);
    /// this is the same answer #124 gave diagnostics, and it is what makes
    /// `egress::read_all`/`render_human`/`read_all_human` reachable from a
    /// real user gesture rather than from tests only.
    fn copy_egress_log(&mut self) {
        let report = crate::diagnostics::egress_report();
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(report)) {
            Ok(()) => self.card.show_answer(
                "Egress log copied",
                "Every request this app has made, oldest first.",
                6,
                None,
            ),
            Err(e) => {
                let detail = self.track_error("Couldn't copy the egress log", &format!("{e}"));
                self.card
                    .show_error_with_details("Couldn't copy the egress log", &detail);
            }
        }
    }

    /// Issue #328: "Check this PC for local models". Reads RAM, GPU, battery
    /// and Ollama's GPU use once, on click, on a worker thread (the message
    /// thread also hosts the keyboard hook and must never wait on a
    /// socket), then `on_machine_check_result` shows the verdict. Nothing is
    /// kept and nothing polls.
    fn check_machine(&mut self) {
        use std::sync::atomic::Ordering;
        if MACHINE_CHECK_RUNNING.swap(true, Ordering::SeqCst) {
            return;
        }
        self.card
            .show_pending(crate::ui::card::PendingStage::Working);
        let target = self.hwnd_isize();
        let base_url = self.config.providers.ollama.base_url.clone();
        std::thread::spawn(move || {
            let facts = crate::machine::gather(&base_url);
            MACHINE_CHECK_RUNNING.store(false, Ordering::SeqCst);
            let payload = Box::into_raw(Box::new(facts));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_MACHINE_CHECK_RESULT,
                    WPARAM(0),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_MACHINE_CHECK_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    fn on_machine_check_result(&mut self, facts: &crate::machine::MachineFacts) {
        let verdict = crate::machine::verdict(facts);
        self.card
            .show_answer(&verdict.headline, &verdict.detail(), 20, None);
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
        if self.busy_now() {
            self.show_busy_card();
            return;
        }
        if pause::is_paused_now() {
            self.card
                .show_answer("Paused", "Resume from the tray menu to calculate.", 3, None);
            return;
        }

        if self.refuse_if_password_manager() {
            return;
        }

        self.set_busy(true);
        self.set_watch(false);
        // Issue #354 follow-up review, finding 3: this reads the current
        // text selection, not a screenshot -- `ReadingSelection` says so
        // truthfully instead of reusing a screenshot-flavored line.
        self.card
            .show_pending(crate::ui::card::PendingStage::ReadingSelection);

        let target = self.hwnd_isize();
        let run = self.run_generation;
        std::thread::spawn(move || {
            crate::ui::send_gate::set_worker_run(run);
            let result: std::result::Result<Answer, String> = catch_worker(|| {
                match crate::calc::run_on_selection(
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
                // `CalcError`'s `Display` is always plain arithmetic-parse
                // prose today (never Win32/API-derived -- see
                // `calc::run_on_selection`'s own doc comment), but every
                // path into `show_error` goes through the same humanizer
                // regardless (review of #349/#426): no exemption for "this
                // one is safe today" is how #349 happened in the first
                // place.
                crate::calc::SelectionCalcOutcome::Error(e) => {
                    Err(human_error_detail(&e.to_string()))
                }
            }
            });
            let payload = Box::into_raw(Box::new(result));
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                    Some(HWND(target as *mut _)),
                    WM_APP_RESULT,
                    WPARAM(run as usize),
                    LPARAM(payload as isize),
                )
                .is_err()
                {
                    // Window gone: nobody will take the box, so free it here.
                    free_boxed_payload(WM_APP_RESULT, LPARAM(payload as isize));
                }
            }
        });
    }

    fn start_learning(&mut self, which: usize) {
        let Some(hook) = &self.hook else {
            // Issue #356: same wording as the sibling install failure at
            // app start (above, around line 379), so re-binding a key hits
            // the same dead end with the same way out, instead of leaving
            // the user stuck with no next step.
            self.card.show_error(
                "Hotkeys unavailable",
                "The keyboard hook is not installed. Use Ask now from the tray menu instead.",
            );
            return;
        };
        hook.start_learning(which);
        self.card.show_answer(
            &format!("Press the {} now", slot_name(which)),
            "Whatever you press next becomes the binding. Modifiers on their own are ignored. Times out in 10 seconds.",
            11,
            None,
        );
    }

    fn on_learned(&mut self, which: usize, chord: Chord) {
        // #576: the same judge Settings' Change buttons use (#345), so the
        // tray refuses the same keys with the same words: a bare typing key,
        // a chord already bound to another slot (pause chord #202, palette,
        // primary vs secondary), and a known system shortcut (#112, press
        // again to confirm). A refusal keeps the previous binding.
        match crate::hotkey::judge_learned(
            &self.config.hotkeys,
            self.pending_conflict,
            which,
            chord,
        ) {
            LearnedVerdict::Cancel => {
                self.pending_conflict = None;
                self.card
                    .show_answer("Cancelled", "The shortcut is unchanged.", 6, None);
                return;
            }
            LearnedVerdict::Refuse(text) => {
                self.card
                    .show_answer(&text, "The previous binding is unchanged.", 8, None);
                return;
            }
            LearnedVerdict::Warn(text) => {
                self.pending_conflict = Some((which, chord));
                self.card
                    .show_answer(&text, "The previous binding is unchanged.", 8, None);
                return;
            }
            LearnedVerdict::Apply => self.pending_conflict = None,
        }
        apply_learned(&mut self.config.hotkeys, which, chord);
        if let Some(hook) = &self.hook {
            if which == HK_PAUSE {
                hook.set_pause_chord(self.config.hotkeys.pause);
            } else {
                hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
            }
        }
        let saved = self.config.save();
        self.refresh_tray_labels();

        let name = chord_to_string(&chord);
        match saved {
            Ok(()) => self
                .card
                .show_answer(&format!("Bound to {name}"), "", 4, None),
            Err(e) => {
                let headline = format!("Bound to {name}: not saved");
                let human = self.track_error(&headline, &format!("{e:#}"));
                self.card.show_error_with_details(
                    &headline,
                    &format!("It will work until you quit.\n\n{human}"),
                );
            }
        }
    }

    fn reload(&mut self) {
        match Config::load() {
            // #711: a file that does not parse must not replace the settings
            // already running; keep them and say what is wrong.
            Ok(config) if config.load_problem.is_some() => {
                let (headline, detail) =
                    config_problem_card(config.load_problem.as_deref().unwrap_or_default());
                self.card.show_error(&headline, &detail);
            }
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
            Err(e) => {
                let detail = self.track_error("Couldn't reload settings", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't reload settings", &detail);
            }
        }
        // Every card above but "Settings reloaded" persists.
        self.set_watch(true);
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
        self.open_settings_inner();
        // #673: every modal path ends here, whatever branch it took, so a
        // completed Save and the deferred messages are handled first and the
        // close request is honoured last.
        self.repost_close_if_requested();
    }

    fn open_settings_inner(&mut self) {
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

        if let Err(e) = commit_edited_config(&mut self.config, edited, Config::save) {
            // Rule 7: neither failure may be silently dropped. Every
            // deferred message is delivered for real (its executor runs,
            // its own card shows) even though the unrelated config save
            // just failed -- but the save error is the problem the user
            // just directly caused, so it is re-shown last, restoring it as
            // the single card slot's final content (`final_settings_card`'s
            // `SaveError` case, unchanged by #213).
            let had_pending = !pending.is_empty();
            let human = self.track_error("Couldn't save settings", &format!("{e:#}"));
            let detail = refused_save_detail(&human);
            self.card
                .show_error_with_details("Couldn't save settings", &detail);
            self.deliver_deferred(pending);
            if had_pending {
                self.card
                    .show_error_with_details("Couldn't save settings", &detail);
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

    /// #712: why Quit (or a `WM_CLOSE`) is refused right now, if it is.
    fn quit_blocked(&self) -> Option<&'static str> {
        quit_refusal(
            self.exec_in_flight,
            crate::provider::ollama_gpu_fix::is_running(),
        )
    }

    /// #673: if the owner window got `WM_CLOSE` while a nested loop (the tray
    /// menu, Settings or a file dialog) was running, re-post it now that the
    /// loop is over so it takes the normal path. Every place that clears
    /// `SETTINGS_OPEN` must call this once its own work is done (a test
    /// checks the count). Returns whether it did.
    fn repost_close_if_requested(&self) -> bool {
        if !take_close_request() {
            return false;
        }
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(self.hwnd()),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
        true
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
                DeferredMessage::PreviewDecided(generation) => self.on_preview_decided(generation),
                DeferredMessage::SendPreview(request) => self.on_send_preview(request),
                DeferredMessage::PendingStage(run, stage) => self.on_pending_stage(run, stage),
                DeferredMessage::FirstRun(run, found) => self.on_first_run_result(run, found),
                DeferredMessage::GenericActionResult(action, schema, result) => {
                    self.on_generic_action_result(action, schema, result)
                }
                DeferredMessage::ExecutorDone(result) => self.on_exec_done(result),
            }
        }
    }

    /// #722: the tray menu runs with the same reentrancy policy as Settings,
    /// so whatever it deferred is delivered here once the menu has closed.
    fn finish_modal_deferrals(&mut self) {
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
        self.deliver_deferred(pending);
        self.show_tray_restore_error(tray_restore_error);
        self.repost_close_if_requested();
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
        // #283: keep the process-wide mode mirror AND the tray's Mode
        // radio-check in sync with a Reload or a Settings save, the same
        // way `set_mode` already does for a tray-driven mode change.
        // Without this, `provider::common::offline_guard` (the only thing
        // enforcing Offline's non-loopback refusal for a by-name-local
        // provider like Ollama) keeps reading whatever mode was current at
        // the last full app restart, silently, until the next one.
        mode::set_current(self.config.mode);
        self.tray.set_mode(self.config.mode);
        // #105: keep the process-wide mirror in sync with a Reload or a
        // Settings save, the same way `set_mode` already does for the mode.
        // A hand-edited `[egress_preview]` otherwise needs a full restart.
        crate::config::set_egress_preview_enabled(self.config.egress_preview.enabled);
        crate::config::set_redact_text_enabled(self.config.capture.redact);
        crate::usage::set_daily_budget(
            self.config.budget.daily_usd,
            self.config.budget.warn_percent,
        );
        crate::ocr::set_preferred_language(&self.config.capture.ocr_language);
        self.card.set_text_scale(self.config.ui.text_scale);
        self.card
            .set_placement(self.config.ui.card_corner, &self.config.ui.card_monitor);
        if let Some(hook) = &self.hook {
            hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
            // #181: a Reload/Settings-save always re-syncs the pause chord
            // too, the same way it already does for primary/secondary --
            // otherwise a hand-edited `[hotkeys.pause]` in config.toml would
            // only take effect after a full app restart.
            hook.set_pause_chord(self.config.hotkeys.pause);
            // #269: same reasoning as the pause chord immediately above,
            // for the Quick Ask palette chord -- previously missing here,
            // so a hand-edited `[hotkeys.palette]` plus Reload kept the OLD
            // chord live in the keyboard hook until a full app restart.
            hook.set_palette_chord(self.config.hotkeys.palette);
        }
        self.refresh_tray_labels();
    }

    /// #174: `edit_settings` only needs to save when the file did not exist
    /// yet, and must open the shell only when that save (if attempted)
    /// actually succeeded -- otherwise `ShellExecuteW` opens a path that
    /// still does not exist, or exists with stale defaults, with nothing on
    /// screen to say so. Pure so this one-branch decision is unit-tested
    /// directly (AGENTS.md rule 8) rather than only through a live
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
            self.set_watch(true);
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
                let detail = self.track_error("Couldn't save settings", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't save settings", &detail);
                self.set_watch(true);
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
        self.commit_model_pick(&model);
    }

    /// #567: apply a model chosen from the tray's Ollama submenu. Same shape
    /// as [`App::pick_model`]; the config value written is the bare name, not
    /// the "(vision)" label.
    fn pick_ollama_model(&mut self, index: usize) {
        let Some(model) = self.tray.ollama_model_at(index).map(str::to_owned) else {
            return;
        };
        if self.config.providers.ollama.model == model {
            return;
        }
        self.config.providers.ollama.model = model.clone();
        self.commit_model_pick(&model);
    }

    /// The part of a tray model pick that comes after the config field is
    /// set: rebuild the chain so the pick is live, save, refresh the tray and
    /// say what happened on a card.
    fn commit_model_pick(&mut self, model: &str) {
        self.chain = Arc::new(self.config.build_chain());
        let saved = self.config.save();
        self.refresh_tray_labels();

        match saved {
            Ok(()) => self.card.show_answer(model, "", 3, None),
            Err(e) => {
                let headline = format!("Using {model}: not saved");
                let human = self.track_error(&headline, &format!("{e:#}"));
                self.card.show_error_with_details(
                    &headline,
                    &format!("It will revert when you quit.\n\n{human}"),
                );
            }
        }
        self.set_watch(true);
    }

    /// Move a provider to the front of `providers.order`, making it the one
    /// that answers. The other stays in the list as the fallback rather than
    /// being dropped, so switching never costs you the second provider.
    fn set_provider(&mut self, provider: ActiveProvider) {
        if !provider.move_to_front(&mut self.config.providers.order) {
            return;
        }

        self.chain = Arc::new(self.config.build_chain());
        let saved = self.config.save();
        self.refresh_tray_labels();

        let name = provider.label();
        match saved {
            // Issue #569: a provider with no key would fail on the next
            // press, so say so now and point at Settings.
            Ok(()) if provider_needs_key(provider, &self.config.providers, self.config.mode) => {
                let (headline, detail) = missing_key_card(provider);
                self.show_setup_card(&headline, &detail);
            }
            Ok(()) => self.card.show_answer(name, "", 3, None),
            Err(e) => {
                let headline = format!("Using {name}: not saved");
                let human = self.track_error(&headline, &format!("{e:#}"));
                self.card.show_error_with_details(
                    &headline,
                    &format!("It will revert when you quit.\n\n{human}"),
                );
            }
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
        // #793: the file did not parse, so the run is Offline until it does.
        if !mode_allowed(mode, self.config.load_problem.as_deref()) {
            let (headline, detail) =
                config_problem_card(self.config.load_problem.as_deref().unwrap_or_default());
            self.card.show_error(&headline, &detail);
            self.tray.set_mode(self.config.mode);
            self.set_watch(true);
            return;
        }
        self.config.mode = mode;
        mode::set_current(mode);
        self.tray.set_mode(mode);
        self.update_tooltip();

        let saved = self.config.save();
        match saved {
            Ok(()) => self.card.show_answer(mode.label(), "", 3, None),
            Err(e) => {
                let headline = format!("Using {}: not saved", mode.label());
                let human = self.track_error(&headline, &format!("{e:#}"));
                self.card.show_error_with_details(
                    &headline,
                    &format!("It will revert when you quit.\n\n{human}"),
                );
            }
        }
        self.set_watch(true);
    }

    /// #567: hand the tray the newest Ollama model list a background refresh
    /// has produced (if any), then start another so the next menu open is
    /// current. Called when the tray menu is about to open.
    fn sync_ollama_menu(&mut self) {
        let ollama = &self.config.providers.ollama;
        if let Some(pulled) = self.ollama_models.take_for(&ollama.base_url) {
            self.tray.set_ollama_models(&pulled, &ollama.model);
        }
        if ollama_menu_refresh_wanted(&self.config.providers.order, &ollama.base_url) {
            self.ollama_models.refresh(&ollama.base_url);
        }
    }

    fn refresh_tray_labels(&mut self) {
        let primary = chord_to_string(&self.config.hotkeys.primary);
        let secondary = chord_to_string(&self.config.hotkeys.secondary);
        self.tray.set_key_bindings(&primary, &secondary);
        self.tray
            .set_pause_binding(&pause_binding_label(self.config.hotkeys.pause.as_ref()));

        let p = &self.config.providers;
        self.tray
            .set_active_provider(ActiveProvider::shown_for(&p.order));
        self.tray.set_models(
            &p.openai.models,
            &p.openai.model,
            &p.anthropic.models,
            &p.anthropic.model,
        );
        // #567: the Ollama submenu keeps its last pulled list; only the
        // checked model follows the config. A fresh list is fetched in the
        // background (never here), see `OllamaModelCache`.
        self.tray.set_ollama_current(&p.ollama.model);
        if ollama_menu_refresh_wanted(&p.order, &p.ollama.base_url) {
            self.ollama_models.refresh(&p.ollama.base_url);
        }

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
            let base = format!("Wingman: {} · {primary}", ready.join(", "));
            match crate::usage::tooltip_part() {
                Some(spend) => format!("{base} · {spend}"),
                None => base,
            }
        };
        // Issue #327: lead with the state (working / failed / offline).
        let tip = crate::ui::tray::state_tooltip(self.tray.icon_state(), &tip);
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
                    let detail = self.track_error(
                        "Couldn't compute tomorrow's pause deadline",
                        &format!("{e:#}"),
                    );
                    self.card.show_error_with_details(
                        "Couldn't compute tomorrow's pause deadline",
                        &detail,
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
            Err(e) => Some(self.track_error("Couldn't restore the tray icon", &format!("{e:#}"))),
        }
    }

    fn set_watch(&mut self, on: bool) {
        let failed = match &self.watcher {
            Some(w) if on => !w.arm(),
            _ => false,
        };
        // Never overwrite a real earlier error: the card being shown may be it.
        if failed && self.last_error.is_none() {
            // Issue #729: no click-away for this card. Record it for
            // "Copy diagnostics" rather than failing the card itself.
            self.record_last_error(
                "Click-away dismissal unavailable",
                "SetWindowsHookExW(WH_MOUSE_LL) failed",
            );
        }
        if let Some(w) = &self.watcher {
            if !on {
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

    /// #117: hands the tray the actions the user wrote, for "Export action".
    /// Called as the menu opens; a missing or broken actions.toml shows none.
    fn sync_export_menu(&mut self) {
        let written = actions::load_actions()
            .map(|a| actions::share::export_candidates(&a))
            .unwrap_or_default();
        self.tray.set_export_actions(&written);
    }

    /// #117: runs an OS file dialog without `&mut App` being reachable from it.
    /// The dialog pumps this thread's messages, so `wnd_proc` can be entered
    /// while it is open; `SETTINGS_OPEN` makes that entry defer or ignore
    /// everything instead of forming a second `&mut App` (see its doc
    /// comment). What was deferred is delivered afterwards, as
    /// `open_settings` does. The closure must not touch `self`.
    fn run_os_dialog<T>(&mut self, dialog: impl FnOnce() -> T) -> T {
        // Like `open_settings`: no card timers or click watcher while the
        // nested loop runs.
        self.card.hide();
        self.set_watch(false);
        SETTINGS_OPEN.with(|c| c.set(true));
        let out = dialog();
        SETTINGS_OPEN.with(|c| c.set(false));
        let pending: VecDeque<DeferredMessage> =
            PENDING_MESSAGES.with(|c| std::mem::take(&mut *c.borrow_mut()));
        let tray_restore_error = if TASKBAR_RECREATED_WHILE_SETTINGS.with(|c| c.replace(false)) {
            self.try_restore_tray_icon()
        } else {
            None
        };
        if PAUSE_REEVALUATE_PENDING.with(|c| c.replace(false)) {
            self.reevaluate_pause(self.hwnd());
        }
        self.deliver_deferred(pending);
        self.show_tray_restore_error(tray_restore_error);
        self.repost_close_if_requested();
        out
    }

    /// #117: tray > Import action from file. Pick a file, check it, and show
    /// what it does on a preview card. Only "Do it" adds it to actions.toml
    /// (`on_preview_decided`); Esc or a click away adds nothing. Nothing runs.
    fn import_action(&mut self) {
        let owner = HWND(self.hwnd_isize() as *mut _);
        let Some(file) = self.run_os_dialog(|| crate::ui::file_dialog::pick_action_file(owner))
        else {
            return;
        };
        let prepared = (|| -> anyhow::Result<actions::share::Prepared> {
            let text = actions::share::read_import_file(&file)?;
            let import = actions::share::parse_import(&text)?;
            actions::share::prepare(&actions::path()?, import)
        })();
        match prepared {
            Ok(prepared) => {
                let value = prepared.preview_value();
                self.pending_preview = Some(PendingPreview::ImportAction(Box::new(prepared)));
                self.pending_preview_generation = self.card.show_preview(
                    "Add this action?",
                    &actions::share::preview_schema(),
                    &value,
                    false,
                );
            }
            Err(e) => {
                let detail = self.track_error("Couldn't import the action", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't import the action", &detail);
            }
        }
    }

    /// #117: the user said "Do it" on an import preview. Re-checks that
    /// actions.toml is still the file the preview was made against.
    fn commit_import(&mut self, prepared: &actions::share::Prepared) {
        let result = actions::path().and_then(|path| prepared.commit(&path));
        match result {
            Ok(()) => self.card.show_answer(
                "Action added",
                &format!("{} is in the Quick Ask list.", prepared.action.name),
                4,
                None,
            ),
            Err(e) => {
                let detail = self.track_error("Couldn't import the action", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't import the action", &detail);
            }
        }
    }

    /// #117: tray > Export action to file > (one of your actions).
    fn export_action(&mut self, index: usize) {
        let Some(id) = self.tray.export_action_at(index).map(str::to_owned) else {
            return;
        };
        let owner = HWND(self.hwnd_isize() as *mut _);
        let mut turned_off = false;
        let text = actions::load_actions().and_then(|all| {
            let found = all
                .iter()
                .find(|r| r.action.id == id && r.origin == actions::Origin::User)
                .ok_or_else(|| {
                    anyhow::anyhow!("the action \"{id}\" is no longer in actions.toml")
                })?;
            turned_off = actions::share::imports_turned_off(&found.action);
            actions::share::export_toml(&found.action)
        });
        let outcome = text.and_then(|text| {
            let picked = self.run_os_dialog(|| crate::ui::file_dialog::pick_save_path(owner, &id));
            let Some(path) = picked else {
                return Ok(None);
            };
            std::fs::write(&path, text)
                .map_err(|e| anyhow::anyhow!("could not write {}: {e}", path.display()))?;
            Ok(Some(()))
        });
        match outcome {
            Ok(Some(())) if turned_off => self.card.show_answer(
                "Action saved",
                "Its prompt is long, so it will be added turned off. Whoever imports it must read the prompt in actions.toml, then turn it on.",
                8,
                None,
            ),
            Ok(Some(())) => self.card.show_answer("Action saved", "", 3, None),
            Ok(None) => {}
            Err(e) => {
                let detail = self.track_error("Couldn't export the action", &format!("{e:#}"));
                self.card
                    .show_error_with_details("Couldn't export the action", &detail);
            }
        }
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
/// is `config.ui.prompt` exactly as before -- see the design spec's
/// "Origin tracking" section for why it is only the FALLBACK now, not the
/// only source: an `actions.toml` override of the default action's `prompt`
/// wins outright (Origin::User). The difficulty rating comes only from the
/// action's own `rate_difficulty` (#197).
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
) -> Result<Answer> {
    let resolved = actions::load_actions().context("failed to load actions")?;
    let default = actions::default_action(&resolved).with_context(|| {
        format!(
            "the default action (\"{}\") is disabled or missing; check actions.toml",
            actions::DEFAULT_ACTION_ID
        )
    })?;

    let (prompt, want_difficulty) = resolve_prompt_and_difficulty(default, ui_prompt);

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
    noting_attribution(chain.complete_parsed_with_fallback_attributed(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| parse_answer(&c.text),
    ))
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
    noting_attribution(chain.complete_parsed_with_fallback_attributed(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| actions::calendar::parse_calendar_proposal(&c.text),
    ))
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
    let target = captured.target();

    let req = match captured.text() {
        Some(text) => review_request_from_text(&action.prompt, text),
        None => review_request_from_screen(&action.prompt, shot),
    };

    let ollama_ready = mode == Mode::Auto
        && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
        && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
    let chain = providers.build_chain_for_mode(mode, ollama_ready);

    let proposal = noting_attribution(chain.complete_parsed_with_fallback_attributed(
        &req,
        || non_vision_inputs(raw, foreground_hwnd),
        |c| actions::review_email::parse_text_review_proposal(&c.text),
    ))?;

    Ok(actions::review_email::ReviewOutcome {
        proposal,
        original_text,
        target,
        source,
        cut: captured.was_cut(),
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
    raw: &mut Option<capture::RawShot>,
    redact: bool,
    foreground_hwnd: isize,
    require_tick_for: crate::config::RequireTickFor,
    use_model: bool,
) -> Result<Value> {
    // No added context: the store's own messages are the card text (a
    // profile sealed for another Windows user or PC says so, in plain words).
    let profile = actions::fill_form::load_or_import_profile()?;

    if actions::fill_form::profile_is_empty(&profile) {
        let toml_path = actions::fill_form::profile_toml_path()?;
        let data_dir = crate::store::data_dir().unwrap_or_default();
        return Ok(serde_json::json!({
            "empty_profile": true,
            "toml_path": toml_path.display().to_string(),
            "data_dir": data_dir.display().to_string(),
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

    if actions::fill_form::model_stage_needed(use_model, &unmapped) {
        let ollama_ready = mode == Mode::Auto
            && mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
            && mode::probe_ollama_ready(&providers.ollama.base_url, &providers.ollama.model);
        let chain = providers.build_chain_for_mode(mode, ollama_ready);

        // Encoded (and redacted) only here: with no model nothing is
        // encoded or sent.
        let raw = raw
            .as_mut()
            .context("no screen capture for the model request")?;
        let shot = encode_for_upload(raw, providers, mode, redact)?;
        let req = actions::fill_form::form_fill_request(&shot, &unmapped, &profile);
        let raw = &*raw;
        let responses = noting_attribution(chain.complete_parsed_with_fallback_attributed(
            &req,
            || non_vision_inputs(raw, foreground_hwnd),
            |c| actions::fill_form::parse_model_response(&c.text),
        ))?;
        mapped.extend(actions::fill_form::merge_model_response(
            &unmapped, &responses, &profile,
        ));
    }

    let proposal = actions::fill_form::build_proposal(&candidates, &mapped, require_tick_for);
    // Local-only run: say which fields were left alone.
    Ok(if use_model {
        proposal
    } else {
        actions::fill_form::with_unfilled_note(proposal, &unmapped)
    })
}

/// #103: the only way `app.rs` turns a capture into a [`crate::provider::Shot`].
/// Paints over private details in `raw` first when the user has not turned
/// that off and the mode's chain could send the pixels off this machine (see
/// `Providers::sends_screenshot_off_machine`); fails closed, so an OCR failure
/// is an `Err` here and the caller's card says nothing was sent. Worker
/// thread only: it blocks on OCR.
fn encode_for_upload(
    raw: &mut capture::RawShot,
    providers: &Providers,
    mode: Mode,
    redact: bool,
) -> Result<crate::provider::Shot> {
    capture::encode_for_chain(raw, providers, mode, redact)
}

/// #24: the intent router's worker-thread body, called from
/// `App::start_options_router`'s spawned thread. Mirrors [`worker`]'s
/// mode-aware chain construction (same Auto-mode Ollama probe, same
/// "network I/O never happens on the main thread" reasoning), but picks the
/// router's own cheapest ready provider/model
/// ([`router::cheapest_router_target`]) instead of the user's configured
/// chain, and runs it through a single-provider [`Chain`] -- issue #99's
/// one-shot repair pass still applies (a schema-invalid response gets one
/// repair attempt), but there is no multi-provider fallback: the router
/// only ever tries the one cheapest ready provider, and if that fails, the
/// options card shows its fallback options.
fn router_worker(
    providers: &Providers,
    mode: Mode,
    raw: &mut capture::RawShot,
    redact: bool,
    candidates: &[router::RouterCandidate],
    limits: router::RouterLimits,
) -> Result<router::RouterResult> {
    let started = std::time::Instant::now();
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

    let shot = encode_for_upload(raw, providers, mode, redact)?;
    // #105: the router runs by itself when the options card opens, so it never
    // shows the send preview; with the toggle on, a cloud request is refused
    // by the send guard instead and the card shows its fallback options.
    // #792: and it stops starting calls (the repair request included) once
    // the card has stopped waiting.
    let chain = router::router_chain(provider, started);
    let raw: &capture::RawShot = raw;
    router::route(&chain, shot.png, candidates, limits, || {
        router_non_vision_inputs(raw)
    })
}

/// Issue #295: the router's own (deliberately lighter) [`non_vision_inputs`]
/// -- OCR text of the router's own downscaled capture, and NO UIA snapshot.
/// Unlike `worker`/`calendar_worker`/`review_worker`/`form_fill_worker`, the
/// router has no `foreground_hwnd` handy at this call site (it runs off
/// `App::show_options_card`'s own capture, not `App::ask`'s), and its
/// schema (`router::router_schema`) never asks about editable-field
/// contents the way `fill_form` does -- so paying for a second COM
/// apartment thread just to snapshot UIA fields the router would never use
/// is pure waste. An OCR failure still fails this closure exactly like
/// `non_vision_inputs` (surfaced as a named skip reason to
/// `Chain::complete_parsed_with_fallback`'s caller); router failures are
/// silent by the time they reach the user regardless (see
/// `App::show_options_card`'s doc comment), so this never becomes a card.
fn router_non_vision_inputs(raw: &capture::RawShot) -> Result<crate::provider::NonVisionInputs> {
    let ocr_output = crate::ocr::recognize(
        &raw.rgba,
        raw.width,
        raw.height,
        crate::ocr::DEFAULT_TIMEOUT,
    )
    .context("OCR unavailable")?;
    Ok(crate::provider::NonVisionInputs {
        ocr_text: crate::ocr::serialize_lines(&ocr_output.lines),
        uia_fields: String::new(),
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

/// #551: posts a pending-card stage from a worker thread. `run` is the
/// `App::run_generation` the worker was started under. If the post fails the
/// box is freed here, so nothing leaks.
fn post_pending_stage(target: isize, run: u64, stage: crate::ui::card::PendingStage) {
    let payload = Box::into_raw(Box::new(stage));
    let posted = unsafe {
        windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(HWND(target as *mut _)),
            WM_APP_PENDING_STAGE,
            WPARAM(run as usize),
            LPARAM(payload as isize),
        )
    };
    if posted.is_err() {
        drop(unsafe { Box::from_raw(payload) });
    }
}

/// #243: `App::first_provider_model_label`'s implementation, pulled out to a
/// free function over `&Providers` (same split as `router_models_for`/
/// `provider_for_router` above) so it is testable without constructing a
/// whole `App`. Used to keep its own hand-mirrored match over
/// "openai"/"anthropic"/"gemini"/"ollama"/"compat:<name>" -- the same five
/// arms issue #222 already consolidated for the router and #201 for
/// diagnostics. Now a thin call into `Providers::describe`, the single
/// source of truth those two already read, so a provider kind missing an
/// arm here is no longer possible: there is no arm here to miss.
///
/// #354 follow-up: reads the first entry `mode` would actually try
/// (`Providers::selected_order_for_mode`, shared with `build_chain_for_mode`)
/// rather than `providers.order.first()` unfiltered.
fn first_provider_model_label(mode: Mode, providers: &Providers) -> Option<String> {
    let selected = providers.selected_order_for_mode(mode, true);
    let name = selected.first()?;
    let model = providers
        .describe(name)
        .map(|d| d.model)
        .unwrap_or_default();
    if model.is_empty() {
        Some(name.clone())
    } else {
        Some(format!("{name}:{model}"))
    }
}

/// #228: hands a chain result's attribution to the main thread (see
/// `provider::take_attribution`) and returns the bare value, so a worker
/// body reads the same as before the attribution existed.
fn noting_attribution<T>(result: Result<(T, crate::provider::Attribution)>) -> Result<T> {
    result.map(|(value, attribution)| {
        crate::provider::record_attribution(Some(attribution));
        value
    })
}

/// #486: the attribution (and its cost) is one global slot that the result
/// handler takes. A run whose result was dropped before the take would leave
/// its line for the next run, so every run starts by clearing it.
fn forget_last_attribution() {
    crate::provider::record_attribution(None);
}

/// #228, #486: the card footer for `attribution`, one line. It names the
/// answering model when a fallback answered, and says what the answer cost
/// when it was billed and priced. `None` for a free first-choice answer, an
/// unpriced one, or no model at all.
fn fallback_footer_for(attribution: Option<&crate::provider::Attribution>) -> Option<String> {
    let a = attribution?;
    let fallback = a
        .fallback()
        .map(|(answered, first)| palette_model::fallback_footer(answered, first));
    let cost = a.cost_usd.and_then(crate::usage::format_cost);
    match (fallback, cost) {
        (Some(f), Some(c)) => Some(format!("{f}. {c}")),
        (f, c) => f.or(c),
    }
}

/// #541: `detail` with each notice as a trailing paragraph.
fn append_notices(detail: &str, notices: Vec<String>) -> String {
    let mut out = detail.to_string();
    for notice in notices {
        out = if out.is_empty() {
            notice
        } else {
            format!("{out}\n\n{notice}")
        };
    }
    out
}

/// #541: the one place a result card asks for the notices queued for "the
/// next card": a store failure (#46), the daily budget warning (#101) and
/// the skipped-actions notice (#620).
/// Every handler that ends a run on a card calls this on its detail text
/// (a source-scanning test lists them), so a notice is shown once, on
/// whichever result card comes first, and never sits unseen.
fn with_pending_notices(detail: &str) -> String {
    append_notices(
        detail,
        [
            crate::store::take_notice(),
            crate::usage::take_budget_notice(),
            crate::config::take_backup_warning(),
            crate::actions::take_skipped_notice(),
        ]
        .into_iter()
        .flatten()
        .collect(),
    )
}
/// #228: `detail` with the footer as its last paragraph, or `detail`
/// unchanged when there is none.
fn with_fallback_footer(detail: &str, footer: Option<&str>) -> String {
    match footer {
        None => detail.to_string(),
        Some(f) if detail.is_empty() => f.to_string(),
        Some(f) => format!("{detail}\n\n{f}"),
    }
}

/// The first day of the week the user's Windows region sets (#336), for "this
/// week" and "next week". Monday when Windows cannot say. Win32, so checked by
/// hand (rule 8): set the region's first day of the week to Sunday and run
/// "Relative dates to real dates" on "Friday this week" on a Sunday; the date
/// moves a week.
fn local_week_start() -> actions::dates::WeekStart {
    use windows::core::PCWSTR;
    use windows::Win32::Globalization::{
        GetLocaleInfoEx, LOCALE_IFIRSTDAYOFWEEK, LOCALE_RETURN_NUMBER,
    };
    // With LOCALE_RETURN_NUMBER the answer is a DWORD written into the buffer.
    let mut buf = [0u16; 2];
    let written = unsafe {
        GetLocaleInfoEx(
            PCWSTR::null(),
            LOCALE_IFIRSTDAYOFWEEK | LOCALE_RETURN_NUMBER,
            Some(&mut buf),
        )
    };
    let value = if written > 0 {
        u32::from(buf[0]) | (u32::from(buf[1]) << 16)
    } else {
        0
    };
    actions::dates::week_start_from_locale(value)
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
/// timeout) fails this whole function -- AGENTS.md rule 7 wants that
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

    let uia_fields = uia_fields_text(foreground_hwnd);

    Ok(crate::provider::NonVisionInputs {
        ocr_text,
        uia_fields,
    })
}

/// The foreground window's UIA fields as compact text, or empty when UIA
/// fails (a hung app, a COM error): the walk runs on its own short-lived
/// thread, as `non_vision_inputs` always did, so it never blocks the caller
/// past `DEFAULT_BUDGET`.
fn uia_fields_text(foreground_hwnd: isize) -> String {
    std::thread::spawn(move || {
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
    .unwrap_or_default()
}

/// #104: the document text of the foreground browser or reader, read through
/// UI Automation on its own thread (its own COM apartment, like
/// [`uia_fields_text`]). `None` when the window exposes no readable page.
fn page_text_of(foreground_hwnd: isize) -> Option<crate::inputs::page_text::PageText> {
    std::thread::spawn(move || {
        crate::inputs::page_text::read_page_text(
            HWND(foreground_hwnd as *mut _),
            crate::inputs::page_text::DEFAULT_MAX_CHARS,
        )
    })
    .join()
    .ok()
    .flatten()
}

/// Why a generic action's worker ended without an answer (#493). A
/// `UserState` is the user's situation (nothing selected, a password field),
/// shown as its own plain message; a `Fault` is a real failure and keeps the
/// "use Copy diagnostics" next step.
enum GenericFailure {
    UserState(String),
    Fault(anyhow::Error),
}

/// What a generic action's worker sends back (#242): the model's proposal,
/// plus the selection to write it over when the action's executor is
/// `replace_text` and the selection can be written back safely (#68), or an
/// error packed with [`pack_error`].
type GenericResult = std::result::Result<
    (
        Value,
        Option<Box<actions::generic_replace::ReplaceSelection>>,
    ),
    String,
>;

impl From<anyhow::Error> for GenericFailure {
    fn from(e: anyhow::Error) -> Self {
        GenericFailure::Fault(e)
    }
}

/// Runs the pure input plan; whatever it refuses with (nothing selected, a
/// password field) is the user's situation, so it routes to `UserState`.
fn plan_generic_inputs(
    inputs: &[actions::InputKind],
    gathered: &actions::generic_inputs::Gathered,
) -> Result<actions::generic_inputs::RequestInputs, GenericFailure> {
    actions::generic_inputs::plan(inputs, gathered).map_err(GenericFailure::UserState)
}

/// Packs a [`GenericFailure`] for the result message (see [`pack_error`]).
fn pack_generic_failure(f: GenericFailure) -> String {
    match f {
        GenericFailure::UserState(message) => pack_error(&message, &message),
        GenericFailure::Fault(e) => pack_error(
            &human_error_detail(&format!("{e:#}")),
            &crate::egress::redact_opaque_tokens(&format!("{e:#}")),
        ),
    }
}

/// #127 step 1 of a linked-page action: the address to ask the user to
/// confirm. Looks in the selection and the clipboard (when the action lists
/// them), then at the screen through OCR (when it lists `screen`), and the
/// first source with a link decides: one link is offered, several are
/// refused as ambiguous, and a link the fetch rules refuse ends with the
/// reason. The model never picks the link. Fails first when the current mode
/// cannot fetch, so nobody is asked to confirm a fetch that would be refused.
fn link_to_confirm(
    inputs: &[actions::InputKind],
    foreground_hwnd: isize,
    raw: Option<&capture::RawShot>,
) -> Result<crate::inputs::web_page::Target, GenericFailure> {
    use actions::link_summary::{decide, NO_LINK_MESSAGE};
    if let Some(refusal) = crate::inputs::web_page::network_refusal() {
        return Err(GenericFailure::UserState(refusal));
    }
    let text_inputs: Vec<actions::InputKind> = inputs
        .iter()
        .copied()
        .filter(|k| {
            matches!(
                k,
                actions::InputKind::Selection | actions::InputKind::Clipboard
            )
        })
        .collect();
    let gathered = gather_generic_inputs(&text_inputs, foreground_hwnd, false)?;
    if gathered.selection_in_password_field {
        return Err(GenericFailure::UserState(
            actions::generic_inputs::PASSWORD_FIELD_MESSAGE.to_string(),
        ));
    }
    for text in [gathered.selection.as_deref(), gathered.clipboard.as_deref()] {
        if let Some(decision) = decide(text) {
            return decision.map_err(GenericFailure::UserState);
        }
    }
    if let (true, Some(raw)) = (inputs.contains(&actions::InputKind::Screen), raw) {
        let screen_text = ocr_text_of(raw)?;
        if let Some(decision) = decide(Some(&screen_text)) {
            return decision.map_err(GenericFailure::UserState);
        }
    }
    Err(GenericFailure::UserState(NO_LINK_MESSAGE.to_string()))
}

/// The text on the screenshot, read by OCR on a thread of its own (OCR and
/// UIA must not share a thread's COM apartment, see [`non_vision_inputs`]).
fn ocr_text_of(raw: &capture::RawShot) -> Result<String> {
    let output = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                crate::ocr::recognize(
                    &raw.rgba,
                    raw.width,
                    raw.height,
                    crate::ocr::DEFAULT_TIMEOUT,
                )
            })
            .join()
    })
    .map_err(|_| anyhow::anyhow!("the OCR thread stopped unexpectedly"))?
    .context("OCR unavailable")?;
    Ok(crate::ocr::serialize_lines(&output.lines))
}

/// #484: reads the text inputs a generic action's `inputs` list names
/// (selection, clipboard, UIA fields) using the existing readers, only the
/// ones listed. Runs on the worker thread (the selection and UIA readers
/// block against a slow app, see their module docs). An empty or unreadable
/// input yields nothing, and `actions::generic_inputs::plan` decides what
/// that means for the request. The selection reader's only error is a
/// clipboard that could not be restored after its Ctrl+C fallback, which is
/// reported as its own failure (rule 7), never as "nothing selected".
fn gather_generic_inputs(
    inputs: &[actions::InputKind],
    foreground_hwnd: isize,
    with_selection_target: bool,
) -> Result<actions::generic_inputs::Gathered, GenericFailure> {
    let mut gathered = actions::generic_inputs::Gathered::default();
    if actions::generic_inputs::wants_selection(inputs) {
        let selection = if with_selection_target {
            crate::inputs::selection::get_selection_foreground_with_target(
                foreground_hwnd,
                crate::inputs::selection::DEFAULT_MAX_CHARS,
                crate::inputs::selection::DEFAULT_CLIPBOARD_WAIT_BUDGET,
            )
        } else {
            crate::inputs::selection::get_selection_foreground(
                crate::inputs::selection::DEFAULT_MAX_CHARS,
                crate::inputs::selection::DEFAULT_CLIPBOARD_WAIT_BUDGET,
            )
        }
        .context("couldn't put your clipboard back after reading the selection")?;
        gathered.selection_target = selection.target.clone();
        gathered.selection_in_password_field =
            selection.source == crate::inputs::selection::SelectionSource::SkippedPasswordField;
        gathered.selection_truncated = selection.truncated;
        gathered.selection = Some(selection.text);
    }
    if actions::generic_inputs::wants_clipboard(inputs) {
        if let Ok(text) = arboard::Clipboard::new().and_then(|mut c| c.get_text()) {
            let (text, truncated) = crate::inputs::selection::truncate_bounded(
                &text,
                crate::inputs::selection::DEFAULT_MAX_CHARS,
            );
            gathered.clipboard = Some(text);
            gathered.clipboard_truncated = truncated;
        }
    }
    if actions::generic_inputs::wants_uia(inputs) {
        gathered.uia = Some(uia_fields_text(foreground_hwnd));
    }
    // #104: the page is read after the selection, because a selection wins.
    if actions::generic_inputs::needs_page_text(inputs, &gathered) {
        if let Some(page) = page_text_of(foreground_hwnd) {
            gathered.page_text = Some(page.text);
            gathered.page_text_truncated = page.truncated;
        }
    }
    // #500: a text input that comes back empty falls back to the screenshot,
    // which could show a focused password field. Look before that happens,
    // and only then (after every text input has been read).
    if actions::generic_inputs::needs_focus_probe(inputs, &gathered) {
        gathered.focus_in_password_field = crate::inputs::selection::focused_is_password();
    }
    Ok(gathered)
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
/// - The difficulty flag: only the resolved action's own `rate_difficulty`.
///   The old global `ui.show_difficulty` setting is gone (#197).
fn resolve_prompt_and_difficulty<'a>(
    default: &'a actions::Resolved,
    ui_prompt: &'a str,
) -> (&'a str, bool) {
    let prompt = match default.origin {
        actions::Origin::User => default.action.prompt.as_str(),
        actions::Origin::Builtin => ui_prompt,
    };
    (prompt, default.action.rate_difficulty)
}

/// Issue #181: which action a pause-toggle-chord press should take. Pure
/// (just a bool in, an enum out) so the toggle direction is unit-tested
/// directly (AGENTS.md rule 8) without a real `App` -- `App::toggle_pause`
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

// -- #349: human-readable, redacted error text for the card ----------------
//
// Most `show_error` calls used to pass anyhow's alternate `Display` output
// straight through: the whole context chain, which for a Win32-backed path
// can read
// "TzSpecificLocalTimeToSystemTime failed" -- a crash-log line, not
// something a user can act on. Every place in this file that turns an
// error into card-visible text now routes through `human_error_detail`
// (or, when it needs to splice the sentence into a larger message,
// `humanize_error_chain` directly) instead of interpolating `{e:#}`/`{e}`
// itself; `show_error_detail_never_bypasses_the_humanizer` below is the
// mechanical guard that nothing new regresses this.

/// Known technical fragments from anyhow context chains (see `app.rs`'s own
/// `.context("... failed")` call sites, e.g. `local_today_and_utc_offset`),
/// mapped to a specific human sentence. Matched by substring against the
/// already-redacted chain, so it survives whatever context wraps it.
const KNOWN_TECHNICAL_DETAILS: &[(&str, &str)] = &[
    (
        crate::redact::FAILURE_NEEDLE,
        crate::redact::FAILURE_SENTENCE,
    ),
    (
        crate::redact::ADMIN_APP_SENTENCE,
        crate::redact::ADMIN_APP_SENTENCE,
    ),
    (
        "TzSpecificLocalTimeToSystemTime",
        "Couldn't read your clock settings.",
    ),
    ("FileTimeToSystemTime", "Couldn't read your clock settings."),
    ("SystemTimeToFileTime", "Couldn't read your clock settings."),
    (
        crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT,
        "The other app stopped responding.",
    ),
];

/// True if `text` contains what looks like a Win32/API-style identifier
/// immediately (optionally via a `(...)` argument list) followed by the
/// word "failed" -- the exact shape every `.context("XxxYyy(...) failed")`
/// call in this codebase produces (`SetWindowsHookExW(WH_MOUSE_LL) failed`,
/// `TzSpecificLocalTimeToSystemTime failed`, `GetMonitorInfoW failed`).
/// Deliberately narrow: an ordinary English sentence ending "...the save
/// failed" or "write failed" has at most one capitalized, non-technical
/// word before "failed" and is left alone. This is #349's detector, and
/// `show_error_call_sites_never_show_a_failed_suffixed_api_identifier`
/// below is its own "Done when" regression test.
fn contains_api_failed_identifier(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    for (i, word) in words.iter().enumerate() {
        if word.trim_end_matches(|c: char| ".,;:!".contains(c)) != "failed" || i == 0 {
            continue;
        }
        let prev = words[i - 1].trim_end_matches(|c: char| ")(:,.".contains(c));
        let ident = prev.split('(').next().unwrap_or(prev);
        if is_api_shaped_identifier(ident) {
            return true;
        }
    }
    false
}

fn is_api_shaped_identifier(ident: &str) -> bool {
    if ident.len() < 4 {
        return false;
    }
    if !ident.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return false;
    }
    let starts_upper = ident.chars().next().is_some_and(|c| c.is_ascii_uppercase());
    let upper_count = ident.chars().filter(|c| c.is_ascii_uppercase()).count();
    // Win32/API names are PascalCase with several internal capitals
    // ("TzSpecificLocalTimeToSystemTime", "SetWindowsHookExW"); an ordinary
    // English word before "failed" is lowercase, or has at most the one
    // leading capital a sentence-initial word would.
    starts_upper && upper_count >= 2
}

// -- review of #349/#426: a wider, self-tested source scanner --------------
//
// The original scanner only matched two exact `format!` literals: the
// alternate-Display specifier on the bound name `e`, and plain Display on
// the same name. It missed the Debug specifier, the `err`/`error` bound
// names just as many `Err(err) => ...`/`.map_err(|error| ...)` arms use,
// a bare call to the error's own `ToString` (no `format!` at all), and any
// whitespace variant inside the macro call. No `regex` dependency: this
// crate has none today and these shapes are simple enough to hand-scan.

// Test-only: exercised solely by `show_error_call_sites_never_show_a_failed_suffixed_api_identifier`
// and the `find_error_interpolations_*`/`interpolation_is_guarded_*` self-tests
// below, never by production code, hence `#[cfg(test)]` on every item in
// this section (otherwise a release build's dead-code lint flags all of it).
#[cfg(test)]
const ERROR_BINDING_NAMES: [&str; 3] = ["e", "err", "error"];
#[cfg(test)]
const ERROR_FORMAT_SPECS: [&str; 3] = ["", ":#", ":?"];

#[cfg(test)]
fn skip_ws(s: &str, mut i: usize) -> usize {
    let bytes = s.as_bytes();
    while i < bytes.len() && (bytes[i] as char).is_whitespace() {
        i += 1;
    }
    i
}

/// Byte offsets (into `src`, which the caller must have already normalized
/// to `\n` line endings -- see
/// `show_error_call_sites_never_show_a_failed_suffixed_api_identifier`
/// below) of every `format!` call whose one placeholder is exactly one of
/// `e`, `err` or `error`, with the alternate-Display, plain-Display or
/// Debug specifier (arbitrary whitespace around the macro's parens and
/// string tolerated), and every direct `.to_string()` call on one of those
/// same three bound names. Each of these is a way to turn an error into a
/// `String` by interpolating its Display/Debug output directly -- #349's
/// whole bug.
#[cfg(test)]
fn find_error_interpolations(src: &str) -> Vec<usize> {
    let bytes = src.as_bytes();
    let mut hits = Vec::new();

    // format!( "{name[:spec]}" )
    let mut i = 0;
    while let Some(rel) = src[i..].find("format!") {
        let start = i + rel;
        let mut j = skip_ws(src, start + "format!".len());
        if bytes.get(j) != Some(&b'(') {
            i = start + 1;
            continue;
        }
        j = skip_ws(src, j + 1);
        if bytes.get(j) != Some(&b'"') {
            i = start + 1;
            continue;
        }
        let str_start = j + 1;
        let Some(close_rel) = src[str_start..].find('"') else {
            break;
        };
        let inner = &src[str_start..str_start + close_rel];
        let after_quote = str_start + close_rel + 1;
        let matched = ERROR_BINDING_NAMES.iter().any(|name| {
            ERROR_FORMAT_SPECS
                .iter()
                .any(|spec| inner == format!("{{{name}{spec}}}"))
        });
        if matched && bytes.get(skip_ws(src, after_quote)) == Some(&b')') {
            hits.push(start);
        }
        i = str_start + 1;
    }

    // name.to_string()
    for name in ERROR_BINDING_NAMES {
        let needle = format!("{name}.to_string()");
        let mut i = 0;
        while let Some(rel) = src[i..].find(needle.as_str()) {
            let pos = i + rel;
            let prev_is_ident =
                pos > 0 && (bytes[pos - 1].is_ascii_alphanumeric() || bytes[pos - 1] == b'_');
            if !prev_is_ident {
                hits.push(pos);
            }
            i = pos + needle.len();
        }
    }

    hits.sort_unstable();
    hits
}

/// True if the statement containing byte offset `pos` in `src` mentions
/// `human_error_detail(` or `track_error(` before that offset -- both
/// redact (#253) before ever building card-visible text, and `track_error`
/// additionally records the chain for "Copy diagnostics" (review of
/// #349/#426). "Statement" is approximated as the text since the nearest
/// preceding `;`, `{` or `}` OUTSIDE a string literal. That last part is
/// not optional: a naive brace search trips on the literal `{`/`}`
/// characters inside `format!`'s own `"{e:#}"` placeholder, which -- in a
/// statement with two `format!` calls, e.g. `pack_error`'s -- would find
/// the first call's closing `}` and cut `human_error_detail(` out of the
/// text being searched for the second call's guard check (caught by
/// `interpolation_is_guarded_sees_past_an_earlier_format_placeholder`
/// below, which is exactly this shape).
#[cfg(test)]
fn interpolation_is_guarded(src: &str, pos: usize) -> bool {
    let before = &src[..pos];
    let mut in_string = false;
    let mut escape_next = false;
    let mut stmt_start = 0;
    for (i, c) in before.char_indices() {
        if escape_next {
            escape_next = false;
            continue;
        }
        if in_string {
            match c {
                '\\' => escape_next = true,
                '"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            ';' | '{' | '}' => stmt_start = i + c.len_utf8(),
            _ => {}
        }
    }
    let stmt = &before[stmt_start..];
    stmt.contains("human_error_detail(") || stmt.contains("track_error(")
}

/// Redacts key-shaped tokens (#253, via the same `egress::redact_opaque_tokens`
/// the egress log already uses) from a raw anyhow context chain -- already
/// rendered with anyhow's alternate `Display`, here or forwarded across a
/// thread boundary as a plain `String` -- and returns just the human
/// sentence(s), with no
/// trailing next-step, so a caller that already has its own can append it.
/// A chain containing a Win32/API `"Xxx failed"`-shaped identifier is
/// replaced outright (by a known mapping, or a generic sentence); anything
/// else already reads as prose and is kept, still redacted. Pure and
/// unit-tested directly (`humanize_error_chain_*` tests below) rather than
/// only indirectly through whichever call site happens to exercise it.
fn humanize_error_chain(raw_chain: &str) -> String {
    // Detection runs against the RAW chain, before redaction: an API
    // identifier like "TzSpecificLocalTimeToSystemTime" is itself long
    // enough and mixed-case enough to trip `redact_opaque_tokens`'
    // key-shaped-token heuristic, which would otherwise erase the very
    // text `KNOWN_TECHNICAL_DETAILS` and `contains_api_failed_identifier`
    // need to see. That is harmless here: on every branch below the raw
    // text itself is discarded in favour of a hand-written human sentence,
    // never echoed back, so redacting it first would only have hidden it
    // from our own matching, not protected the user.
    for (needle, sentence) in KNOWN_TECHNICAL_DETAILS {
        if raw_chain.contains(needle) {
            return (*sentence).to_string();
        }
    }
    if contains_api_failed_identifier(raw_chain) {
        return "Something went wrong.".to_string();
    }
    // Only reached for a chain that never looked API-shaped, i.e. is
    // presumed to already be human prose (an io::Error message, a plain
    // `anyhow!("...")`) -- still redacted before display, per #253.
    crate::egress::redact_opaque_tokens(raw_chain)
        .trim()
        .to_string()
}

/// #261/#264 code review follow-up: a `UIA_E_TIMEOUT` can surface from any
/// bare UIA call, not only the handful `inputs::uia_automation::
/// describe_timeout` wraps by hand (e.g. `IUIAutomationTextPattern::
/// GetSelection`/`IUIAutomationTextRange::GetText` in
/// `inputs::selection::com::probe_from_element`/`selection_identity`,
/// which have no per-call wrapping). Requiring every current and future
/// UIA call site to opt in is the kind of thing a future call is likely to
/// miss; walking `e`'s whole `anyhow` context chain here instead, looking
/// for the underlying `windows::core::Error` and comparing its `code()`
/// directly to `UIA_E_TIMEOUT`, cannot miss a site the way a fixed string
/// fragment could (a timed-out call's `Display` text is whatever the OS
/// happens to have registered for that HRESULT, not something this crate
/// controls). Every worker-thread `pack_error` call site in this file
/// calls this instead of formatting `e` directly, so the result always
/// still goes through [`human_error_detail`]/`redact_opaque_tokens` same
/// as before.
fn worker_error_chain_text(e: &anyhow::Error) -> String {
    for cause in e.chain() {
        if let Some(win_err) = cause.downcast_ref::<windows::core::Error>() {
            if crate::inputs::uia_automation::is_uia_timeout(win_err) {
                return crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT.to_string();
            }
        }
    }
    // Positional, not `{e:#}`: this fallback's *return value* already
    // always goes through `human_error_detail` at every call site below
    // (never displayed directly), so it is not one of the raw-interpolation
    // shapes `find_error_interpolations`'s scanner exists to catch -- using
    // `{e:#}`'s named-binding form here would trip that scanner as a false
    // positive on this file's own already-guarded plumbing.
    format!("{:#}", e)
}

/// The common card-detail shape (#349): `humanize_error_chain`'s sentence
/// plus a next step, for the many call sites that pass
/// `human_error_detail`'s output straight to `show_error`'s `detail`.
fn human_error_detail(raw_chain: &str) -> String {
    format!("{} {DIAGNOSTICS_STEP}", humanize_error_chain(raw_chain))
}

/// The next step every real fault's card detail ends with.
const DIAGNOSTICS_STEP: &str =
    "Try again, and if it keeps happening, use Copy diagnostics for details.";

/// Issue #327: whether a failed run's human text came from a real fault (a
/// model or action error, built by [`human_error_detail`]) rather than a
/// user-state message such as "Nothing is selected", which is plain text with
/// no diagnostics step (#493). Only real faults raise the tray's failed dot.
fn is_run_fault(human: &str) -> bool {
    human.ends_with(DIAGNOSTICS_STEP)
}

/// Separator `pack_error`/`unpack_error` use. A control character no real
/// error message or human sentence will ever contain, so a plain
/// `split_once` is exact.
const ERROR_PACK_SEP: char = '\u{1}';

/// Review of #349/#426: a worker thread builds a `Result<T, String>` that
/// crosses into the main thread's message loop as a boxed, type-erased
/// pointer (`WM_APP_CALENDAR_RESULT` and friends) -- changing that `String`
/// to a tuple would mean re-deriving every `unsafe { Box::from_raw::<...>() }`
/// cast at each of those call sites exactly right, which is a correctness
/// risk this fix does not need to take. Packing both the human, redacted
/// detail and the full, redacted chain into one `String` (split back apart
/// by [`unpack_error`] in the `on_*_result` handler that already runs on
/// the main thread, with access to `self`) gets the same result -- the
/// full chain survives to [`App::track_error`] -- without touching any of
/// that unsafe plumbing.
fn pack_error(human: &str, chain: &str) -> String {
    format!("{human}{ERROR_PACK_SEP}{chain}")
}

/// The other half of [`pack_error`]. Defensive against a string that was
/// never packed (falls back to using the same text for both halves) so a
/// caller can never panic on this, even if some future path started
/// building the `Result<T, String>` a different way.
fn unpack_error(packed: &str) -> (&str, &str) {
    packed
        .split_once(ERROR_PACK_SEP)
        .unwrap_or((packed, packed))
}

/// #712: what a confirmed action's executor produced, built on the worker
/// thread and handed to the UI thread as `WM_APP_EXECUTOR_DONE`. An error
/// stays an `anyhow::Error` until the card code, which records its full
/// chain through `track_error`.
enum ExecDone {
    Calendar(anyhow::Result<executors::Undo>),
    Review(anyhow::Result<executors::Undo>),
    FormFill(anyhow::Result<executors::Undo>),
    /// A non-built-in action's executor. Carries the action and the
    /// confirmed proposal because the card text is built from both.
    Generic {
        action: Box<actions::Action>,
        executor_name: String,
        proposal: Value,
        result: anyhow::Result<executors::Undo>,
    },
    Replace {
        action_name: String,
        report: actions::generic_replace::ReplaceReport,
    },
    RestoreForm(anyhow::Result<()>),
}

impl std::fmt::Debug for ExecDone {
    // `Undo` holds a closure and has no `Debug`; the variant is all a log
    // or an assertion needs.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ExecDone::Calendar(_) => "ExecDone::Calendar",
            ExecDone::Review(_) => "ExecDone::Review",
            ExecDone::FormFill(_) => "ExecDone::FormFill",
            ExecDone::Generic { .. } => "ExecDone::Generic",
            ExecDone::Replace { .. } => "ExecDone::Replace",
            ExecDone::RestoreForm(_) => "ExecDone::RestoreForm",
        })
    }
}

fn exec_work_calendar(
    executor: &dyn executors::Executor,
    confirmed: confirm::Confirmed<Value>,
) -> ExecDone {
    ExecDone::Calendar(executor.execute(confirmed))
}

fn exec_work_form_fill(
    executor: &dyn executors::Executor,
    confirmed: confirm::Confirmed<Value>,
) -> ExecDone {
    ExecDone::FormFill(executor.execute(confirmed))
}

fn exec_work_review(
    executor: &dyn executors::Executor,
    ctx: &actions::review_email::ReviewContext,
) -> ExecDone {
    ExecDone::Review(actions::review_email::do_review_confirmed(executor, ctx))
}

fn exec_work_generic(
    executor: Box<dyn executors::Executor>,
    action: actions::Action,
    confirmed: confirm::Confirmed<Value>,
) -> ExecDone {
    // #441: `execute` consumes `confirmed`, so keep the value the card text
    // may need (a read-only `"none"` action's answer).
    let proposal = confirmed.value().clone();
    let result = executor.execute(confirmed);
    ExecDone::Generic {
        action: Box::new(action),
        executor_name: executor.name().to_string(),
        proposal,
        result,
    }
}

fn exec_work_replace(
    action_name: String,
    replace: Box<dyn executors::Executor>,
    copy: Box<dyn executors::Executor>,
    plan: Option<Box<actions::generic_replace::ReplaceSelection>>,
    confirmed: confirm::Confirmed<Value>,
) -> ExecDone {
    let report = actions::generic_replace::replace_or_copy(
        replace.as_ref(),
        copy.as_ref(),
        plan.as_deref(),
        confirmed,
    );
    ExecDone::Replace {
        action_name,
        report,
    }
}

fn exec_work_restore(undo: executors::Undo) -> ExecDone {
    ExecDone::RestoreForm(undo.undo())
}

/// #712: why Quit is refused right now, if it is. An executor worker that is
/// mid-write (typing into a form) is never killed; the user tries again. The
/// same for "Fix it" on the Ollama GPU setting: it stops and restarts Ollama,
/// and quitting between the two would leave Ollama stopped (#795).
fn quit_refusal(exec_in_flight: bool, gpu_fix_running: bool) -> Option<&'static str> {
    if exec_in_flight {
        Some("Wingman is still typing into a form; try again in a moment.")
    } else if gpu_fix_running {
        Some("Wingman is still restarting Ollama; try again in a moment.")
    } else {
        None
    }
}

/// Issue #227: runs a worker's whole computation and turns a panic into the
/// same packed error a failed run produces, so the result message is always
/// posted and `busy` is always cleared. Only matters where panics unwind
/// (dev and test builds); a release build aborts, and the crash note
/// (`crash_note.rs`) covers that instead.
fn catch_worker<T>(
    work: impl FnOnce() -> std::result::Result<T, String>,
) -> std::result::Result<T, String> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(work)) {
        Ok(result) => result,
        Err(payload) => {
            let profile = crate::known_folder::user_profile()
                .map(|p| p.display().to_string())
                .unwrap_or_default();
            Err(packed_panic_error(
                &crate::crash_note::message_of(payload.as_ref()),
                &crate::crash_note::take_location(),
                &profile,
            ))
        }
    }
}

/// A caught panic as a [`pack_error`] string: a fixed human sentence (never
/// the panic text) and a chain for "Copy details" that names the location
/// and, only for a string-literal message, the message
/// ([`crate::crash_note::message_of`] drops formatted ones, which can carry
/// selection, OCR or model text).
fn packed_panic_error(message: &str, location: &str, profile: &str) -> String {
    let mut chain = String::from("Wingman hit an internal error");
    if !location.is_empty() {
        chain.push_str(&format!(
            " in {}",
            crate::crash_note::sanitize(location, profile, 120)
        ));
    }
    if !message.is_empty() {
        chain.push_str(&format!(
            ": {}",
            crate::crash_note::sanitize(message, profile, crate::crash_note::MAX_MESSAGE_CHARS)
        ));
    }
    pack_error(
        &human_error_detail("Wingman hit an internal error."),
        &chain,
    )
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
    /// #673: the owner window got `WM_CLOSE` while Settings was open. Read
    /// once by `open_settings` after the modal loop returns.
    static CLOSE_AFTER_SETTINGS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

    /// True while the tray menu's loop runs (#722). Always set together with
    /// `SETTINGS_OPEN`, which supplies the reentrancy machinery; this only
    /// selects [`tray_menu_policy`] instead of the Settings one.
    static TRAY_MENU_OPEN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };

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

    /// #723: the run generation currently in flight, mirrored from
    /// `App::run_generation` so `wnd_proc` can drop a stale worker result
    /// before it forms `&mut App`.
    static CURRENT_RUN: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };

    /// #723: the in-flight run outlasted its bound; the next press abandons
    /// it. Thread-local for the same reason as `SETTINGS_OPEN`.
    static BUSY_STUCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

use std::os::windows::ffi::OsStrExt;

/// One message deferred by [`SettingsReentrancy::Defer`] while Settings was
/// open (issue #213), holding whatever payload its `WM_APP_*` counterpart
/// boxed into `lparam` -- already taken out of that box, so `PENDING_MESSAGES`
/// never stores a raw pointer.
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
    /// `WM_APP_PREVIEW_DECIDED`. The decision itself lives on `Card`/`App`
    /// (`take_confirmed`/`pending_review`), read fresh when this is finally
    /// delivered by `on_preview_decided`; the `u32` is #225's preview
    /// generation, carried through from the message's `WPARAM` so a
    /// deferred notification is still matched against the right preview
    /// when Settings closes and the queue drains.
    PreviewDecided(u32),
    /// `WM_APP_SEND_PREVIEW`, deferred only while the tray menu is open
    /// (#722), so opening the menu never cancels a send waiting on its
    /// preview.
    SendPreview(crate::ui::send_gate::SendRequest),
    /// `WM_APP_PENDING_STAGE` (run, stage), tray menu only (#722).
    PendingStage(u64, crate::ui::card::PendingStage),
    /// `WM_APP_FIRST_RUN` (run, probe answer), tray menu only (#722).
    FirstRun(u64, FirstRun),
    /// `WM_APP_GENERIC_ACTION_RESULT` (#242). Carries the exact `Action`
    /// and schema `run_generic_action` dispatched with, not just the id --
    /// see `on_generic_action_result`'s doc comment for why.
    GenericActionResult(actions::Action, Value, GenericResult),
    /// `WM_APP_EXECUTOR_DONE` (#712).
    ExecutorDone(std::result::Result<ExecDone, String>),
}

/// Marks the single preview slot as the calendar add, replacing whatever was
/// there. A replaced `Send` drops its channel, which cancels that send.
fn arm_calendar_slot(slot: &mut Option<PendingPreview>) {
    *slot = Some(PendingPreview::Calendar);
}

/// Empties the slot if it holds a send preview, which wakes the blocked
/// worker with no authorization. Returns whether it did. Any other pending
/// preview is left alone.
fn take_send_slot(slot: &mut Option<PendingPreview>) -> bool {
    if matches!(slot, Some(PendingPreview::Send(_))) {
        *slot = None;
        true
    } else {
        false
    }
}

/// Whether a worker's packed error is the user cancelling the send preview.
/// Looks at the raw chain half, which keeps the cancel text verbatim.
fn is_send_cancelled(packed: &str) -> bool {
    unpack_error(packed)
        .1
        .contains(crate::provider::SEND_CANCELLED_TEXT)
}

/// #832: whether a worker's packed error is the empty chain ("no providers
/// configured"). Looks at the raw chain half.
fn is_no_providers(packed: &str) -> bool {
    unpack_error(packed)
        .1
        .contains(crate::provider::NO_PROVIDERS_TEXT)
}

/// #832: the setup card for a run that found no provider to try. The gate
/// cannot probe Ollama, so it lets an Ollama-only setup through; when the
/// worker then finds nothing answering, this says what to do. Any other
/// setup gets the gate's own card.
pub(crate) fn empty_chain_card(
    mode: Mode,
    providers: &Providers,
    config_path: &str,
    load_problem: Option<&str>,
) -> (String, String) {
    // #892: the defaults standing in for an unreadable file are not what the
    // user needs to hear about.
    if let Some(problem) = load_problem {
        return broken_file_card(problem, config_path);
    }
    if mode == Mode::Auto
        && crate::mode::should_probe_ollama(&providers.order, &providers.ollama.base_url)
    {
        return (
            "Ollama is not running".to_string(),
            "Start Ollama, or click here to open Settings and paste an API key.".to_string(),
        );
    }
    App::readiness_gate(mode, providers, config_path).unwrap_or_else(|| {
        (
            "No AI model set up yet".to_string(),
            "Click here to open Settings and paste an API key, or install Ollama to run models on this PC for free.".to_string(),
        )
    })
}

/// Issue #225: which action is waiting on the preview currently on screen.
/// See [`App::pending_preview`] for why this is one slot rather than one
/// `Option` per kind.
enum PendingPreview {
    /// #25: the calendar add. Explicit rather than an empty slot, so an empty
    /// slot can never be mistaken for it (a stale `Send` slot could
    /// otherwise be released by a calendar "Do it").
    Calendar,
    /// #38 "Review this email": the stashed target to write the edits back
    /// through, consumed by `run_review_executor`.
    Review(actions::review_email::ReviewContext),
    /// #40 "Fill this form": the merged, pre-approval proposal. The value
    /// the card confirmed is only the preview's flat translation, so this is
    /// what `actions::fill_form::rebuild_after_confirm` rebuilds against.
    FormFill(Value),
    /// #242: a non-built-in `actions.toml` action awaiting its preview
    /// decision. Carries the resolved `Action` itself (not just its id) so
    /// `run_confirmed_generic_action` does not need a second
    /// `actions::load_actions()` call between the model's proposal and the
    /// user's confirm click.
    Generic(actions::Action),
    /// #68: a generic action whose executor is `replace_text`: the selection
    /// to write the confirmed text over, or `None` when it cannot be written
    /// back safely, in which case the text is copied instead.
    GenericReplace(
        actions::Action,
        Option<Box<actions::generic_replace::ReplaceSelection>>,
    ),
    /// #127: a linked-page action's confirmation card. Carries the action's
    /// id; "Do it" starts the fetch step with the address on the card.
    FetchLink(String),
    /// #105: a worker is blocked before a request that leaves the machine,
    /// waiting for this preview's answer. Holds the channel back to it:
    /// "Send" answers it (`ui::send_gate::decide`), and every other way the
    /// preview ends drops it, which cancels the send.
    Send(std::sync::mpsc::Sender<confirm::SendAuthorized>),
    /// #117: an action file waiting for "Do it" before it is added to
    /// actions.toml.
    ImportAction(Box<actions::share::Prepared>),
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
    /// `WM_APP_LEARNED` (a key captured for a Change inside Settings (#345)
    /// never gets here: Settings' own message loop takes it off the queue
    /// first. Any other one is stale, and its boxed `Chord` payload is freed
    /// rather than leaked), `WM_APP_PAUSE_TOGGLE` (#181: same
    /// treatment as the hotkey -- a chord press while Settings is open is
    /// dropped, not queued), and the two palette messages (#25: the palette
    /// cannot be shown while Settings is modal-open anyway; `WM_APP_PALETTE_RUN`'s
    /// boxed `String` payload is freed rather than leaked, same as
    /// `WM_APP_LEARNED`'s), and `WM_APP_ROUTER_RESULT` (#24: the options
    /// card a router suggestion belongs to cannot be visible while Settings is
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
    /// `WM_CLOSE` on the owner window (#673): a shutdown, a script or a
    /// smoke test asking Wingman to exit. Falling back to `DefWindowProcW`
    /// would destroy the owner while Settings is modal-open, and then
    /// `WM_DESTROY` is also a fallback, so the process would never leave its
    /// message loop. Instead: remember the request, close Settings (edits are
    /// discarded, as for its own close box), and let `open_settings` re-post
    /// `WM_CLOSE` once the modal loop has returned.
    CloseAfterSettings,
}

/// #722: the policy while the tray menu is open. Settings hides the card
/// before it opens, so it can drop a send preview, a first-run answer and a
/// pending-stage update; the tray menu does not, so those are deferred and
/// delivered when the menu closes (a send preview must not be cancelled just
/// because the tray was opened). `WM_APP_DISMISS` stays dropped: the click
/// that closes the menu is the user's dismissal, and the menu took that
/// click, so there is no card click left to act on.
fn tray_menu_policy(msg: u32, taskbar_created_msg: u32) -> SettingsReentrancy {
    match msg {
        WM_APP_SEND_PREVIEW | WM_APP_FIRST_RUN | WM_APP_PENDING_STAGE => SettingsReentrancy::Defer,
        _ => settings_reentrancy_policy(msg, taskbar_created_msg),
    }
}

fn settings_reentrancy_policy(msg: u32, taskbar_created_msg: u32) -> SettingsReentrancy {
    if taskbar_created_msg != 0 && msg == taskbar_created_msg {
        return SettingsReentrancy::DeferTaskbarCreated;
    }
    match msg {
        WM_CLOSE => SettingsReentrancy::CloseAfterSettings,
        // Issue #213: every WM_APP_* message that carries a worker's result
        // or a preview's Do it/Cancel decision is deferred, never Ignored --
        // see DeferredMessage and the Defer variant's own doc comment. #40:
        // WM_APP_FORM_FILL_RESULT joins the same group for the same reason
        // -- its boxed `Result<Value, String>` payload is taken (not freed)
        // in the Defer arm below.
        // #242: WM_APP_GENERIC_ACTION_RESULT joins the same group -- a
        // finished non-built-in action run must not silently lose its card
        // just because Settings happened to be open, same as
        // WM_APP_CALENDAR_RESULT/WM_APP_FORM_FILL_RESULT.
        WM_APP_RESULT
        | WM_APP_CALENDAR_RESULT
        | WM_APP_REVIEW_RESULT
        | WM_APP_PREVIEW_DECIDED
        | WM_APP_FORM_FILL_RESULT
        | WM_APP_GENERIC_ACTION_RESULT
        | WM_APP_EXECUTOR_DONE => SettingsReentrancy::Defer,
        WM_APP_HOTKEY
        | WM_APP_ACTIVATE
        | WM_APP_TRAY
        | WM_APP_DISMISS
        | WM_APP_LEARNED
        | WM_APP_PAUSE_TOGGLE
        // #133: only a card update; with Settings open there is nothing to
        // update, so the box is freed and the probe's answer dropped.
        | WM_APP_FIRST_RUN
        // #347: like WM_APP_TRAY's own OPEN_SETTINGS command, this just
        // calls open_settings() with no payload to leak, and the card that
        // posts it is hidden the moment it does so, so there is nothing
        // left to defer either.
        | WM_APP_CARD_OPEN_SETTINGS
        // #425: same treatment -- no payload to leak, and an error card
        // cannot be showing at all while Settings is modal-open (every path
        // that opens Settings hides the card first, the same invariant
        // #347's WM_APP_CARD_OPEN_SETTINGS above already relies on), so
        // there is nothing left for a deferred copy to act on either.
        | WM_APP_CARD_COPY_DETAILS
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
        | WM_APP_ROUTER_RESULT
        // #307: the options card is hidden whenever Settings opens (so its
        // key claim is already cleared), so a late choice has nothing to
        // act on. No payload to free.
        | WM_APP_OPTIONS_CHOICE
        // #551: advisory, and the pending card is hidden while Settings is
        // open; the boxed stage is freed below.
        | WM_APP_PENDING_STAGE
        // #328: an on-demand card; Settings hides every card when it
        // opens, so a result arriving then has nothing to show on. The
        // boxed facts are freed below.
        | WM_APP_MACHINE_CHECK_RESULT
        // #105: a send preview cannot be answered while Settings is
        // modal-open, so it is dropped, which cancels the send (the worker
        // sees the reply channel close and the request ends in a card).
        // The boxed `SendRequest` is freed explicitly below.
        | WM_APP_SEND_PREVIEW => SettingsReentrancy::Ignore,
        _ => SettingsReentrancy::Fallback,
    }
}

/// Issue #569: whether `provider` is a keyed cloud provider with no key.
/// Ollama needs none. A stored key that could not be read counts as present;
/// the unreadable-key card already covers it.
fn provider_needs_key(provider: ActiveProvider, providers: &Providers, mode: Mode) -> bool {
    // Local and Offline never use a cloud provider, so a missing key is not
    // why nothing is answering there: show the plain card.
    if matches!(mode, Mode::Local | Mode::Offline) {
        return false;
    }
    let key = match provider {
        ActiveProvider::OpenAi => &providers.openai.api_key,
        ActiveProvider::Anthropic => &providers.anthropic.api_key,
        ActiveProvider::Gemini => &providers.gemini.api_key,
        ActiveProvider::Ollama => return false,
    };
    key.trim().is_empty()
}

/// The card for choosing `provider` from the tray with no key set.
fn missing_key_card(provider: ActiveProvider) -> (String, String) {
    (
        format!("{} needs an API key", provider.label()),
        format!(
            "Click here to open Settings and paste a {} key. Wingman will use it as soon as it is saved.",
            provider.label()
        ),
    )
}

/// Free the boxed payload a `WM_APP_*` message carries, if `msg` is one that
/// carries one. Returns whether it was one. Used where a message is dropped
/// unhandled (Settings is open, or the window is being destroyed) so the
/// worker's `Box::into_raw` is never leaked.
fn free_boxed_payload(msg: u32, lparam: LPARAM) -> bool {
    let p = lparam.0;
    if p == 0 {
        return false;
    }
    unsafe {
        match msg {
            WM_APP_LEARNED => drop(Box::from_raw(p as *mut Chord)),
            WM_APP_PALETTE_RUN => drop(Box::from_raw(p as *mut String)),
            WM_APP_SEND_PREVIEW => drop(Box::from_raw(p as *mut crate::ui::send_gate::SendRequest)),
            WM_APP_PENDING_STAGE => drop(Box::from_raw(p as *mut crate::ui::card::PendingStage)),
            WM_APP_MACHINE_CHECK_RESULT => {
                drop(Box::from_raw(p as *mut crate::machine::MachineFacts))
            }
            WM_APP_FIRST_RUN => drop(Box::from_raw(p as *mut FirstRun)),
            WM_APP_ROUTER_RESULT => drop(Box::from_raw(
                p as *mut (u64, std::result::Result<router::RouterResult, String>),
            )),
            WM_APP_RESULT => drop(Box::from_raw(p as *mut std::result::Result<Answer, String>)),
            WM_APP_CALENDAR_RESULT | WM_APP_FORM_FILL_RESULT => {
                drop(Box::from_raw(p as *mut std::result::Result<Value, String>))
            }
            WM_APP_REVIEW_RESULT => drop(Box::from_raw(
                p as *mut std::result::Result<actions::review_email::ReviewOutcome, String>,
            )),
            WM_APP_GENERIC_ACTION_RESULT => drop(Box::from_raw(
                p as *mut (actions::Action, Value, GenericResult),
            )),
            WM_APP_EXECUTOR_DONE => drop(Box::from_raw(
                p as *mut std::result::Result<ExecDone, String>,
            )),
            _ => return false,
        }
    }
    true
}

/// #673: remember that the owner got `WM_CLOSE` during a nested loop.
fn note_close_request() {
    CLOSE_AFTER_SETTINGS.with(|c| c.set(true));
}

/// #673: whether a close was asked for during a nested loop; clears it.
fn take_close_request() -> bool {
    CLOSE_AFTER_SETTINGS.with(|c| c.replace(false))
}

/// Issue #302: `DestroyWindow` discards messages still queued for the window,
/// so a worker result posted just before Quit would never reach `wnd_proc` and
/// its box would leak. Pull each one out and free it.
fn drain_queued_payloads(hwnd: HWND) {
    let mut m = MSG::default();
    while unsafe { PeekMessageW(&mut m, Some(hwnd), WM_APP, 0xBFFF, PM_REMOVE) }.as_bool() {
        free_boxed_payload(m.message, m.lParam);
    }
}

/// The tray callback (#722). `TrackPopupMenu` runs a nested message loop, so
/// no `&mut App` may be live across it: the menu is built under a short
/// borrow, shown with none held, and the chosen command runs under a fresh
/// one. While the menu is open the Settings reentrancy policy is on, so a
/// result, hotkey or timer that arrives meanwhile is deferred or dropped
/// before it can form a second `&mut App`.
fn handle_tray_message(hwnd: HWND, ptr: *mut App, lparam: LPARAM) -> LRESULT {
    let app: &mut App = unsafe { &mut *ptr };
    // #339: the menu takes the foreground (SetForegroundWindow in
    // `PreparedMenu::track`), so ask who was in front first. Only on the
    // menu gesture, so tray hover traffic costs nothing.
    let menu_hint = Tray::opens_menu(lparam) && app.password_manager_now();
    // #567: show what the last background refresh found, then start
    // the next one. Never waits on Ollama, so the menu opens at once.
    if Tray::opens_menu(lparam) {
        app.sync_ollama_menu();
        app.sync_export_menu();
    }
    let mut was_menu = false;
    let id = match app.tray.on_tray_message(lparam) {
        TrayEvent::None => return LRESULT(0),
        TrayEvent::Command(id) => id,
        TrayEvent::Menu(menu) => {
            was_menu = true;
            SETTINGS_OPEN.with(|c| c.set(true));
            TRAY_MENU_OPEN.with(|c| c.set(true));
            // #307: Esc closes the menu, so the options card must not hold it.
            options_keys::clear();
            let chosen = menu.track();
            TRAY_MENU_OPEN.with(|c| c.set(false));
            SETTINGS_OPEN.with(|c| c.set(false));
            // #307: the menu cleared the options card's key claim. Whatever
            // was chosen, the card (if it is still up) gets it back first, so
            // its hint stays true; a command that hides the card clears it
            // again. A no-op when the card is gone or the claim has lapsed.
            unsafe { &mut *ptr }.card.rearm_options_claim();
            match chosen {
                Some(id) => id,
                None => {
                    let app = unsafe { &mut *ptr };
                    app.finish_modal_deferrals();
                    return LRESULT(0);
                }
            }
        }
    };
    // A fresh borrow: the one above ended before the menu loop started.
    let app: &mut App = unsafe { &mut *ptr };
    if was_menu {
        app.finish_modal_deferrals();
    }
    app.password_manager_hint = menu_hint;
    // Choosing anything other than a rebind disarms a learn mode
    // left armed by an earlier "Set ... key" click; otherwise it
    // would silently swallow the next key pressed anywhere.
    if !matches!(
        decode(id),
        MenuChoice::Command(cmd::SET_PRIMARY)
            | MenuChoice::Command(cmd::SET_SECONDARY)
            | MenuChoice::Command(cmd::SET_PAUSE)
    ) {
        if let Some(h) = &app.hook {
            h.cancel_learning();
        }
    }
    match decode(id) {
        MenuChoice::OpenAiModel(i) => app.pick_model(true, i),
        MenuChoice::AnthropicModel(i) => app.pick_model(false, i),
        MenuChoice::OllamaModel(i) => app.pick_ollama_model(i),
        MenuChoice::ExportAction(i) => app.export_action(i),
        MenuChoice::Command(cmd::IMPORT_ACTION) => app.import_action(),
        MenuChoice::Command(cmd::ASK_NOW) => app.ask(Trigger::Tray),
        MenuChoice::Command(cmd::EXTRACT_TEXT) => app.extract_text(),
        MenuChoice::Command(cmd::READ_QR) => app.read_qr(),
        MenuChoice::Command(cmd::COPY_REGION) => app.copy_region(),
        MenuChoice::Command(cmd::COPY_LAST) => app.copy_last(),
        MenuChoice::Command(cmd::ADD_TO_CALENDAR) => app.add_event_from_screen(),
        MenuChoice::Command(cmd::QUICK_ASK) => app.toggle_palette(),
        MenuChoice::Command(cmd::WHAT_CAN_I_DO) => app.show_options_card(Trigger::Tray, true),
        MenuChoice::Command(cmd::REVIEW_EMAIL) => app.review_this_email(),
        MenuChoice::Command(cmd::FILL_FORM) => app.fill_form_from_screen(),
        MenuChoice::Command(cmd::RESTORE_LAST_FORM) => app.restore_last_form(),
        MenuChoice::Command(cmd::SET_PRIMARY) => app.start_learning(HK_PRIMARY),
        MenuChoice::Command(cmd::SET_SECONDARY) => app.start_learning(HK_SECONDARY),
        MenuChoice::Command(cmd::SET_PAUSE) => app.start_learning(HK_PAUSE),
        MenuChoice::Command(cmd::EDIT_SETTINGS) => app.edit_settings(),
        MenuChoice::Command(cmd::RELOAD) => app.reload(),
        MenuChoice::Command(cmd::COPY_DIAGNOSTICS) => app.copy_diagnostics(),
        MenuChoice::Command(cmd::COPY_EGRESS_LOG) => app.copy_egress_log(),
        MenuChoice::Command(cmd::CHECK_MACHINE) => app.check_machine(),
        MenuChoice::Command(cmd::CALCULATE_SELECTION) => app.calculate_selection(),
        MenuChoice::Command(cmd::USE_OPENAI) => app.set_provider(ActiveProvider::OpenAi),
        MenuChoice::Command(cmd::USE_ANTHROPIC) => app.set_provider(ActiveProvider::Anthropic),
        MenuChoice::Command(cmd::USE_OLLAMA) => app.set_provider(ActiveProvider::Ollama),
        MenuChoice::Command(cmd::USE_GEMINI) => app.set_provider(ActiveProvider::Gemini),
        MenuChoice::Command(cmd::PAUSE_1H) => app.pause_for(PauseChoice::OneHour),
        MenuChoice::Command(cmd::PAUSE_UNTIL_TOMORROW) => app.pause_for(PauseChoice::UntilTomorrow),
        MenuChoice::Command(cmd::PAUSE_UNTIL_RESUMED) => app.pause_for(PauseChoice::UntilResumed),
        MenuChoice::Command(cmd::RESUME) => app.resume(),
        MenuChoice::Command(cmd::MODE_CLOUD) => app.set_mode(Mode::Cloud),
        MenuChoice::Command(cmd::MODE_LOCAL) => app.set_mode(Mode::Local),
        MenuChoice::Command(cmd::MODE_AUTO) => app.set_mode(Mode::Auto),
        MenuChoice::Command(cmd::MODE_OFFLINE) => app.set_mode(Mode::Offline),
        MenuChoice::Command(cmd::OPEN_SETTINGS) => app.open_settings(),
        MenuChoice::Command(cmd::QUIT) => {
            if let Some(text) = app.quit_blocked() {
                app.card.show_answer("Still working", text, 5, None);
                app.set_watch(true);
            } else {
                unsafe {
                    let _ = DestroyWindow(hwnd);
                }
            }
        }
        _ => {}
    }
    // Not after Quit: the window (and `app`) may already be gone.
    if !matches!(decode(id), MenuChoice::Command(cmd::QUIT)) {
        app.password_manager_hint = false;
    }
    LRESULT(0)
}

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        OWNER_HWND.with(|h| h.set(hwnd.0 as isize));
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }

    // #723: a result from a run the app gave up on is dropped here, before
    // anything else, so it can neither clear the new run's busy flag nor
    // show an old answer.
    if is_run_result(msg) && !result_is_current(wparam.0, CURRENT_RUN.with(|c| c.get())) {
        free_boxed_payload(msg, lparam);
        return LRESULT(0);
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
        if msg == WM_TIMER && wparam.0 == STUCK_TIMER_ID {
            unsafe {
                let _ = KillTimer(Some(hwnd), STUCK_TIMER_ID);
            }
            BUSY_STUCK.with(|c| c.set(true));
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
        let policy = if TRAY_MENU_OPEN.with(|c| c.get()) {
            tray_menu_policy(msg, taskbar_created_msg)
        } else {
            settings_reentrancy_policy(msg, taskbar_created_msg)
        };
        match policy {
            SettingsReentrancy::Ignore => {
                // WM_APP_LEARNED, WM_APP_PALETTE_RUN and WM_APP_ROUTER_RESULT
                // are the only ignored messages carrying a boxed payload;
                // free them so none leaks. (WM_APP_CALENDAR_RESULT,
                // WM_APP_REVIEW_RESULT and WM_APP_FORM_FILL_RESULT are all
                // Defer, not Ignore -- issue #213/#40.)
                free_boxed_payload(msg, lparam);
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
                    WM_APP_PREVIEW_DECIDED => DeferredMessage::PreviewDecided(wparam.0 as u32),
                    // #722: only `tray_menu_policy` defers these three.
                    WM_APP_SEND_PREVIEW => DeferredMessage::SendPreview(unsafe {
                        *Box::from_raw(lparam.0 as *mut crate::ui::send_gate::SendRequest)
                    }),
                    WM_APP_PENDING_STAGE => {
                        DeferredMessage::PendingStage(wparam.0 as u64, unsafe {
                            *Box::from_raw(lparam.0 as *mut crate::ui::card::PendingStage)
                        })
                    }
                    WM_APP_FIRST_RUN => DeferredMessage::FirstRun(wparam.0 as u64, unsafe {
                        *Box::from_raw(lparam.0 as *mut FirstRun)
                    }),
                    // #242: same treatment as WM_APP_CALENDAR_RESULT above.
                    WM_APP_GENERIC_ACTION_RESULT => {
                        let (action, schema, result) = *unsafe {
                            Box::from_raw(lparam.0 as *mut (actions::Action, Value, GenericResult))
                        };
                        DeferredMessage::GenericActionResult(action, schema, result)
                    }
                    WM_APP_EXECUTOR_DONE => DeferredMessage::ExecutorDone(unsafe {
                        *Box::from_raw(lparam.0 as *mut std::result::Result<ExecDone, String>)
                    }),
                    _ => {
                        unreachable!("the reentrancy policies only return Defer for the ids above")
                    }
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
            SettingsReentrancy::CloseAfterSettings => {
                // Settings, a file dialog or the tray menu is open. Only
                // Settings can be closed from here; the others end on their
                // own and then repost this (`repost_close_if_requested`).
                note_close_request();
                settings::request_close();
                return LRESULT(0);
            }
        }
    }

    let ptr = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut App;
    if ptr.is_null() {
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
    }
    // #722: handed the raw pointer, before any `&mut App` is bound here, so
    // that no borrow can be live across the tray menu's message loop.
    if msg == WM_APP_TRAY {
        return handle_tray_message(hwnd, ptr, lparam);
    }
    let app: &mut App = unsafe { &mut *ptr };

    match msg {
        WM_APP_HOTKEY => {
            let _which = wparam.0; // both bindings do the same thing
            if !app.cancel_pending_send_preview() {
                app.on_primary_key(Trigger::Hotkey);
            }
            LRESULT(0)
        }
        WM_APP_ACTIVATE => {
            // `ask` already no-ops while `busy`, so a second Copilot-key press
            // landing here while a request is in flight is harmless, except
            // that it cancels a waiting send preview (#105). Any process can
            // post this message (#236), so unlike the hotkey it is rate
            // limited, and a refusal is counted for the diagnostics report.
            if !app.cancel_pending_send_preview() {
                // The limiter is only consulted when an ask would really run:
                // busy and paused make `ask` a no-op (or a Paused card), and
                // must not cost the user an allowance.
                let would_run = matches!(
                    model_action_gate(app.busy_now(), pause::is_paused_now()),
                    ModelActionGate::Proceed
                );
                // #308: a duplicate launch shows the static card, the
                // model-free options only. No router call, so it cannot cost
                // money; the limiter stays as it was.
                if !would_run {
                    app.show_options_card(Trigger::Activation, false);
                } else {
                    match crate::activation::check_activation_now() {
                        crate::activation::Verdict::Allowed => {
                            app.show_options_card(Trigger::Activation, false)
                        }
                        // Rule 7: the Start menu and the Copilot-key picker
                        // arrive here too, so a refusal says so once.
                        crate::activation::Verdict::Refused { notify: true } => {
                            app.card.show_answer(
                                "Too many requests in a row",
                                "Try again in a moment.",
                                3,
                                None,
                            )
                        }
                        crate::activation::Verdict::Refused { notify: false } => {}
                    }
                }
            }
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
        WM_APP_GENERIC_ACTION_RESULT => {
            let (action, schema, result) =
                unsafe { *Box::from_raw(lparam.0 as *mut (actions::Action, Value, GenericResult)) };
            app.on_generic_action_result(action, schema, result);
            LRESULT(0)
        }
        WM_APP_EXECUTOR_DONE => {
            let result =
                unsafe { *Box::from_raw(lparam.0 as *mut std::result::Result<ExecDone, String>) };
            app.on_exec_done(result);
            LRESULT(0)
        }
        WM_APP_MACHINE_CHECK_RESULT => {
            let facts = unsafe { *Box::from_raw(lparam.0 as *mut crate::machine::MachineFacts) };
            app.on_machine_check_result(&facts);
            LRESULT(0)
        }
        WM_APP_PENDING_STAGE => {
            let stage = unsafe { *Box::from_raw(lparam.0 as *mut crate::ui::card::PendingStage) };
            app.on_pending_stage(wparam.0 as u64, stage);
            LRESULT(0)
        }
        WM_APP_PREVIEW_DECIDED => {
            app.on_preview_decided(wparam.0 as u32);
            LRESULT(0)
        }
        WM_APP_SEND_PREVIEW => {
            // #105: the worker is blocked until this is answered or dropped.
            let request =
                unsafe { *Box::from_raw(lparam.0 as *mut crate::ui::send_gate::SendRequest) };
            app.on_send_preview(request);
            LRESULT(0)
        }
        WM_APP_CARD_OPEN_SETTINGS => {
            // Issue #347: a click on a "settings needed" card. No payload.
            app.open_settings();
            LRESULT(0)
        }
        WM_APP_CARD_COPY_DETAILS => {
            // Issue #425: a click/Enter on an expanded error card's "Copy
            // details" affordance. No payload.
            app.copy_error_details();
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
        WM_APP_FIRST_RUN => {
            // #133: take ownership of the probe's answer.
            let found = unsafe { *Box::from_raw(lparam.0 as *mut FirstRun) };
            app.on_first_run_result(wparam.0 as u64, found);
            LRESULT(0)
        }
        WM_APP_OPTIONS_CHOICE => {
            // #307: a row clicked, a claimed digit, or Esc on the options
            // card. No payload; `lparam` is the card's generation.
            app.on_options_choice(wparam.0, lparam.0);
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
            } else if wparam.0 == STUCK_TIMER_ID {
                app.on_busy_stuck();
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
        // #673: an owner-window close (a script, a smoke test) takes the same
        // refusal as tray > Quit while work is in flight. Session end is
        // WM_ENDSESSION, which never comes through here and is not blocked.
        WM_CLOSE => {
            if let Some(text) = app.quit_blocked() {
                app.card.show_answer("Still working", text, 5, None);
                app.set_watch(true);
                LRESULT(0)
            } else {
                unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
            }
        }
        WM_DESTROY => {
            // Drop the App (and with it the hook, tray icon and card) before
            // the loop exits, so the tray icon actually disappears.
            unsafe { SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0) };
            drop(unsafe { Box::from_raw(ptr) });
            drain_queued_payloads(hwnd);
            unsafe { PostQuitMessage(0) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

#[cfg(test)]
mod tests {
    // -- #105 send preview slot ------------------------------------------

    #[test]
    fn the_key_cancels_a_pending_send_and_wakes_the_worker() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut slot = Some(super::PendingPreview::Send(tx));
        assert!(super::take_send_slot(&mut slot));
        assert!(slot.is_none());
        assert!(rx.recv().is_err(), "the worker must be woken with nothing");
    }

    #[test]
    fn the_key_leaves_any_other_pending_preview_alone() {
        let mut slot = Some(super::PendingPreview::FormFill(serde_json::Value::Null));
        assert!(!super::take_send_slot(&mut slot));
        assert!(matches!(slot, Some(super::PendingPreview::FormFill(_))));
        let mut empty = None;
        assert!(!super::take_send_slot(&mut empty));
    }

    #[test]
    fn a_calendar_preview_replaces_a_stale_send_slot() {
        let (tx, rx) = std::sync::mpsc::channel();
        let mut slot = Some(super::PendingPreview::Send(tx));
        super::arm_calendar_slot(&mut slot);
        assert!(matches!(slot, Some(super::PendingPreview::Calendar)));
        assert!(rx.recv().is_err(), "the replaced send must not authorize");
    }

    #[test]
    fn a_cancelled_send_is_recognised_through_the_packed_error() {
        let chain = format!("{:#}", anyhow::Error::new(crate::provider::SendCancelled));
        let packed = super::pack_error(&super::human_error_detail(&chain), &chain);
        assert!(super::is_send_cancelled(&packed));
        let other = super::pack_error("x", "anthropic: HTTP 500");
        assert!(!super::is_send_cancelled(&other));
    }

    use super::append_notices;
    use super::first_line;
    use super::resolve_prompt_and_difficulty;
    use super::unreadable_secrets_card;
    use super::App;
    use super::BeginPlan;
    use super::WM_APP_FIRST_RUN;
    use super::{
        exec_work_calendar, exec_work_form_fill, exec_work_restore, ExecDone, WM_APP_EXECUTOR_DONE,
    };
    use super::{final_settings_card, SettingsFinalCard};
    use super::{first_provider_model_label, provider_for_router, router_models_for};
    use super::{mode_allowed, settings_reentrancy_policy, SettingsReentrancy};
    use super::{
        WM_APP_ACTIVATE, WM_APP_CALENDAR_RESULT, WM_APP_FORM_FILL_RESULT,
        WM_APP_GENERIC_ACTION_RESULT, WM_APP_MACHINE_CHECK_RESULT, WM_APP_PENDING_STAGE,
        WM_APP_RESULT, WM_APP_REVIEW_RESULT, WM_APP_ROUTER_RESULT, WM_APP_SEND_PREVIEW,
    };
    use crate::actions::{self, Origin};
    use crate::capture;
    use crate::config::Providers;
    use crate::dismiss::WM_APP_DISMISS;
    use crate::executors;
    use crate::hotkey::{
        WM_APP_HOTKEY, WM_APP_LEARNED, WM_APP_PALETTE_TOGGLE, WM_APP_PAUSE_TOGGLE,
    };
    use crate::mode::Mode;
    use crate::options_keys::WM_APP_OPTIONS_CHOICE;
    use crate::provider::ollama_admin::FirstRun;
    use crate::provider::Provider;
    use crate::router;
    use crate::ui::card::{
        WM_APP_CARD_COPY_DETAILS, WM_APP_CARD_OPEN_SETTINGS, WM_APP_PREVIEW_DECIDED,
    };
    use crate::ui::confirm;
    use crate::ui::palette::WM_APP_PALETTE_RUN;
    use crate::ui::palette_model;
    use crate::ui::tray::WM_APP_TRAY;
    use serde_json::Value;
    use windows::Win32::UI::WindowsAndMessaging::{WM_CLOSE, WM_DESTROY};

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
        let (prompt, _) = resolve_prompt_and_difficulty(&r, "config.toml prompt");
        assert_eq!(prompt, "config.toml prompt");
    }

    #[test]
    fn user_origin_overrides_the_config_prompt() {
        // #23's Done-when: a user action overriding a built-in's prompt is
        // what runs.
        let r = resolved(Origin::User, "overridden prompt", false);
        let (prompt, _) = resolve_prompt_and_difficulty(&r, "config.toml prompt");
        assert_eq!(prompt, "overridden prompt");
    }

    #[test]
    fn difficulty_follows_only_the_actions_own_flag() {
        for action_flag in [false, true] {
            let r = resolved(Origin::Builtin, "p", action_flag);
            let (_, want_difficulty) = resolve_prompt_and_difficulty(&r, "p");
            assert_eq!(want_difficulty, action_flag);
        }
    }

    #[test]
    fn fresh_install_defaults_send_no_difficulty_rubric() {
        // `rate_difficulty` defaults to false, so a fresh install sends no
        // rubric (#197).
        let r = resolved(Origin::Builtin, crate::provider::DEFAULT_PROMPT, false);
        let (prompt, want_difficulty) =
            resolve_prompt_and_difficulty(&r, crate::provider::DEFAULT_PROMPT);
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
        // AGENTS.md rule 11: no em dashes in user-facing strings.
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

    /// #723: a press while busy must leave a card on screen, but must not
    /// cover a card that already says so or one waiting on the user.
    #[test]
    fn busy_press_card_shows_only_when_nothing_explains_the_wait() {
        use crate::ui::card::CardState;
        for (state, expected) in [
            (CardState::Pending, super::BusyPress::Leave),
            (CardState::Preview, super::BusyPress::Leave),
            (CardState::Hidden, super::BusyPress::ShowStillWorking),
            (CardState::Collapsed, super::BusyPress::Leave),
            (CardState::Expanded, super::BusyPress::Leave),
            // #307: a card the user is choosing from is never covered.
            (CardState::Options, super::BusyPress::Leave),
        ] {
            assert_eq!(super::busy_press_card(state), expected, "{state:?}");
        }
    }

    // -- #307: the options card's choices ---------------------------------

    fn session(generation: u32, ids: &[&str]) -> super::OptionsSession {
        super::OptionsSession {
            generation,
            ids: ids.iter().map(|s| s.to_string()).collect(),
            catalogue: Vec::new(),
            key_pickable: Vec::new(),
        }
    }

    fn choose(
        wparam: usize,
        lparam: isize,
        card: Option<u32>,
        s: Option<&super::OptionsSession>,
    ) -> super::OptionsChoiceAction {
        super::options_choice_action(wparam, lparam, card, s)
    }

    #[test]
    fn a_pick_runs_the_action_at_that_position() {
        use super::OptionsChoiceAction::Run;
        let s = session(7, &["check-my-work", "explain-screen", "translate"]);
        assert_eq!(choose(1, 7, Some(7), Some(&s)), Run("check-my-work".into()));
        assert_eq!(
            choose(2, 7, Some(7), Some(&s)),
            Run("explain-screen".into())
        );
        assert_eq!(choose(3, 7, Some(7), Some(&s)), Run("translate".into()));
    }

    #[test]
    fn esc_closes_the_card_and_runs_nothing() {
        let s = session(7, &["a", "b"]);
        assert_eq!(
            choose(0, 7, Some(7), Some(&s)),
            super::OptionsChoiceAction::Close
        );
    }

    #[test]
    fn a_choice_made_on_another_card_runs_nothing() {
        use super::OptionsChoiceAction::Ignore;
        let s = session(7, &["a", "b"]);
        // The card on screen is a newer one, or none, or the session is gone.
        assert_eq!(choose(1, 7, Some(8), Some(&s)), Ignore);
        assert_eq!(choose(1, 7, None, Some(&s)), Ignore);
        assert_eq!(choose(1, 7, Some(7), None), Ignore);
        // The click was made on an older card than the one showing.
        assert_eq!(choose(1, 6, Some(7), Some(&s)), Ignore);
        assert_eq!(choose(0, 6, Some(7), Some(&s)), Ignore, "a stale Esc too");
    }

    #[test]
    fn a_pick_with_no_button_there_runs_nothing() {
        use super::OptionsChoiceAction::Ignore;
        // Still looking: no ids yet, so a digit cannot name an action.
        let looking = session(3, &[]);
        assert_eq!(choose(1, 3, Some(3), Some(&looking)), Ignore);
        // Past the last of two buttons.
        let two = session(3, &["a", "b"]);
        assert_eq!(choose(3, 3, Some(3), Some(&two)), Ignore);
        assert_eq!(choose(4, 3, Some(3), Some(&two)), Ignore);
    }

    #[test]
    fn a_malformed_choice_runs_nothing() {
        let s = session(3, &["a", "b", "c", "d"]);
        for w in [5usize, 6, 99, usize::MAX] {
            assert_eq!(
                choose(w, 3, Some(3), Some(&s)),
                super::OptionsChoiceAction::Ignore,
                "wparam {w}"
            );
        }
    }

    #[test]
    fn the_generation_is_compared_within_24_bits() {
        // A card generation is 24 bits; the message carries it in a wider
        // word, so stray high bits must not make a live card look stale, and
        // must not make a stale one look live.
        let s = session(0x00AB_CDEF, &["a"]);
        let live = 0x00AB_CDEFisize;
        assert!(matches!(
            choose(1, live, Some(0x00AB_CDEF), Some(&s)),
            super::OptionsChoiceAction::Run(_)
        ));
        assert_eq!(
            choose(1, live + 1, Some(0x00AB_CDEF), Some(&s)),
            super::OptionsChoiceAction::Ignore
        );
    }

    #[test]
    fn option_rows_carry_the_id_and_label_and_cap_at_four() {
        let options: Vec<router::RouterOption> = (0..6)
            .map(|n| router::RouterOption {
                intent: format!("id{n}"),
                label: format!("Label {n}"),
                confidence: 0.5,
            })
            .collect();
        let rows = super::option_rows(&options);
        assert_eq!(rows.len(), 4);
        assert_eq!(
            (rows[0].id.as_str(), rows[0].label.as_str()),
            ("id0", "Label 0")
        );
        assert_eq!(rows[3].id, "id3");
        assert!(super::option_rows(&[]).is_empty());
    }

    /// The body of the `impl App` method `name`, from its signature to its
    /// closing brace (`    }` at the method indent). CRLF-normalised.
    fn app_method_body(name: &str) -> String {
        let src = lf(include_str!("app.rs"));
        let sig = format!("\n    fn {name}(");
        let start = src
            .find(&sig)
            .unwrap_or_else(|| panic!("app.rs has no method {name}"));
        let rest = &src[start + 1..];
        let end = rest.find("\n    }\n").expect("method has no end");
        let body = rest[..end].to_string();
        assert!(!body.is_empty(), "scanned an empty body for {name}");
        body
    }

    /// The one dispatch (spec "Implementation shape"): picking an option runs
    /// the action through `dispatch_palette_action`, with no second `match` on
    /// action ids that could drift from the palette's.
    #[test]
    fn an_option_runs_through_the_one_palette_dispatch() {
        let body = app_method_body("on_options_choice");
        assert!(body.contains("self.dispatch_palette_action("), "{body}");
        for second in [
            "dispatch_palette_target",
            "dispatch_target_for",
            "DispatchTarget",
            "self.ask(",
            "self.extract_text(",
        ] {
            assert!(!body.contains(second), "a second dispatch: {second}");
        }
        // And the card is gone, and settled, before the action captures.
        let close = body
            .find("self.close_options_card()")
            .expect("never closes");
        let settle = body.find("CARD_SETTLE_MS").expect("never settles");
        let run = body.find("self.dispatch_palette_action(").unwrap();
        assert!(close < settle && settle < run);
    }

    /// #805: opening the palette sends nothing. It reads no screen and starts
    /// no model request: the action the user picks or types does that later.
    #[test]
    fn opening_the_palette_captures_and_sends_nothing() {
        let body = app_method_body("toggle_palette");
        for forbidden in [
            "grab_raw",
            "capture::",
            "router::",
            "router_worker",
            "start_options_router",
            "show_options_card",
            "thread::spawn",
            "WM_APP_ROUTER_RESULT",
        ] {
            assert!(
                !body.contains(forbidden),
                "toggle_palette must not use {forbidden}"
            );
        }
        assert!(body.contains("self.palette.show("));
    }

    /// The options card is reachable and answered: the tray item runs it, the
    /// window procedure routes its choices and the router's reply to it.
    #[test]
    fn the_options_card_is_wired_to_the_tray_and_the_window_procedure() {
        let src = lf(include_str!("app.rs"));
        let proc_src = wnd_proc_body(&src);
        let arm = proc_src
            .split("WM_APP_OPTIONS_CHOICE => {")
            .nth(1)
            .expect("wnd_proc has no WM_APP_OPTIONS_CHOICE arm");
        let arm = &arm[..arm.find("LRESULT(0)").expect("arm has no end")];
        assert!(arm.contains("app.on_options_choice("), "{arm}");
        let reply = proc_src
            .split("WM_APP_ROUTER_RESULT => {")
            .nth(1)
            .expect("wnd_proc has no WM_APP_ROUTER_RESULT arm");
        let reply = &reply[..reply.find("LRESULT(0)").expect("arm has no end")];
        assert!(reply.contains("app.on_router_result("), "{reply}");
        assert!(
            src.contains("MenuChoice::Command(cmd::WHAT_CAN_I_DO) => app.show_options_card("),
            "the tray item must open the options card"
        );
    }

    /// An ignored choice (stale generation, or no buttons yet) leaves the card
    /// up with its claim spent, so it must be put back; a choice that closes
    /// the card or runs an action must not be.
    #[test]
    fn only_an_ignored_choice_puts_the_claim_back() {
        use super::{options_choice_action, rearm_after, OptionsChoiceAction, OptionsSession};
        let session = OptionsSession {
            generation: 5,
            ids: vec!["a".to_string(), "b".to_string()],
            catalogue: Vec::new(),
            key_pickable: Vec::new(),
        };
        // A digit made on an older card: ignored, card still up.
        let stale = options_choice_action(1, 4, Some(5), Some(&session));
        assert_eq!(stale, OptionsChoiceAction::Ignore);
        assert!(rearm_after(&stale));
        // A digit naming no button yet: ignored.
        let none_yet = options_choice_action(3, 5, Some(5), Some(&session));
        assert_eq!(none_yet, OptionsChoiceAction::Ignore);
        assert!(rearm_after(&none_yet));
        // A real pick and Esc end the card.
        let pick = options_choice_action(2, 5, Some(5), Some(&session));
        assert_eq!(pick, OptionsChoiceAction::Run("b".to_string()));
        assert!(!rearm_after(&pick));
        let esc = options_choice_action(0, 5, Some(5), Some(&session));
        assert_eq!(esc, OptionsChoiceAction::Close);
        assert!(!rearm_after(&esc));
    }

    // -- #308: the bare key opens the options card ------------------------

    fn catalogue_with_builtins() -> Vec<crate::ui::palette_model::PaletteAction> {
        let resolved = crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]);
        crate::ui::palette_model::catalogue(&resolved)
    }

    fn routed(intent: &str, confidence: f64) -> crate::router::RouterOption {
        crate::router::RouterOption {
            intent: intent.to_string(),
            label: "x".to_string(),
            confidence,
        }
    }

    /// #859: a router reply for a screen carrying injected text can put a
    /// clipboard option first at confidence 1.0; the key must still not run it.
    #[test]
    fn a_clipboard_option_at_full_confidence_is_still_not_key_pickable() {
        use crate::ui::palette_model::Effect;
        let cat = catalogue_with_builtins();
        let clip: Vec<_> = cat
            .iter()
            .filter(|a| a.effect == Effect::Clipboard)
            .collect();
        assert!(clip.len() >= 3);
        for a in clip {
            let picks = super::option_key_pickable(&[routed(&a.id, 1.0)], &cat);
            assert_eq!(picks, vec![false], "{}", a.id);
        }
        // A read-only action is; an id the catalogue does not know is not.
        let picks = super::option_key_pickable(
            &[
                routed(crate::actions::DEFAULT_ACTION_ID, 0.1),
                routed("not-an-action", 1.0),
            ],
            &cat,
        );
        assert_eq!(picks, vec![true, false]);
        assert!(!super::keypress_runs_first(&[]));
    }

    /// The single-press table, read off the app's own state.
    #[test]
    fn the_key_reads_the_options_card_state_the_app_holds() {
        use crate::keypress::Phase;
        let mut s = session(4, &[]);
        // Looking: a shell with no buttons.
        assert_eq!(super::key_phase_of(Some(&s), Some(4)), Phase::Looking);
        s = session(4, &["a", "b"]);
        s.key_pickable = vec![true, false];
        assert_eq!(
            super::key_phase_of(Some(&s), Some(4)),
            Phase::Options {
                first_key_pickable: true
            }
        );
        s.key_pickable = vec![false, true];
        assert_eq!(
            super::key_phase_of(Some(&s), Some(4)),
            Phase::Options {
                first_key_pickable: false
            }
        );
        // A card that is not this session's, or no card: not an options card.
        assert_eq!(super::key_phase_of(Some(&s), Some(5)), Phase::Other);
        assert_eq!(super::key_phase_of(Some(&s), None), Phase::Other);
        assert_eq!(super::key_phase_of(None, Some(4)), Phase::Other);
    }

    /// Wired-to-nothing guard: the hook's message reaches the key table, and a
    /// duplicate launch shows the static card and never asks a model.
    #[test]
    fn the_hotkey_runs_the_key_table_and_activation_shows_the_static_card() {
        let src = lf(include_str!("app.rs"));
        let proc_src = wnd_proc_body(&src);
        let hotkey = proc_src
            .split("WM_APP_HOTKEY => {")
            .nth(1)
            .expect("no WM_APP_HOTKEY arm");
        let hotkey = &hotkey[..hotkey.find("LRESULT(0)").unwrap()];
        assert!(
            hotkey.contains("app.on_primary_key(Trigger::Hotkey)"),
            "{hotkey}"
        );
        assert!(!hotkey.contains(".ask("), "the key no longer asks directly");
        let activate = proc_src
            .split("WM_APP_ACTIVATE => {")
            .nth(1)
            .expect("no WM_APP_ACTIVATE arm");
        let activate = &activate[..activate.find("LRESULT(0)").unwrap()];
        assert!(
            activate.contains("show_options_card(Trigger::Activation, false)"),
            "{activate}"
        );
        assert!(!activate.contains(".ask("), "an activation must never ask");
        let key = src
            .split("fn on_primary_key(")
            .nth(1)
            .expect("no on_primary_key");
        let key = &key[..key.find("\n    }\n").unwrap()];
        assert!(key.contains("keypress::on_key("), "{key}");
        assert!(key.contains("primary_target()"), "{key}");
    }

    /// The tray menu clears the claim; it is put back right after the menu
    /// ends, before the chosen command is dispatched, and not only when
    /// nothing was chosen. (The window procedure needs a live window, so this
    /// one pins the order in the source; the card's own behaviour is tested in
    /// `ui::card::options`.)
    #[test]
    fn the_tray_menu_puts_the_options_claim_back_before_dispatching() {
        let src = lf(include_str!("app.rs"));
        let start = src
            .find("\nfn handle_tray_message(")
            .expect("no handle_tray_message");
        let body = &src[start + 1..];
        let body = &body[..body.find("\n}\n").expect("fn has no end")];
        let track = body.find(".track()").expect("the menu is never tracked");
        let rearm = body
            .find("rearm_options_claim()")
            .expect("the claim is never put back");
        let dispatch = body.find("match chosen {").expect("no dispatch on chosen");
        assert!(track < rearm && rearm < dispatch, "wrong order");
        assert_eq!(body.matches("rearm_options_claim()").count(), 1);
    }

    /// The tray menu loop runs a nested message loop, and Esc closes a menu:
    /// the options card must not hold the key while it runs.
    #[test]
    fn the_tray_menu_releases_the_options_key_claim_before_it_runs() {
        let src = lf(include_str!("app.rs"));
        let start = src
            .find("\nfn handle_tray_message(")
            .expect("app.rs has no fn handle_tray_message");
        let body = &src[start + 1..];
        let body = &body[..body.find("\n}\n").expect("fn has no end")];
        let clear = body
            .find("options_keys::clear()")
            .expect("the menu never releases the claim");
        let track = body.find(".track()").expect("the menu is never tracked");
        assert!(clear < track);
    }

    /// #723: a run flagged stuck is abandoned by the next press; a healthy
    /// busy run still blocks it.
    #[test]
    fn a_stuck_run_is_abandoned_by_the_next_press() {
        assert!(!super::busy_after_press(false, false));
        assert!(!super::busy_after_press(false, true));
        assert!(super::busy_after_press(true, false));
        assert!(!super::busy_after_press(true, true));
    }

    /// #723: a late result from an abandoned run is dropped; one posted
    /// without a generation (0) is always kept.
    #[test]
    fn stale_results_are_dropped_by_generation() {
        assert!(super::result_is_current(5, 5));
        assert!(!super::result_is_current(4, 5));
        assert!(super::result_is_current(0, 5));
        for msg in [WM_APP_RESULT, WM_APP_CALENDAR_RESULT, WM_APP_REVIEW_RESULT] {
            assert!(super::is_run_result(msg));
        }
        // A send preview from an abandoned run is stale too.
        assert!(super::is_run_result(WM_APP_SEND_PREVIEW));
        assert!(!super::is_run_result(WM_APP_HOTKEY));
        // An executor's done message is a real write: never dropped as stale.
        assert!(!super::is_run_result(WM_APP_EXECUTOR_DONE));
        let src = lf(include_str!("app.rs"));
        let tagged = src.matches("send_gate::set_worker_run(run);").count();
        assert!(tagged >= 7, "workers that tag their run: {tagged}");
    }

    /// #723: every worker result is posted with its run generation, or the
    /// stale check would never see it. Scans the post sites in app.rs.
    #[test]
    fn worker_results_carry_their_run_generation() {
        let src = lf(include_str!("app.rs"));
        let mut seen = 0;
        for id in [
            "WM_APP_RESULT,",
            "WM_APP_CALENDAR_RESULT,",
            "WM_APP_REVIEW_RESULT,",
            "WM_APP_FORM_FILL_RESULT,",
            "WM_APP_GENERIC_ACTION_RESULT,",
        ] {
            for (i, _) in src.match_indices(&format!("                    {id}\n")) {
                let next = src[i..].lines().nth(1).unwrap_or("");
                assert!(next.contains("WPARAM(run as usize)"), "{id} {next}");
                seen += 1;
            }
        }
        assert!(seen >= 7, "expected the result post sites, found {seen}");
    }

    /// #805: opening the palette sends nothing. The router's request (a
    /// screenshot to a model) used to start at the end of `toggle_palette`;
    /// it now belongs to the key press and the options card. Scans the body
    /// of `toggle_palette` for anything that captures the screen or starts a
    /// model request.
    #[test]
    fn opening_the_palette_sends_no_model_request() {
        let src = lf(include_str!("app.rs"));
        let start = src
            .find("    fn toggle_palette(&mut self) {")
            .expect("toggle_palette exists");
        let body = &src[start..];
        let end = body.find("\n    }\n").expect("toggle_palette ends");
        let body = &body[..end];
        assert!(
            body.contains("self.palette.show("),
            "the scan found the wrong body"
        );
        for forbidden in [
            "show_options_card",
            "router_worker",
            "capture::",
            "grab_raw",
            "thread::spawn",
            "set_router_pending",
        ] {
            assert!(
                !body.contains(forbidden),
                "toggle_palette starts a request again: {forbidden}"
            );
        }
    }

    /// #723: the stuck bound covers every provider in a chain at its own
    /// wall clock, so a healthy run is never called stuck.
    #[test]
    fn stuck_bound_covers_the_slowest_honest_chain() {
        for n in [1usize, 4, 9] {
            let bound = std::hint::black_box(crate::provider::worst_case_run_secs(n));
            assert!(bound >= n as u64 * crate::provider::common::MAX_TOTAL_WALL_CLOCK_SECS);
            assert!(bound >= n as u64 * crate::provider::ollama::TOTAL_TIMEOUT_SECS);
        }
        // A chain with no providers still gets a positive bound.
        assert!(crate::provider::worst_case_run_secs(0) > 0);
    }

    /// #723: no busy gate may return without a card. Scans every
    /// early return on the busy flag in app.rs.
    #[test]
    fn busy_gates_never_return_silently() {
        let src = lf(include_str!("app.rs"));
        let mut seen = 0;
        for (i, _) in src.match_indices(&("if self.".to_owned() + "busy_now() {")) {
            let after = src.get(i..i + 120).unwrap_or(&src[i..]);
            assert!(
                after.contains("self.show_busy_card()"),
                "a busy gate returns without a card: {after}"
            );
            seen += 1;
        }
        assert!(seen >= 3, "expected the busy gates, found {seen}");
        // The raw flag must not gate a press either: it skips the stuck-run
        // abandon and returns with no card.
        let raw = "if self.".to_owned() + "busy {";
        for (i, _) in src.match_indices(&raw) {
            let after = src.get(i..i + 120).unwrap_or(&src[i..]);
            assert!(
                after.contains("self.card.show_answer(") || after.contains("self.show_busy_card()"),
                "a raw busy check returns without a card: {after}"
            );
        }
        let gate = src
            .find("ModelActionGate::Busy => {")
            .expect("begin_action has no Busy arm");
        assert!(src
            .get(gate..gate + 120)
            .unwrap_or(&src[gate..])
            .contains("self.show_busy_card()"));
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

    /// #328: a machine-check result dropped unhandled (Settings open, window
    /// closing, post failure) must free its boxed facts, not leak them.
    #[test]
    fn free_boxed_payload_frees_a_machine_check_result() {
        let facts = Box::into_raw(Box::new(crate::machine::MachineFacts {
            ram_bytes: None,
            gpus: None,
            ollama: crate::machine::OllamaState::NotRunning,
            power: crate::machine::Power::Unknown,
        }));
        assert!(super::free_boxed_payload(
            super::WM_APP_MACHINE_CHECK_RESULT,
            windows::Win32::Foundation::LPARAM(facts as isize)
        ));
    }

    #[test]
    fn choosing_a_keyed_provider_with_no_key_needs_a_key() {
        use crate::config::{ActiveProvider as P, Providers};
        let mut providers = Providers::default();
        providers.gemini.api_key.clear();
        providers.openai.api_key = "k".to_string();
        let need = |p, m| super::provider_needs_key(p, &providers, m);
        assert!(need(P::Gemini, Mode::Cloud));
        assert!(need(P::Gemini, Mode::Auto));
        assert!(!need(P::OpenAi, Mode::Cloud));
        assert!(!need(P::Ollama, Mode::Cloud));
        providers.gemini.api_key = "  ".to_string();
        assert!(super::provider_needs_key(
            P::Gemini,
            &providers,
            Mode::Cloud
        ));
        providers.gemini.api_key = "k".to_string();
        assert!(!super::provider_needs_key(
            P::Gemini,
            &providers,
            Mode::Cloud
        ));
    }

    #[test]
    fn local_and_offline_modes_never_claim_a_key_is_the_problem() {
        use crate::config::{ActiveProvider as P, Providers};
        let mut providers = Providers::default();
        providers.gemini.api_key.clear();
        for mode in [Mode::Local, Mode::Offline] {
            assert!(!super::provider_needs_key(P::Gemini, &providers, mode));
        }
    }

    #[test]
    fn the_missing_key_card_names_the_provider_and_where_to_add_it() {
        let (headline, detail) = super::missing_key_card(crate::config::ActiveProvider::Gemini);
        assert_eq!(headline, "Gemini needs an API key");
        assert!(detail.contains("Settings"), "{detail}");
    }

    #[test]
    fn free_boxed_payload_frees_a_boxed_message_and_ignores_the_rest() {
        use windows::Win32::Foundation::LPARAM;
        let boxed = Box::into_raw(Box::new("palette action".to_string()));
        assert!(super::free_boxed_payload(
            WM_APP_PALETTE_RUN,
            LPARAM(boxed as isize)
        ));
        // Messages that carry no box, and a null lparam, are left alone.
        assert!(!super::free_boxed_payload(WM_APP_TRAY, LPARAM(0)));
        assert!(!super::free_boxed_payload(WM_APP_TRAY, LPARAM(8)));
        assert!(!super::free_boxed_payload(WM_APP_PALETTE_RUN, LPARAM(0)));
    }

    /// #712: the executor result box is freed when its message is dropped
    /// unhandled (Settings open, window gone), so none leaks.
    #[test]
    fn an_executor_result_box_is_freed_when_dropped_unhandled() {
        use windows::Win32::Foundation::LPARAM;
        let done: std::result::Result<ExecDone, String> =
            Ok(ExecDone::RestoreForm(Err(anyhow::anyhow!("x"))));
        let boxed = Box::into_raw(Box::new(done));
        assert!(super::free_boxed_payload(
            WM_APP_EXECUTOR_DONE,
            LPARAM(boxed as isize)
        ));
    }

    struct FakeExecutor {
        fail: bool,
    }

    impl executors::Executor for FakeExecutor {
        fn name(&self) -> &'static str {
            "fake"
        }
        fn effect(&self) -> executors::Effect {
            executors::Effect::Writes
        }
        fn execute(
            &self,
            _confirmed: confirm::Confirmed<Value>,
        ) -> anyhow::Result<executors::Undo> {
            if self.fail {
                anyhow::bail!("the write failed")
            }
            Ok(executors::Undo::none("did it"))
        }
    }

    fn fake_confirmed() -> confirm::Confirmed<Value> {
        confirm::confirm(
            confirm::Proposal::new(serde_json::json!({})),
            confirm::user_confirmed(),
        )
    }

    /// #712: the worker half turns the executor's outcome into a plain
    /// message for the UI thread, and an error keeps its full chain text.
    #[test]
    fn the_executor_worker_half_reports_success_and_failure() {
        let ok = exec_work_form_fill(&FakeExecutor { fail: false }, fake_confirmed());
        assert!(matches!(ok, ExecDone::FormFill(Ok(ref u)) if u.summary == "did it"));
        let bad = exec_work_calendar(&FakeExecutor { fail: true }, fake_confirmed());
        assert!(
            matches!(bad, ExecDone::Calendar(Err(ref e)) if e.root_cause().to_string().contains("the write failed"))
        );
    }

    #[test]
    fn quit_is_refused_only_while_an_executor_is_writing() {
        assert!(super::quit_refusal(true, false).is_some_and(|t| !t.contains('\u{2014}')));
        assert!(super::quit_refusal(false, false).is_none());
    }

    #[test]
    fn quit_is_refused_while_the_ollama_fix_is_running() {
        // #795: quitting between the stop and the restart leaves Ollama stopped.
        let text = super::quit_refusal(false, true).expect("must refuse");
        assert!(!text.contains('\u{2014}'));
        assert!(super::quit_refusal(true, true).is_some());
    }

    #[test]
    fn a_failed_restore_comes_back_as_an_error_message() {
        let undo = executors::Undo::recording("x", || anyhow::bail!("could not put it back"));
        let done = exec_work_restore(undo);
        assert!(
            matches!(done, ExecDone::RestoreForm(Err(ref e)) if e.root_cause().to_string().contains("put it back"))
        );
    }

    /// #712: no executor or undo runs on the UI thread, which is also the
    /// keyboard hook's thread. Every call to one in this file must sit in an
    /// `exec_work_*` function, which only the spawned worker runs.
    #[test]
    fn executors_are_only_called_from_the_worker_half() {
        let src = include_str!("app.rs").replace('\r', "");
        let production = src.split("\nmod tests {").next().expect("production part");
        let mut current = String::new();
        let mut hits = 0;
        for line in production.lines() {
            let t = line.trim_start();
            // Any fn header: strip visibility and qualifiers, then `fn `.
            let mut head = t;
            for vis in ["pub(crate) ", "pub(super) ", "pub "] {
                if let Some(r) = head.strip_prefix(vis) {
                    head = r;
                    break;
                }
            }
            for q in ["const ", "unsafe ", "async ", "extern \"system\" "] {
                head = head.strip_prefix(q).unwrap_or(head);
            }
            if let Some(rest) = head.strip_prefix("fn ") {
                current = rest
                    .split(|c: char| !(c.is_alphanumeric() || c == '_'))
                    .next()
                    .unwrap_or("")
                    .to_string();
            }
            if t.starts_with("//") {
                continue;
            }
            for call in [
                ".execute(",
                "Executor::execute(",
                ".undo()",
                "replace_or_copy(",
                "do_review_confirmed(",
            ] {
                if t.contains(call) {
                    hits += 1;
                    // `copy_region` only puts an image on the clipboard: no UIA,
                    // no typed input, nothing slow.
                    assert!(
                        current.starts_with("exec_work_") || current == "copy_region",
                        "{call} is called from `{current}`, not an exec_work_ function"
                    );
                }
            }
        }
        assert!(hits >= 5, "the scan found only {hits} calls");
    }

    #[test]
    fn begin_plan_without_a_model_skips_gate_capture_and_model_card() {
        let none = BeginPlan::for_fill_form(false);
        assert!(!none.require_provider, "no provider must not block");
        assert!(!none.capture, "nothing to capture when nothing is sent");
        assert!(!none.asks_model, "the card must not claim a model is asked");
        assert_eq!(BeginPlan::for_fill_form(true), BeginPlan::MODEL);
    }

    // -- first-run readiness (issue #133) ----------------------------------

    #[test]
    fn only_the_latest_first_run_probe_is_acted_on() {
        assert!(super::first_run_result_is_current(3, 3));
        assert!(!super::first_run_result_is_current(2, 3), "an older press");
        assert!(!super::first_run_result_is_current(4, 3));
    }

    #[test]
    fn a_quiet_first_run_result_hides_only_its_own_working_card() {
        assert!(super::should_hide_after_first_run(true, false));
        assert!(
            !super::should_hide_after_first_run(false, false),
            "an answer is showing"
        );
        assert!(
            !super::should_hide_after_first_run(true, true),
            "a real run owns the card"
        );
    }

    #[test]
    fn the_adopted_card_names_the_model_and_warns_only_for_the_stock_app() {
        use crate::provider::ollama_admin::ListenerKind;
        let (head, other) = super::first_run_adopted_card("gemma3:4b", ListenerKind::Other);
        assert!(other.contains("gemma3:4b"), "{other}");
        assert!(!other.contains("CPU"), "{other}");
        let (_, stock) = super::first_run_adopted_card("gemma3:4b", ListenerKind::StockTrayServer);
        assert!(
            stock.contains("CPU") && stock.contains("Settings"),
            "{stock}"
        );
        for text in [&head, &other, &stock] {
            assert!(
                !text.contains('\u{2014}') && !text.contains('\u{2013}'),
                "{text}"
            );
        }
    }

    #[test]
    fn a_first_run_probe_box_is_freed_when_dropped_unhandled() {
        use windows::Win32::Foundation::LPARAM;
        let boxed = Box::into_raw(Box::new(FirstRun::NoVisionModel));
        assert!(super::free_boxed_payload(
            WM_APP_FIRST_RUN,
            LPARAM(boxed as isize)
        ));
    }

    #[test]
    fn first_run_card_only_changes_the_card_when_ollama_has_no_vision_model() {
        use crate::provider::ollama_admin::{FirstRun, ListenerKind};
        assert!(super::first_run_detail(&FirstRun::NotRunning).is_none());
        let found = FirstRun::Found {
            model: "gemma3:4b".to_string(),
            kind: ListenerKind::Other,
        };
        assert!(super::first_run_detail(&found).is_none());
        let detail = super::first_run_detail(&FirstRun::NoVisionModel).expect("a detail");
        assert!(detail.contains("gemma3:4b"), "{detail}");
        assert!(detail.contains("Settings"), "{detail}");
        assert!(
            !detail.contains('\u{2014}') && !detail.contains('\u{2013}'),
            "{detail}"
        );
        // An older Ollama that lists no capabilities: say so, do not claim
        // that no model can read the screen.
        let unknown = super::first_run_detail(&FirstRun::VisionUnknown).expect("a detail");
        assert!(unknown.contains("could not tell"), "{unknown}");
        assert!(!unknown.contains("none of its models"), "{unknown}");
        assert!(unknown.contains("Settings"), "{unknown}");
        assert!(!unknown.contains('\u{2014}') && !unknown.contains('\u{2013}'));
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
        // Issue #347: "Edit settings" is not a real menu item (the tray has
        // "Settings..." and "Open config.toml"). The card must name a real
        // action, and clicking it must actually open Settings.
        assert_eq!(headline, "No AI model set up yet");
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
        // Issue #347: same fix as Cloud mode's headline above.
        assert_eq!(headline, "No AI model set up yet");
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
        // AGENTS.md rule 11.
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

    // -- #831: the welcome card on the very first launch -------------------

    #[test]
    fn welcome_decision_against_a_scratch_folder() {
        use std::fs;
        let dir = std::env::temp_dir().join(format!("wingman-test-welcome-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let cfg = dir.join("config.toml");
        let old = dir.join("old-config.toml");
        let show = |existed, settings, other| {
            super::should_show_welcome(&cfg, &old, existed, settings, other)
        };

        // Unwritable folder: load could not create the file, so no card.
        assert!(!show(false, false, false));

        // Fresh start: absent before, created by this start.
        fs::write(&cfg, "").unwrap();
        assert!(show(false, false, false));

        // Every later launch (and autostart): the file existed before.
        assert!(!show(true, false, false));
        // install.ps1's `--settings` launch already opens Settings.
        assert!(!show(false, true, false));
        // A startup problem card must not be replaced.
        assert!(!show(false, false, true));

        // Unfinished copilot-ask migration: a returning user.
        fs::write(&old, "").unwrap();
        assert!(!show(false, false, false));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn welcome_card_names_the_keys_and_the_tray_icon() {
        let (headline, detail) = super::welcome_card("Ctrl+Shift+/");
        assert!(headline.starts_with("Wingman is running"), "{headline}");
        assert!(detail.contains("Copilot key"), "{detail}");
        assert!(detail.contains("Ctrl+Shift+/"), "{detail}");
        assert!(detail.contains("tray"), "{detail}");
        assert!(detail.contains("Click here"), "{detail}");
        assert!(!headline.contains('\u{2014}') && !detail.contains('\u{2014}'));
    }

    // -- #832: an empty chain at run time ends in the setup card ----------

    #[test]
    fn an_empty_chain_error_is_recognised_by_its_text() {
        let err = anyhow::Error::new(crate::provider::NoProviders);
        let packed = super::pack_error("x", &super::worker_error_chain_text(&err));
        assert!(super::is_no_providers(&packed));
        let wrapped = anyhow::anyhow!("boom").context("outer");
        let packed = super::pack_error("x", &super::worker_error_chain_text(&wrapped));
        assert!(!super::is_no_providers(&packed));
    }

    #[test]
    fn every_action_result_handler_checks_for_an_empty_chain() {
        // CRLF-safe; each handler must offer the packed error to the setup
        // card before it builds a generic failure card.
        let src = include_str!("app.rs").replace('\r', "");
        for name in [
            "on_calendar_result",
            "on_generic_action_result",
            "on_review_result",
            "on_form_fill_result",
        ] {
            let def = format!("    fn {name}(");
            let start = src.find(&def).expect("handler exists");
            let body = &src[start..];
            let end = body.find("unpack_error(&packed)").expect("failure arm");
            assert!(
                body[..end].contains("show_empty_chain_failure(&packed)"),
                "{name} must call show_empty_chain_failure first"
            );
        }
    }

    #[test]
    fn empty_chain_card_says_ollama_is_not_running_when_only_ollama_is_listed() {
        let (headline, detail) =
            super::empty_chain_card(Mode::Auto, &ollama_only_providers(), "config.toml", None);
        assert_eq!(headline, "Ollama is not running");
        assert!(detail.contains("Start Ollama"), "{detail}");
        assert!(detail.contains("Settings"), "{detail}");
        assert!(!detail.contains("Try again"), "{detail}");
    }

    /// #892: a config.toml that did not parse starts the run on default
    /// providers; the setup card must name the file, not tell the user to
    /// edit `providers.order` in the file Wingman could not read.
    #[test]
    fn a_file_that_did_not_parse_is_named_by_the_gate_and_the_empty_chain_card() {
        let problem = Some("config.toml has an error at line 3, column 1");
        let path = "C:\\Users\\me\\config.toml";
        let providers = nothing_configured_providers();
        let cards = [
            super::setup_gate(Mode::Offline, &providers, path, problem).expect("blocked"),
            super::empty_chain_card(Mode::Offline, &providers, path, problem),
            super::empty_chain_card(Mode::Auto, &ollama_only_providers(), path, problem),
        ];
        for (headline, detail) in cards {
            assert_eq!(headline, "Settings file not read");
            assert!(detail.contains("line 3, column 1"), "{detail}");
            assert!(detail.contains(path), "{detail}");
            assert!(detail.contains("Open config.toml"), "{detail}");
            assert!(!detail.contains("providers.order"), "{detail}");
            assert!(!detail.contains('\u{2014}'));
        }
        // No problem: the old cards, unchanged. A ready provider still passes.
        assert_eq!(
            super::setup_gate(Mode::Offline, &providers, path, None),
            App::readiness_gate(Mode::Offline, &providers, path)
        );
        assert!(super::setup_gate(Mode::Cloud, &cloud_ready_providers(), path, problem).is_none());
    }

    #[test]
    fn a_broken_file_card_keeps_its_detail_whatever_the_ollama_probe_found() {
        let broken = "The file is here".to_string();
        for found in [FirstRun::NoVisionModel, FirstRun::VisionUnknown] {
            assert_eq!(
                super::first_run_card_detail(broken.clone(), &found, Some("line 3")),
                broken
            );
            assert_ne!(
                super::first_run_card_detail(broken.clone(), &found, None),
                broken,
                "without a load problem the probe's detail still applies"
            );
        }
    }

    #[test]
    fn empty_chain_card_is_the_gate_card_when_ollama_is_not_listed() {
        let providers = nothing_configured_providers();
        assert_eq!(
            Some(super::empty_chain_card(
                Mode::Auto,
                &providers,
                "config.toml",
                None
            )),
            App::readiness_gate(Mode::Auto, &providers, "config.toml")
        );
    }

    #[test]
    fn empty_chain_cards_have_no_em_dash() {
        for providers in [ollama_only_providers(), nothing_configured_providers()] {
            let (headline, detail) =
                super::empty_chain_card(Mode::Auto, &providers, "c.toml", None);
            assert!(!headline.contains('\u{2014}') && !detail.contains('\u{2014}'));
        }
    }

    // -- first_provider_model_label / provider_model_label_for (issue #354
    //    follow-up review, finding 4) -------------------------------------

    #[test]
    fn provider_model_label_is_mode_aware_not_just_order_first() {
        // order = ["openai", "ollama"], but mode = Local: Local mode never
        // tries "openai" (`select_providers`/`is_local_provider`), so the
        // label must name "ollama", not "openai". Before the fix this read
        // `providers.order.first()` directly and asserted "openai" here,
        // which is the exact wrong-model-name bug finding 4 reports.
        let mut providers = Providers {
            order: vec!["openai".to_string(), "ollama".to_string()],
            ..Providers::default()
        };
        providers.openai.api_key = "sk-real".to_string();
        providers.openai.model = "gpt-5".to_string();
        providers.ollama.base_url = "http://127.0.0.1:11434".to_string();
        providers.ollama.model = "llava".to_string();

        let label = App::provider_model_label_for(Mode::Local, &providers)
            .expect("ollama is configured and selected for Local mode");
        assert_eq!(label, "ollama:llava");
    }

    #[test]
    fn provider_model_label_matches_order_first_when_mode_does_not_filter_it_out() {
        // Cloud mode DOES select "openai" first here, so this stays "openai"
        // -- a control case proving the fix did not just always answer
        // "ollama".
        let mut providers = Providers {
            order: vec!["openai".to_string(), "ollama".to_string()],
            ..Providers::default()
        };
        providers.openai.api_key = "sk-real".to_string();
        providers.openai.model = "gpt-5".to_string();

        let label = App::provider_model_label_for(Mode::Cloud, &providers)
            .expect("openai is configured for Cloud mode");
        assert_eq!(label, "openai:gpt-5");
    }

    #[test]
    fn provider_model_label_is_none_when_mode_filtering_leaves_nothing() {
        // Cloud mode with only ollama configured: nothing survives
        // `selected_order_for_mode`, so the label is None (never a stale
        // name from an unfiltered `order`).
        assert!(App::provider_model_label_for(Mode::Cloud, &ollama_only_providers()).is_none());
    }

    // -- form_fill_headline / calendar_headline (issue #268) ---------------

    #[test]
    fn form_fill_headline_says_nothing_filled_when_every_field_was_skipped_or_refused() {
        // The exact shape `executors::fill_form::format_outcomes` produces
        // when every field is skipped or refused -- see its own test
        // `format_outcomes_counts_filled_and_lists_the_rest`'s style.
        let summary = "Filled 0 of 3 fields. Skipped \"Password\": password control. \
                        Refused \"Place order\": invokable or forbidden target. \
                        Refused \"Total\": payment-shaped label.";
        assert_eq!(App::form_fill_headline(summary), "Nothing filled");
    }

    #[test]
    fn form_fill_headline_says_form_filled_when_every_field_was_filled() {
        let summary = "Filled 2 of 2 fields.";
        assert_eq!(App::form_fill_headline(summary), "Form filled");
    }

    #[test]
    fn form_fill_headline_says_partially_filled_for_a_mixed_outcome() {
        let summary = "Filled 1 of 3 fields. Skipped \"Password\": password control.";
        assert_eq!(App::form_fill_headline(summary), "Form partially filled");
    }

    #[test]
    fn form_fill_headline_falls_back_safely_on_an_unparseable_summary() {
        // Defensive only: `format_outcomes` is the sole producer of this
        // string and is itself covered by
        // `format_outcomes_counts_filled_and_lists_the_rest`, so this
        // should never fire in production. Proves the fallback doesn't
        // panic rather than asserting a specific string.
        let _ = App::form_fill_headline("not the expected shape at all");
    }

    #[test]
    fn form_fill_headlines_never_use_an_em_dash() {
        for summary in [
            "Filled 0 of 3 fields. Skipped \"Password\": password control.",
            "Filled 2 of 2 fields.",
            "Filled 1 of 3 fields. Skipped \"Password\": password control.",
        ] {
            let headline = App::form_fill_headline(summary);
            assert!(!headline.contains('\u{2014}'), "{headline}");
        }
    }

    #[test]
    fn calendar_headline_says_opened_when_the_connector_opened_it() {
        // The exact shape `CalendarAddExecutor::execute` produces when
        // `result.opened` is `true` (`opened_note` is empty).
        let summary = "Added \"Standup\" to your calendar via ics. Undo removes this file; \
                        removing the event from your calendar app is a manual step.";
        assert_eq!(
            App::calendar_headline(summary),
            "Event opened in your calendar app"
        );
    }

    #[test]
    fn calendar_headline_says_not_opened_when_the_connector_could_not_open_it() {
        // The exact shape when `result.opened` is `false`: `opened_note`
        // appends "...it could not be opened automatically; open ...
        // yourself" before the fixed Undo sentence.
        let summary = "Added \"Standup\" to your calendar via ics, but it could not be opened \
                        automatically; open C:\\temp\\standup.ics yourself. Undo removes this \
                        file; removing the event from your calendar app is a manual step.";
        assert_eq!(App::calendar_headline(summary), "Event added, not opened");
    }

    #[test]
    fn calendar_headlines_never_use_an_em_dash() {
        for summary in [
            "Added \"Standup\" to your calendar via ics.",
            "Added \"Standup\" to your calendar via ics, but it could not be opened automatically; open x yourself.",
        ] {
            let headline = App::calendar_headline(summary);
            assert!(!headline.contains('\u{2014}'), "{headline}");
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
    fn a_config_that_did_not_parse_allows_only_offline() {
        // #793: one tray click must not switch cloud on with default settings.
        let problem = Some("config.toml has an error at line 1, column 1");
        assert!(mode_allowed(Mode::Offline, problem));
        for m in [Mode::Cloud, Mode::Local, Mode::Auto] {
            assert!(!mode_allowed(m, problem), "{m:?}");
            assert!(mode_allowed(m, None), "{m:?}");
        }
    }

    /// #890: a refused Settings save leaves the running config exactly as it
    /// was; a saved one replaces it.
    #[test]
    fn a_refused_settings_save_leaves_the_running_config_untouched() {
        let mut current = crate::config::Config {
            mode: Mode::Cloud,
            ..Default::default()
        };
        let before = current.clone();
        let mut edited = current.clone();
        edited.mode = Mode::Local;
        edited.ui.card_seconds += 5;
        let refused = super::commit_edited_config(&mut current, edited, |_| {
            Err(anyhow::anyhow!("config.toml changed on disk"))
        });
        assert!(refused.is_err());
        assert_eq!(current, before);

        let mut edited = before.clone();
        edited.ui.card_seconds += 5;
        let want = edited.clone();
        super::commit_edited_config(&mut current, edited, |_| Ok(())).expect("saved");
        assert_eq!(current, want);
    }

    #[test]
    fn an_edited_mode_is_held_to_offline_while_the_file_does_not_parse() {
        let mut current = crate::config::Config {
            load_problem: Some("config.toml has an error at line 1, column 1".into()),
            ..Default::default()
        };
        let mut edited = current.clone();
        edited.mode = Mode::Cloud;
        super::commit_edited_config(&mut current, edited, |saved| {
            assert_eq!(saved.mode, Mode::Offline);
            Ok(())
        })
        .expect("saved");
        assert_eq!(current.mode, Mode::Offline);
    }

    #[test]
    fn a_refused_save_card_says_nothing_was_applied() {
        let detail = super::refused_save_detail("config.toml changed on disk");
        assert!(detail.starts_with("config.toml changed on disk"));
        assert!(detail.contains("not applied"), "{detail}");
        assert!(detail.contains("key"), "{detail}");
        assert!(!detail.contains('\u{2014}'));
    }

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
    fn a_close_request_is_remembered_once_and_then_cleared() {
        // #673: the flag is set by wnd_proc during a nested loop and must be
        // consumed exactly once, or a later Settings open+close would quit.
        let _ = super::take_close_request();
        assert!(!super::take_close_request());
        super::note_close_request();
        assert!(super::take_close_request());
        assert!(
            !super::take_close_request(),
            "consumed, so it cannot fire again"
        );
    }

    #[test]
    fn every_modal_path_that_clears_settings_open_reposts_a_pending_close() {
        // The tray menu, Settings and file dialogs all set SETTINGS_OPEN, and
        // wnd_proc swallows WM_CLOSE while it is set. Each of them has to
        // repost it afterwards. Source scan, CRLF-safe, non-empty.
        let src = include_str!("app.rs").replace('\r', "");
        let code = src.split("#[cfg(test)]\nmod tests {").next().unwrap();
        let clears = code
            .matches("SETTINGS_OPEN.with(|c| c.set(false));")
            .count();
        assert_eq!(
            clears, 3,
            "a modal path was added or removed: update this test"
        );
        let reposts = code.matches("self.repost_close_if_requested();").count();
        assert_eq!(
            reposts, clears,
            "each path that clears SETTINGS_OPEN must call repost_close_if_requested"
        );
    }

    #[test]
    fn settings_reentrancy_turns_wm_close_into_a_close_after_settings() {
        // #673: the default path destroyed the owner behind the modal and
        // the process never left its message loop.
        assert_eq!(
            settings_reentrancy_policy(WM_CLOSE, FAKE_TASKBAR_CREATED_MSG),
            SettingsReentrancy::CloseAfterSettings
        );
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
        ("WM_APP_CARD_OPEN_SETTINGS", WM_APP_CARD_OPEN_SETTINGS),
        ("WM_APP_GENERIC_ACTION_RESULT", WM_APP_GENERIC_ACTION_RESULT),
        ("WM_APP_CARD_COPY_DETAILS", WM_APP_CARD_COPY_DETAILS),
        ("WM_APP_SEND_PREVIEW", WM_APP_SEND_PREVIEW),
        ("WM_APP_PENDING_STAGE", WM_APP_PENDING_STAGE),
        ("WM_APP_MACHINE_CHECK_RESULT", WM_APP_MACHINE_CHECK_RESULT),
        ("WM_APP_FIRST_RUN", WM_APP_FIRST_RUN),
        ("WM_APP_EXECUTOR_DONE", WM_APP_EXECUTOR_DONE),
        ("WM_APP_OPTIONS_CHOICE", WM_APP_OPTIONS_CHOICE),
    ];

    // -- #567: the tray's Ollama model cache --------------------------------

    fn names(list: &[(&str, bool)]) -> Vec<(String, bool)> {
        list.iter().map(|(n, v)| (n.to_string(), *v)).collect()
    }

    #[test]
    fn ollama_cache_hands_over_a_finished_list_once_and_resets_in_flight() {
        let cache = super::OllamaModelCache::default();
        let url = "http://127.0.0.1:11434";
        let h = cache
            .refresh_with(url, |_| names(&[("gemma3:4b", true)]))
            .expect("a fetch starts");
        h.join().unwrap();
        assert!(cache.state.lock().unwrap().in_flight.is_none());
        assert_eq!(cache.take_for(url), Some(names(&[("gemma3:4b", true)])));
        assert_eq!(cache.take_for(url), None, "taken once");
        // in_flight was reset, so another fetch can start.
        assert!(cache.refresh_with(url, |_| Vec::new()).is_some());
    }

    #[test]
    fn ollama_cache_drops_a_list_for_another_url() {
        let cache = super::OllamaModelCache::default();
        cache
            .refresh_with("http://127.0.0.1:11434", |_| names(&[("a", false)]))
            .unwrap()
            .join()
            .unwrap();
        assert_eq!(cache.take_for("http://127.0.0.1:9999"), None);
    }

    #[test]
    fn ollama_cache_respawns_for_a_new_url_and_ignores_the_stale_answer() {
        use std::sync::mpsc;
        let cache = super::OllamaModelCache::default();
        let (release_old, wait_old) = mpsc::channel::<()>();
        let old = cache
            .refresh_with("http://old", move |_| {
                let _ = wait_old.recv();
                names(&[("old", false)])
            })
            .unwrap();
        // Same URL while running: no second fetch.
        assert!(cache.refresh_with("http://old", |_| Vec::new()).is_none());
        // New URL: its own fetch, and it finishes first.
        cache
            .refresh_with("http://new", |_| names(&[("new", false)]))
            .expect("a changed URL respawns")
            .join()
            .unwrap();
        release_old.send(()).unwrap();
        old.join().unwrap();
        assert_eq!(cache.take_for("http://new"), Some(names(&[("new", false)])));
        assert_eq!(cache.take_for("http://old"), None);
    }

    #[test]
    fn ollama_menu_refresh_is_wanted_only_for_ollama_in_the_chain_or_loopback() {
        use super::ollama_menu_refresh_wanted as wanted;
        let order = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert!(wanted(&order(&["openai"]), "http://127.0.0.1:11434"));
        assert!(wanted(&order(&["ollama"]), "http://192.168.1.5:11434"));
        assert!(!wanted(&order(&["openai"]), "http://192.168.1.5:11434"));
        assert!(!wanted(&order(&["openai"]), "http://localhost:11434"));
    }

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
            + count_declarations(include_str!("ui/palette.rs"))
            + count_declarations(include_str!("options_keys.rs"));

        assert_eq!(
            declared,
            ALL_WM_APP_IDS.len(),
            "found {declared} `pub const WM_APP_* = WM_APP + n;` declarations across \
             app.rs/dismiss.rs/hotkey.rs/ui/tray.rs/ui/card.rs/ui/palette.rs/options_keys.rs but \
             ALL_WM_APP_IDS lists {}; add the new constant to ALL_WM_APP_IDS too",
            ALL_WM_APP_IDS.len()
        );
    }

    /// Issue #234, the half `ui::tray`'s own menu test cannot cover: every
    /// command id declared in `ui::tray::cmd` must have a `WM_COMMAND` arm
    /// in `wnd_proc`, or the user clicks a real menu item and nothing
    /// happens. Same source-scanning technique as
    /// `wm_app_ids_registry_is_exhaustive` above, for the same reason: the
    /// match arms are ordinary code that no type system ties to the
    /// constant list.
    ///
    /// Filed after a real instance: `cmd::COPY_EGRESS_LOG` was first given
    /// id 1020, already held by `EXTRACT_TEXT`. That one the compiler caught
    /// as an unreachable arm, but a NEW id with no arm at all is silent.
    ///
    /// The three ids excluded below are not fixed menu commands: the two
    /// `*_MODEL_BASE` values are the starts of the dynamic model-submenu
    /// ranges, dispatched through `MenuChoice::{OpenAiModel, AnthropicModel}`
    /// rather than by exact id, and `MODEL_RANGE` is that range's width.
    #[test]
    fn every_tray_cmd_id_has_a_wnd_proc_arm() {
        // Not fixed menu commands. The two *_MODEL_BASE values start the
        // dynamic model-submenu ranges (dispatched by MenuChoice::OpenAiModel /
        // AnthropicModel, not by exact id) and MODEL_RANGE is that width.
        const DYNAMIC: &[&str] = &[
            "MODEL_RANGE",
            "OPENAI_MODEL_BASE",
            "ANTHROPIC_MODEL_BASE",
            "OLLAMA_MODEL_BASE",
            "ACTION_EXPORT_BASE",
        ];
        // Display-only items: added disabled (greyed), so no click can ever
        // produce their WM_COMMAND and they need no wnd_proc arm (#317).
        const DISPLAY_ONLY: &[&str] = &["VERSION_LABEL"];

        let tray_src = include_str!("ui/tray.rs");
        let app_src = include_str!("app.rs");

        let declared: Vec<&str> = tray_src
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
            .filter(|name| {
                !DYNAMIC.contains(name)
                    && !DISPLAY_ONLY.contains(name)
                    && !name.starts_with("WM_APP_")
            })
            .collect();

        assert!(
            declared.len() > 20,
            "scanner found only {} cmd constants in ui/tray.rs; the declaration format must have changed and this test is no longer checking anything",
            declared.len()
        );

        let unhandled: Vec<&str> = declared
            .iter()
            .filter(|name| !app_src.contains(&format!("MenuChoice::Command(cmd::{name})")))
            .copied()
            .collect();

        assert!(
            unhandled.is_empty(),
            "these ui::tray::cmd ids have no `MenuChoice::Command(cmd::NAME)` arm in wnd_proc, so clicking their menu item does nothing: {unhandled:?}"
        );
    }

    // -- WM_APP_* wiring, both ends ---------------------------------------
    //
    // #163's `wm_app_ids_registry_is_exhaustive` proves every `WM_APP_*`
    // constant in the crate is listed in `ALL_WM_APP_IDS`, and #213's
    // `reentrancy_policy_table_matches_all_wm_app_ids` proves every listed
    // id is classified. Neither proves the id is *connected to anything*: a
    // constant can be declared, listed, classified, and still have no arm
    // in `wnd_proc` (so the sender's message is silently swallowed by
    // `DefWindowProcW`) or no sender at all (so the handler is dead code
    // that reads as a live feature). Both are the wired-to-nothing bug this
    // repo's skill of that name exists for, and neither shows up as a
    // compile error, a clippy warning or a failing test.
    //
    // The two tests below close each end. They are source scans for the
    // same reason `every_tray_cmd_id_has_a_wnd_proc_arm` is one: the match
    // arms and the `PostMessageW` call sites are ordinary code that no type
    // system ties back to the constant list.

    /// Slice `app.rs`'s `wnd_proc` body out of the source text.
    ///
    /// Scanning the whole file would let `settings_reentrancy_policy`'s own
    /// `WM_APP_* =>` arms satisfy the dispatch check, which is exactly the
    /// vacuous pass this test exists to prevent: an id can be classified
    /// there and still have no arm in `wnd_proc`. rustfmt keeps a top-level
    /// item's closing brace in column 0, so the first `\n}\n` after the
    /// signature ends the function.
    ///
    /// Takes already-normalised source: the working copy on this machine is
    /// CRLF (`.gitattributes`/`core.autocrlf`), so `include_str!` hands back
    /// `\r\n` and every `\n`-anchored pattern below would silently miss.
    /// [`lf`] does the normalising, once, for both tests.
    fn wnd_proc_body(app_src: &str) -> &str {
        const SIG: &str = "extern \"system\" fn wnd_proc(";
        let start = app_src
            .find(SIG)
            .expect("app.rs no longer contains a `extern \"system\" fn wnd_proc(` signature; this scanner is no longer looking at anything");
        let rest = &app_src[start..];
        let end = rest
            .find("\n}\n")
            .expect("no column-0 closing brace found after wnd_proc's signature");
        &rest[..end]
    }

    /// Strip `\r` so these source scans read the same whether the working
    /// copy checked out LF or CRLF. Without it both tests pass vacuously on
    /// one of the two, which is the worse failure: a guard that reports
    /// green while checking nothing.
    fn lf(src: &str) -> String {
        src.replace('\r', "")
    }

    /// Slice `App::apply_config`'s body out of the source text, the same
    /// technique [`wnd_proc_body`] uses for `wnd_proc`. `apply_config` is a
    /// method (`fn apply_config(&mut self) {`), so unlike a top-level item
    /// its own closing brace sits at 4 columns of indent, not 0 -- the first
    /// `\n    }\n` after the signature ends it. Takes already-`lf`-normalised
    /// source; see [`lf`]'s doc comment for why.
    fn apply_config_body(app_src: &str) -> &str {
        const SIG: &str = "fn apply_config(&mut self) {";
        let start = app_src
            .find(SIG)
            .expect("app.rs no longer contains a `fn apply_config(&mut self) {` signature; this scanner is no longer looking at anything");
        let rest = &app_src[start..];
        let end = rest
            .find("\n    }\n")
            .expect("no 4-space-indented closing brace found after apply_config's signature");
        &rest[..end]
    }

    /// Slice `App::run`'s body out of the source text, the same technique
    /// [`wnd_proc_body`] uses for `wnd_proc`. `run` is a top-level function,
    /// so like `wnd_proc` its closing brace sits at column 0. Takes
    /// already-`lf`-normalised source; see [`lf`]'s doc comment for why.
    fn run_body(app_src: &str) -> &str {
        const SIG: &str = "pub fn run() -> Result<()> {";
        let start = app_src
            .find(SIG)
            .expect("app.rs no longer contains a `pub fn run() -> Result<()> {` signature; this scanner is no longer looking at anything");
        let rest = &app_src[start..];
        let end = rest
            .find("\n}\n")
            .expect("no column-0 closing brace found after run's signature");
        &rest[..end]
    }

    /// Every id in `ALL_WM_APP_IDS` must be dispatched somewhere in
    /// `wnd_proc`, or the message posted to the owner window falls through
    /// to `DefWindowProcW` and nothing happens. The sender looks correct,
    /// the constant is registered, the reentrancy table classifies it, and
    /// the feature is dead.
    ///
    /// Two arm shapes count, because `wnd_proc` uses both: a `match msg`
    /// arm (`WM_APP_RESULT => {`) and the `if msg == WM_APP_PALETTE_RUN`
    /// chain that runs before the match for the ids needing `hwnd`.
    #[test]
    fn every_wm_app_id_has_a_wnd_proc_arm() {
        let app_src = lf(include_str!("app.rs"));
        let body = wnd_proc_body(&app_src);

        assert!(
            body.len() > 5_000,
            "wnd_proc's extracted body is only {} bytes; the slicing in `wnd_proc_body` has broken and this test is no longer checking anything",
            body.len()
        );

        let undispatched: Vec<&str> = ALL_WM_APP_IDS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| {
                !body.contains(&format!("{name} =>"))
                    && !body.contains(&format!("{name}\n"))
                    && !body.contains(&format!("msg == {name}"))
            })
            .collect();

        assert!(
            undispatched.is_empty(),
            "these WM_APP_* ids have no dispatch in wnd_proc, so a message posted with them is swallowed by DefWindowProcW and the feature silently does nothing: {undispatched:?}"
        );
    }

    /// Every id in `ALL_WM_APP_IDS` must be posted by somebody. A handler
    /// with no sender is dead code that reads as a live feature, and it
    /// survives every other check in this file: the constant is declared,
    /// listed, classified and dispatched, and nothing ever sends it.
    ///
    /// Scans each `PostMessageW(`/`SendMessageW(` call site's argument list
    /// rather than the whole file, so a mention in a doc comment (there are
    /// dozens) cannot satisfy it.
    #[test]
    fn every_wm_app_id_is_posted_somewhere() {
        /// Every file in the crate that posts a window message. A new one
        /// added here without being listed would make this test report a
        /// false positive, which is the safe direction: it fails loudly
        /// rather than passing vacuously.
        const POSTING_SOURCES: &[&str] = &[
            include_str!("app.rs"),
            include_str!("dismiss.rs"),
            include_str!("hotkey.rs"),
            include_str!("single_instance.rs"),
            include_str!("ui/card.rs"),
            include_str!("ui/palette.rs"),
            include_str!("ui/region.rs"),
            include_str!("ui/send_gate.rs"),
            include_str!("ui/settings.rs"),
            include_str!("ui/tray.rs"),
            include_str!("inputs/selection.rs"),
        ];

        /// The argument text of every `PostMessageW`/`SendMessageW` call,
        /// capped at a fixed span rather than brace-matched: the calls are
        /// rustfmt'd to at most a handful of short lines, and a fixed span
        /// cannot be fooled by a brace inside a string literal.
        fn call_argument_text(sources: &[&str]) -> String {
            const SPAN: usize = 240;
            let mut out = String::new();
            for raw in sources {
                let src = &lf(raw);
                for pat in ["PostMessageW(", "SendMessageW("] {
                    let mut from = 0;
                    while let Some(i) = src[from..].find(pat) {
                        let start = from + i + pat.len();
                        let end = src.len().min(start + SPAN);
                        // Never slice inside a UTF-8 sequence: these files
                        // contain non-ASCII in comments and card strings.
                        let end = (start..=end)
                            .rev()
                            .find(|&e| src.is_char_boundary(e))
                            .unwrap_or(start);
                        out.push_str(&src[start..end]);
                        out.push('\n');
                        from = start;
                    }
                }
            }
            out
        }

        let posted = call_argument_text(POSTING_SOURCES);

        assert!(
            posted.len() > 2_000,
            "the PostMessageW/SendMessageW scanner collected only {} bytes of argument text; the call format must have changed and this test is no longer checking anything",
            posted.len()
        );

        let never_posted: Vec<&str> = ALL_WM_APP_IDS
            .iter()
            .map(|(name, _)| *name)
            .filter(|name| !posted.contains(*name))
            .collect();

        assert!(
            never_posted.is_empty(),
            "these WM_APP_* ids are never passed to PostMessageW/SendMessageW anywhere in the crate, so their wnd_proc arms are dead code that reads as a live feature: {never_posted:?}"
        );
    }

    // -- apply_config re-syncs the mode mirror (issue #283) ----------------

    /// `App::run`'s startup sequence calls both `mode::set_current` (the
    /// process-wide atomic `provider::common::offline_guard` reads via
    /// `mode::is_offline_now()`) and `self.tray.set_mode` (the tray's Mode
    /// radio-check) right after loading `Config` -- see lines 242 and 304.
    /// `apply_config` -- the function both `reload()` (tray "Reload") and a
    /// Settings save call after replacing `self.config` wholesale -- must
    /// do the same, or a Reload/Settings-save with a changed `Config.mode`
    /// leaves both mirrors on the OLD mode until the next full restart.
    /// That is exactly #283: switching to Offline this way leaves
    /// `offline_guard` reading the stale (non-Offline) mirror, so it skips
    /// its loopback check entirely and a non-loopback-configured Ollama (or
    /// any `compat:` provider pointed off-box) keeps sending while the user
    /// believes Offline is on.
    #[test]
    fn apply_config_resyncs_the_mode_mirror() {
        let app_src = lf(include_str!("app.rs"));
        let body = apply_config_body(&app_src);
        assert!(
            body.contains("mode::set_current("),
            "apply_config never calls mode::set_current, so a Reload/Settings-save with a changed Config.mode leaves offline_guard's process-wide mirror on the OLD mode (#283):\n{body}"
        );
        assert!(
            body.contains("set_mode("),
            "apply_config never calls the tray's set_mode, so a Reload/Settings-save with a changed Config.mode leaves the tray's Mode radio-check on the OLD mode (#283):\n{body}"
        );
    }

    // -- apply_config re-syncs the palette chord (issue #269) --------------

    /// `App::run`'s startup sequence installs the hook and then calls
    /// `set_pause_chord` AND `set_palette_chord` on it (lines 330 and 333).
    /// `apply_config` already re-syncs `set_bindings` and `set_pause_chord`
    /// (issue #181's fix) but never `set_palette_chord`, so a hand-edited
    /// `[hotkeys.palette]` plus Reload, or a future Settings save that
    /// changes the palette chord, keeps the OLD chord live in the keyboard
    /// hook until the next full app restart -- the exact bug #181 already
    /// fixed for the pause chord, reintroduced for palette.
    #[test]
    fn apply_config_resyncs_the_palette_chord() {
        let app_src = lf(include_str!("app.rs"));
        let body = apply_config_body(&app_src);
        assert!(
            body.contains("set_palette_chord("),
            "apply_config never calls hook.set_palette_chord, so a Reload/Settings-save with a changed hotkeys.palette leaves the OLD chord live until a full restart (#269):\n{body}"
        );
    }

    // -- every startup config-mirror setter is resynced by apply_config ----
    //
    // #283 (mode::set_current / tray.set_mode) and #269
    // (hook.set_palette_chord) are two instances of one root cause: a
    // subsystem holds its OWN copy of a `Config` value (a process-wide
    // atomic, the tray's radio-check, the keyboard hook's packed chord),
    // `App::run` publishes it there at startup, and `apply_config` -- the
    // function both `reload()` (tray "Reload") and a Settings save call
    // after replacing `self.config` wholesale -- forgets to publish it
    // again. The two tests immediately above catch exactly those two rows;
    // this generalises the check so a THIRD row cannot ship silently.

    /// A "config mirror setter" is any `set_*(...)` call in `App::run`
    /// whose own source line also mentions `config.` -- i.e. it is
    /// threading a value straight out of the freshly loaded `Config` into
    /// some setter, unlike e.g. `card.set_owner(hwnd)` (no `config.` on
    /// that line) or `HotkeyHook::install(hwnd, primary, secondary)` (not a
    /// `set_*` call at all -- a constructor call has no `apply_config`-time
    /// equivalent to check against, so it is correctly out of scope).
    /// Matches by NAME only, not by receiver: the hook's local variable is
    /// `h` in `run` and `hook` in `apply_config`, and the tray is
    /// `app.tray` versus `self.tray`.
    fn config_mirror_setter_names(body: &str) -> Vec<&str> {
        fn setter_name(line: &str) -> Option<&str> {
            let at = line.find("set_")?;
            let after = &line[at..];
            let paren = after.find('(')?;
            let name = &after[..paren];
            (!name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
                .then_some(name)
        }

        let mut names: Vec<&str> = body
            .lines()
            .filter(|line| line.contains("config."))
            .filter_map(setter_name)
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Mutation-checked 2026-09-19: with `hook.set_palette_chord(...)`
    /// deleted from `apply_config` (not merely commented out -- a comment
    /// still contains the call text and would pass this test vacuously,
    /// which is exactly how the first draft of this check on this machine
    /// tonight failed to catch its own planted mutation), this test failed
    /// with `["set_current", "set_mode", "set_palette_chord"]` in the
    /// message (the same run also had #283's two calls deleted); restored,
    /// it passes.
    #[test]
    fn every_startup_config_mirror_setter_is_resynced_by_apply_config() {
        let app_src = lf(include_str!("app.rs"));
        let run_src = run_body(&app_src);
        let apply_src = apply_config_body(&app_src);

        let candidates = config_mirror_setter_names(run_src);

        // Non-empty-candidate-set check (issue #281 was exactly a guard
        // that skipped this and passed vacuously once its own scan broke):
        // today there are 7 (set_current, set_egress_preview_enabled, set_daily_budget,
        // set_text_scale, set_mode, set_pause_chord, set_palette_chord);
        // require a comfortable margin under that so losing one candidate
        // to a future refactor cannot silently zero out this check.
        assert!(
            candidates.len() >= 4,
            "scanner found only {} config-mirror setter names in App::run; the scan or run's shape has changed and this test is no longer checking anything: {candidates:?}",
            candidates.len()
        );

        let unresynced: Vec<&str> = candidates
            .iter()
            .filter(|name| !apply_src.contains(&format!("{name}(")))
            .copied()
            .collect();

        assert!(
            unresynced.is_empty(),
            "these config-mirror setters run at startup (App::run) but apply_config never calls a setter of the same name, so a Reload/Settings-save leaves their mirror on whatever was true at the last full restart: {unresynced:?}"
        );
    }

    // -- activation trust (#236, #237) ---------------------------------------

    #[test]
    fn startup_card_shows_neither_one_or_both_problems() {
        let keys = vec!["openai".to_string()];
        assert_eq!(super::startup_card(&[], false, None), None);
        let only_keys = super::startup_card(&keys, false, None).unwrap();
        assert!(only_keys.0.contains("key"), "{only_keys:?}");
        let only_squat = super::startup_card(&[], true, None).unwrap();
        assert!(only_squat.0.contains("startup lock"), "{only_squat:?}");
        let (headline, detail) = super::startup_card(&keys, true, None).unwrap();
        assert_eq!(headline, only_keys.0);
        assert!(detail.contains(&only_keys.1), "keys text kept: {detail}");
        assert!(detail.contains(&only_squat.0), "squat text added: {detail}");
        assert!(!detail.contains('\u{2014}'));
    }

    /// #711: a config.toml that did not parse gets its own card, with the
    /// location, and it is not lost when the other startup problems apply.
    #[test]
    fn startup_card_reports_a_config_file_that_did_not_parse() {
        let problem = "config.toml has an error at line 3, column 7";
        let (headline, detail) = super::startup_card(&[], false, Some(problem)).unwrap();
        assert!(headline.contains("Settings file"), "{headline}");
        assert!(detail.contains(problem), "{detail}");
        assert!(detail.contains("Reload settings"), "{detail}");
        assert!(!detail.contains('\u{2014}'));
        let (_, both) = super::startup_card(&["openai".to_string()], true, Some(problem)).unwrap();
        assert!(
            both.contains(problem) && both.contains("startup lock"),
            "{both}"
        );
        assert!(both.contains("Stored API key unreadable"), "{both}");
        assert!(both.contains("Offline"), "{both}");
    }

    /// #237: the owner window must exist before the single-instance name is
    /// taken, or a duplicate arriving in the gap finds the name held and no
    /// window to poke. Source scan over `run`'s body, so it needs no window.
    #[test]
    fn run_creates_the_owner_window_before_taking_the_instance_name() {
        let app_src = lf(include_str!("app.rs"));
        let run_src = run_body(&app_src);
        let window = run_src.find("create_owner_window(instance)");
        let acquire = run_src.find("single_instance::acquire()");
        assert!(window.is_some(), "run no longer creates the owner window");
        assert!(acquire.is_some(), "run no longer takes the instance name");
        assert!(
            window < acquire,
            "the owner window must be created before single_instance::acquire"
        );
    }

    /// #722: the tray menu defers what Settings drops, so opening the menu
    /// never cancels a send waiting on its preview.
    #[test]
    fn tray_menu_policy_defers_send_preview_first_run_and_stage() {
        for msg in [WM_APP_SEND_PREVIEW, WM_APP_FIRST_RUN, WM_APP_PENDING_STAGE] {
            assert_eq!(super::tray_menu_policy(msg, 0), SettingsReentrancy::Defer);
            assert_eq!(
                settings_reentrancy_policy(msg, 0),
                SettingsReentrancy::Ignore
            );
        }
        // Everything else matches the Settings policy.
        for msg in [WM_APP_RESULT, WM_APP_HOTKEY, WM_APP_DISMISS, WM_APP_TRAY, 0] {
            assert_eq!(
                super::tray_menu_policy(msg, 0),
                settings_reentrancy_policy(msg, 0)
            );
        }
    }

    /// #722: the tray menu runs a nested message loop. No `&mut App` may be
    /// live across it, and the reentrancy policy must be on while it runs.
    #[test]
    fn tray_menu_loop_runs_without_an_app_borrow() {
        let src = lf(include_str!("app.rs"));
        let start = src
            .find("\nfn handle_tray_message(")
            .expect("app.rs has no fn handle_tray_message");
        let body = &src[start + 1..];
        let body = &body[..body.find("\n}\n").expect("fn has no end")];
        assert!(!body.is_empty());
        let track = body.find(".track()").expect("the menu is never tracked");
        let on = body[..track]
            .rfind("SETTINGS_OPEN.with(|c| c.set(true));")
            .expect("the reentrancy policy is not switched on before the menu loop");
        let off = track
            + body[track..]
                .find("SETTINGS_OPEN.with(|c| c.set(false));")
                .expect("the reentrancy policy is never switched off");
        let span = &body[on..off];
        assert!(
            !span.contains("app."),
            "App is used while the menu loop runs"
        );
        assert!(
            !span.contains("tray."),
            "Tray is used while the menu loop runs"
        );
        // The wnd_proc arm only delegates; it never reaches the menu itself.
        let proc_src = wnd_proc_body(&src);
        let call = proc_src
            .find("handle_tray_message(")
            .expect("wnd_proc never delegates the tray message");
        let bind = proc_src
            .find("let app: &mut App")
            .expect("wnd_proc binds no app");
        assert!(call < bind, "the tray call must precede any App borrow");
        assert!(!proc_src.contains("on_tray_message"));
        // And the tray's own message handler no longer calls TrackPopupMenu.
        let tray = lf(include_str!("ui/tray.rs"));
        let h = tray
            .find("pub fn on_tray_message(")
            .expect("no on_tray_message");
        let h = &tray[h..h + tray[h..].find("\n    }\n").expect("no end")];
        assert!(!h.contains("TrackPopupMenu"));
    }

    /// #236: only the activation arm may be rate limited, and it must be.
    #[test]
    fn only_the_activation_arm_is_rate_limited() {
        let app_src = lf(include_str!("app.rs"));
        let body = wnd_proc_body(&app_src);
        let arm = body
            .split("WM_APP_ACTIVATE => {")
            .nth(1)
            .expect("wnd_proc has no WM_APP_ACTIVATE arm");
        let arm = &arm[..arm.find("LRESULT(0)").expect("arm has no end")];
        assert!(arm.contains("check_activation_now()"), "{arm}");
        assert!(arm.contains("Trigger::Activation"), "{arm}");
        assert_eq!(
            body.matches("check_activation_now()").count(),
            1,
            "the limiter must guard activations only, never a key press"
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
        (
            "WM_APP_CARD_OPEN_SETTINGS",
            WM_APP_CARD_OPEN_SETTINGS,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_GENERIC_ACTION_RESULT",
            WM_APP_GENERIC_ACTION_RESULT,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_CARD_COPY_DETAILS",
            WM_APP_CARD_COPY_DETAILS,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_SEND_PREVIEW",
            WM_APP_SEND_PREVIEW,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_PENDING_STAGE",
            WM_APP_PENDING_STAGE,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_MACHINE_CHECK_RESULT",
            WM_APP_MACHINE_CHECK_RESULT,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_FIRST_RUN",
            WM_APP_FIRST_RUN,
            SettingsReentrancy::Ignore,
        ),
        (
            "WM_APP_EXECUTOR_DONE",
            WM_APP_EXECUTOR_DONE,
            SettingsReentrancy::Defer,
        ),
        (
            "WM_APP_OPTIONS_CHOICE",
            WM_APP_OPTIONS_CHOICE,
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

    /// #227: a panicking worker body must come back as an ordinary packed
    /// error (so the result message is still posted and `busy` clears), and a
    /// normal result must pass through untouched. Tests build with unwind.
    #[test]
    fn catch_worker_turns_a_panic_into_a_packed_error() {
        let out: std::result::Result<u32, String> =
            super::catch_worker(|| panic!("boom in worker"));
        let packed = out.expect_err("a panic must become an Err");
        let (human, chain) = super::unpack_error(&packed);
        assert!(super::is_run_fault(human), "{human}");
        assert!(!human.contains("boom in worker"), "{human}");
        assert!(chain.contains("boom in worker"), "{chain}");
    }

    #[test]
    fn catch_worker_passes_results_through() {
        assert_eq!(super::catch_worker(|| Ok::<_, String>(7)), Ok(7));
        assert_eq!(
            super::catch_worker(|| Err::<u32, _>("plain".to_string())),
            Err("plain".to_string())
        );
    }

    #[test]
    fn a_formatted_panic_keeps_the_location_and_drops_the_text() {
        let secret = "SECRET selection text";
        crate::crash_note::note_location("src/x.rs:9:9");
        let out: std::result::Result<u32, String> =
            super::catch_worker(|| panic!("bad input: {secret}"));
        let packed = out.unwrap_err();
        let (_, chain) = super::unpack_error(&packed);
        assert!(!packed.contains("SECRET"), "{packed}");
        assert!(chain.contains("src/x.rs:9:9"), "{chain}");
    }

    #[test]
    fn a_literal_panic_message_is_kept_in_the_chain() {
        let out: std::result::Result<u32, String> = super::catch_worker(|| panic!("plain literal"));
        let packed = out.unwrap_err();
        assert!(super::unpack_error(&packed).1.contains("plain literal"));
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

    // -- #425: "Copy details" clipboard text ---------------------------------

    use super::{error_details_clipboard_text, LastError};
    use std::time::SystemTime;

    #[test]
    fn error_details_clipboard_text_is_the_redacted_chain_and_never_the_raw_token() {
        // The same fake-key shape `redact_opaque_tokens`'s own tests use
        // (egress.rs's `redact_opaque_tokens_scrubs_a_long_key_shaped_token`)
        // -- proves the property this function exists for: whatever
        // `record_last_error` stored (already redacted, #253) is exactly
        // what reaches the clipboard, and the raw token never does.
        let fake_key = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE1234567890";
        let raw_chain = format!("HTTP 401: invalid api key {fake_key} supplied");
        let last_error = LastError {
            action: "Couldn't add the event".to_string(),
            occurred_at: SystemTime::now(),
            // Exactly what `App::record_last_error` does to `raw_chain`.
            chain: crate::egress::redact_opaque_tokens(&raw_chain),
        };

        let text = error_details_clipboard_text(&last_error);

        assert!(
            !text.contains(fake_key),
            "the raw token must never reach the clipboard text: {text}"
        );
        assert!(text.contains("[redacted]"), "{text}");
        assert!(text.contains("HTTP 401"), "{text}");
        assert_eq!(text, last_error.chain, "must be the SAME chain, not a copy");
    }

    #[test]
    fn error_details_clipboard_text_leaves_ordinary_prose_alone() {
        // Neighbour: a chain with nothing token-shaped in it round-trips
        // unchanged, same as `redact_opaque_tokens` itself does.
        let raw_chain = "HTTP 429: too many requests, retry in 30s";
        let last_error = LastError {
            action: "Couldn't ask".to_string(),
            occurred_at: SystemTime::now(),
            chain: crate::egress::redact_opaque_tokens(raw_chain),
        };
        assert_eq!(error_details_clipboard_text(&last_error), raw_chain);
    }

    // -- #349: human-readable, redacted error text for the card -------------

    use super::{
        contains_api_failed_identifier, find_error_interpolations, human_error_detail,
        humanize_error_chain, interpolation_is_guarded, is_api_shaped_identifier,
        worker_error_chain_text,
    };

    #[test]
    fn contains_api_failed_identifier_catches_the_timezone_chain() {
        assert!(contains_api_failed_identifier(
            "TzSpecificLocalTimeToSystemTime failed"
        ));
        assert!(contains_api_failed_identifier(
            "FileTimeToSystemTime failed"
        ));
        assert!(contains_api_failed_identifier(
            "SystemTimeToFileTime failed"
        ));
    }

    #[test]
    fn contains_api_failed_identifier_catches_a_parenthesized_call_site() {
        // The `.context("SetWindowsHookExW(WH_MOUSE_LL) failed")` shape used
        // throughout hotkey.rs/dismiss.rs.
        assert!(contains_api_failed_identifier(
            "context: SetWindowsHookExW(WH_MOUSE_LL) failed"
        ));
        assert!(contains_api_failed_identifier("GetMonitorInfoW failed"));
        assert!(contains_api_failed_identifier(
            "SHGetKnownFolderPath failed"
        ));
        assert!(contains_api_failed_identifier("CryptProtectData failed"));
    }

    #[test]
    fn contains_api_failed_identifier_leaves_ordinary_prose_alone() {
        assert!(!contains_api_failed_identifier("the save failed"));
        assert!(!contains_api_failed_identifier("write failed"));
        assert!(!contains_api_failed_identifier(
            "request failed: connection reset"
        ));
        assert!(!contains_api_failed_identifier(
            "No such file or directory (os error 2)"
        ));
    }

    #[test]
    fn is_api_shaped_identifier_requires_two_capitals_and_a_leading_one() {
        assert!(is_api_shaped_identifier("GetMonitorInfoW"));
        assert!(!is_api_shaped_identifier("Save")); // one leading capital only
        assert!(!is_api_shaped_identifier("save"));
        assert!(!is_api_shaped_identifier("abc")); // too short
    }

    #[test]
    fn humanize_error_chain_maps_the_known_timezone_fragments() {
        for chain in [
            "couldn't read the local date: TzSpecificLocalTimeToSystemTime failed",
            "couldn't read the local date: FileTimeToSystemTime failed",
            "couldn't read the local date: SystemTimeToFileTime failed",
        ] {
            let human = humanize_error_chain(chain);
            assert_eq!(human, "Couldn't read your clock settings.");
            assert!(!contains_api_failed_identifier(&human));
        }
    }

    /// #261/#264: a UIA call bounded by
    /// `inputs::uia_automation::create_automation`'s timeouts, once it
    /// actually trips, must show a readable card, not a raw HRESULT.
    #[test]
    fn humanize_error_chain_maps_a_uia_timeout() {
        let chain = format!(
            "couldn't read the focused element: {} (GetFocusedElement)",
            crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT
        );
        let human = humanize_error_chain(&chain);
        assert_eq!(human, "The other app stopped responding.");
        assert!(!contains_api_failed_identifier(&human));
    }

    /// Code review follow-up on #261/#264: a bare UIA call this crate never
    /// wrapped with `describe_timeout` (e.g. `GetSelection`/`GetText`) can
    /// still surface `UIA_E_TIMEOUT` deep in an anyhow context chain.
    /// `worker_error_chain_text` must find it by downcasting, not by
    /// looking for a fixed string, so it works regardless of how many
    /// `.context(...)` layers sit on top.
    #[test]
    fn worker_error_chain_text_finds_a_uia_timeout_with_no_context() {
        let win_err = windows::core::Error::from_hresult(windows::core::HRESULT(
            crate::inputs::uia_automation::UIA_E_TIMEOUT_HRESULT,
        ));
        let err: anyhow::Error = win_err.into();
        let text = worker_error_chain_text(&err);
        assert_eq!(
            text,
            crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT
        );
    }

    #[test]
    fn worker_error_chain_text_finds_a_uia_timeout_under_several_context_layers() {
        let win_err = windows::core::Error::from_hresult(windows::core::HRESULT(
            crate::inputs::uia_automation::UIA_E_TIMEOUT_HRESULT,
        ));
        let err: anyhow::Error = anyhow::Error::new(win_err)
            .context("reading the selection")
            .context("capturing input");
        let text = worker_error_chain_text(&err);
        assert_eq!(
            text,
            crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT
        );
    }

    #[test]
    fn worker_error_chain_text_passes_through_a_non_timeout_error() {
        let win_err = windows::core::Error::from_hresult(windows::core::HRESULT(
            windows::Win32::Foundation::E_FAIL.0,
        ));
        let err: anyhow::Error = anyhow::Error::new(win_err).context("doing something else");
        let text = worker_error_chain_text(&err);
        assert!(!text.contains(crate::inputs::uia_automation::TIMEOUT_MESSAGE_FRAGMENT));
        assert!(text.contains("doing something else"));
    }

    #[test]
    fn humanize_error_chain_falls_back_to_a_generic_sentence_for_an_unmapped_api_failure() {
        let human = humanize_error_chain("SetWindowsHookExW(WH_MOUSE_LL) failed");
        assert_eq!(human, "Something went wrong.");
    }

    #[test]
    fn humanize_error_chain_keeps_prose_that_is_already_human() {
        let human = humanize_error_chain("No such file or directory (os error 2)");
        assert_eq!(human, "No such file or directory (os error 2)");
    }

    #[test]
    fn humanize_error_chain_redacts_a_key_shaped_token_issue_253() {
        let fake_key = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE1234567890";
        let human = humanize_error_chain(&format!("HTTP 401: invalid api key {fake_key}"));
        assert!(!human.contains(fake_key), "{human}");
    }

    // -- review of #349/#426, item 2: actionable provider errors must stay
    // specific, never collapse to the generic "Something went wrong." --
    //
    // `contains_api_failed_identifier` only replaces text that looks
    // Win32/API-shaped ("XxxYyy(...) failed"); every provider error text
    // below mirrors the exact `format!`/`anyhow!` template
    // `src/provider/common.rs` actually uses (`http_error`,
    // `rate_limited_error`, the `NoResponse` transport-error arm), run
    // through the same `egress::redact_opaque_tokens` those call sites
    // already apply (#253) before it ever reaches `app.rs`, so these are
    // the real strings a card would show, not synthetic ones.

    #[test]
    fn humanize_error_chain_keeps_a_401_bad_key_specific() {
        // Real Anthropic 401 body shape (anthropic.rs/common.rs's
        // `http_error`: `"{tag}: HTTP {status}: {truncated}"`).
        let body = r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#;
        let chain = format!(
            "anthropic: HTTP 401: {}",
            crate::egress::redact_opaque_tokens(body)
        );
        assert!(!contains_api_failed_identifier(&chain));
        let human = humanize_error_chain(&chain);
        assert!(human.contains("HTTP 401"), "{human}");
        assert!(human.contains("invalid x-api-key"), "{human}");
        assert_ne!(human, "Something went wrong.");
    }

    #[test]
    fn humanize_error_chain_keeps_a_429_rate_limit_specific() {
        // Real shape from `rate_limited_error`: `"{tag}: too many requests.
        // Retry after {} seconds."`.
        let chain = "openai: too many requests. Retry after 20 seconds.".to_string();
        assert!(!contains_api_failed_identifier(&chain));
        let human = humanize_error_chain(&chain);
        assert!(human.contains("too many requests"), "{human}");
        assert!(human.contains("Retry after 20 seconds"), "{human}");
        assert_ne!(human, "Something went wrong.");
    }

    #[test]
    fn humanize_error_chain_keeps_a_model_not_found_specific() {
        // Real OpenAI 404 body shape, same `http_error` template as the 401
        // case above.
        let body = r#"{"error":{"message":"The model `gpt-9` does not exist","type":"invalid_request_error","code":"model_not_found"}}"#;
        let chain = format!(
            "openai: HTTP 404: {}",
            crate::egress::redact_opaque_tokens(body)
        );
        assert!(!contains_api_failed_identifier(&chain));
        let human = humanize_error_chain(&chain);
        assert!(human.contains("HTTP 404"), "{human}");
        assert!(human.contains("does not exist"), "{human}");
        assert_ne!(human, "Something went wrong.");
    }

    #[test]
    fn humanize_error_chain_keeps_ollama_not_running_specific() {
        // Real shape from the `TransportError::NoResponse` arm:
        // `"{tag}: transport error: {}"`, `redact_detail`-ed -- what a real
        // `ureq` connection-refused error (Ollama's tray app not running,
        // #253's own "stock Ollama tray steals the port" pitfall aside)
        // looks like.
        let transport_err = "connect error: Connection refused (os error 10061)";
        let chain = format!(
            "ollama: transport error: {}",
            crate::egress::redact_opaque_tokens(transport_err)
        );
        assert!(!contains_api_failed_identifier(&chain));
        let human = humanize_error_chain(&chain);
        assert!(human.contains("Connection refused"), "{human}");
        assert_ne!(human, "Something went wrong.");
    }

    #[test]
    fn humanize_error_chain_keeps_a_network_timeout_specific() {
        // Same `"{tag}: transport error: {}"` template, a timeout instead
        // of a refused connection.
        let transport_err = "operation timed out after 30s";
        let chain = format!(
            "openai: transport error: {}",
            crate::egress::redact_opaque_tokens(transport_err)
        );
        assert!(!contains_api_failed_identifier(&chain));
        let human = humanize_error_chain(&chain);
        assert!(human.contains("timed out"), "{human}");
        assert_ne!(human, "Something went wrong.");
    }

    /// #103's wiring guard: every screenshot `app.rs` encodes goes through
    /// `encode_for_upload` (which redacts), never `capture::encode`, which is
    /// private to `capture` anyway. This scan keeps a new call site from
    /// being added to the wrong helper, and fails if the scan itself stops
    /// finding anything (CRLF-normalised, non-empty candidate set).
    #[test]
    fn every_screenshot_encode_in_app_goes_through_the_redacting_helper() {
        let src = include_str!("app.rs").replace('\r', "");
        // Needles are built from pieces so this test's own text never
        // matches them.
        let upload = ["encode_for", "_upload(&mut raw"].concat();
        let router_upload = ["encode_for", "_upload(raw,"].concat();
        // The generic worker's screenshot is optional (#127), so it reaches
        // the helper through a borrowed `screen`.
        let optional_upload = ["encode_for", "_upload(screen,"].concat();
        let plain = ["capture", "::encode("].concat();
        let encode_calls = src.matches(&upload).count()
            + src.matches(&router_upload).count()
            + src.matches(&optional_upload).count();
        assert!(
            encode_calls >= 6,
            "expected the six worker call sites, found {encode_calls}"
        );
        assert_eq!(
            src.matches(&plain).count(),
            0,
            "the plain encode skips redaction; use the redacting helper"
        );
    }

    /// #103: the fail-closed card keeps its fixed sentence whatever the OCR
    /// cause looks like, including an API-shaped one that would otherwise
    /// become "Something went wrong."
    #[test]
    fn a_failed_screenshot_check_always_shows_the_fixed_sentence() {
        for cause in [
            "RoInitialize failed",
            "OCR recognition timed out after 5s",
            "no OCR-capable language is installed",
        ] {
            let err = anyhow::anyhow!(cause).context(crate::redact::FAILURE_NEEDLE);
            let chain = worker_error_chain_text(&err);
            assert_eq!(
                humanize_error_chain(&chain),
                crate::redact::FAILURE_SENTENCE,
                "{cause}"
            );
        }
        assert_eq!(
            crate::redact::FAILURE_SENTENCE,
            "Could not check the screenshot for private details, so nothing was sent."
        );
    }

    /// #506: an app running as administrator gets its own plain sentence, not
    /// the generic one and not "Something went wrong."
    #[test]
    fn an_administrator_app_shows_its_own_sentence() {
        let err = anyhow::anyhow!("access denied").context(crate::redact::ADMIN_APP_SENTENCE);
        let chain = worker_error_chain_text(&err);
        assert_eq!(
            humanize_error_chain(&chain),
            crate::redact::ADMIN_APP_SENTENCE
        );
        assert_eq!(
            crate::redact::ADMIN_APP_SENTENCE,
            "Wingman can't check this app for password fields (it may be running as administrator), so nothing was sent."
        );
    }

    /// #327: only a real fault raises the tray's red dot. A user-state
    /// message ("Nothing selected") and a plain unpacked string do not.
    #[test]
    fn only_real_faults_count_as_run_failures() {
        use super::{
            is_run_fault, pack_generic_failure, unpack_error, GenericFailure, ERROR_PACK_SEP,
        };

        assert!(is_run_fault(&human_error_detail("connection refused")));

        let fault = pack_generic_failure(GenericFailure::Fault(anyhow::anyhow!("boom")));
        assert!(is_run_fault(unpack_error(&fault).0));

        let user = pack_generic_failure(GenericFailure::UserState("Nothing is selected".into()));
        assert!(!is_run_fault(unpack_error(&user).0));

        // Calculate's "nothing selected" is sent as a plain, unpacked string.
        let plain = "Nothing selected. Select an expression first.";
        assert!(!is_run_fault(unpack_error(plain).0));
        assert!(!plain.contains(ERROR_PACK_SEP));
    }

    #[test]
    fn human_error_detail_appends_a_next_step() {
        let detail = human_error_detail("TzSpecificLocalTimeToSystemTime failed");
        assert_eq!(
            detail,
            "Couldn't read your clock settings. Try again, and if it keeps happening, use Copy diagnostics for details."
        );
    }

    /// #493: a user-state failure ("nothing is selected") is a plain
    /// message; only a real fault gets the "use Copy diagnostics" step.
    #[test]
    fn generic_user_state_failure_has_no_diagnostics_suffix_but_a_fault_does() {
        use super::{pack_generic_failure, plan_generic_inputs, unpack_error, GenericFailure};
        use crate::actions::generic_inputs::{Gathered, PASSWORD_FIELD_MESSAGE};
        use crate::actions::InputKind;

        // The real refusals, routed the way the worker routes them.
        let empty = Gathered::default();
        let password = Gathered {
            selection_in_password_field: true,
            ..Gathered::default()
        };
        let cases: [(&[InputKind], &Gathered, Option<&str>); 4] = [
            (&[InputKind::Selection], &empty, Some("Nothing is selected")),
            (
                &[InputKind::Clipboard],
                &empty,
                Some("clipboard has no text"),
            ),
            (&[InputKind::Uia], &empty, Some("any text from the window")),
            (&[InputKind::Selection, InputKind::Screen], &password, None),
        ];
        for (inputs, gathered, expect) in cases {
            let failure = plan_generic_inputs(inputs, gathered).unwrap_err();
            let GenericFailure::UserState(message) = &failure else {
                panic!("a refused plan must be a UserState failure: {inputs:?}");
            };
            match expect {
                Some(fragment) => assert!(message.contains(fragment), "{message}"),
                None => assert_eq!(message, PASSWORD_FIELD_MESSAGE),
            }
            let message = message.clone();
            let packed = pack_generic_failure(failure);
            let (human, chain) = unpack_error(&packed);
            assert_eq!(human, message);
            assert_eq!(chain, message);
            assert!(!human.contains("Copy diagnostics"), "{human}");
        }

        let fault = pack_generic_failure(GenericFailure::Fault(anyhow::anyhow!(
            "couldn't put your clipboard back after reading the selection"
        )));
        let (human, _) = unpack_error(&fault);
        assert!(human.starts_with("couldn't put your clipboard back"));
        assert!(human.contains("Copy diagnostics"));
    }

    /// #349's "Done when": no card's visible text may contain a Win32/API
    /// function name. Rather than exercising every `show_error` call site
    /// live (most need a real worker thread, a real window, or real I/O),
    /// this scans `app.rs`'s own source (normalized to `\n` line endings,
    /// so a CRLF checkout never changes what this test sees) for every
    /// shape `find_error_interpolations` recognizes -- interpolating a
    /// bound error value's Display/Debug output, or calling its
    /// `.to_string()`, directly -- and fails if one is found whose
    /// enclosing statement does not mention `human_error_detail` or
    /// `track_error`. This is the mechanical guard that a new `show_error`
    /// call site cannot reintroduce the bug this issue closes. See
    /// `find_error_interpolations_*` below for proof the scanner itself
    /// has teeth (review of #349/#426: the original version of this test
    /// only matched two exact literals).
    #[test]
    fn show_error_call_sites_never_show_a_failed_suffixed_api_identifier() {
        let src = include_str!("app.rs").replace("\r\n", "\n");
        for pos in find_error_interpolations(&src) {
            let window_start = pos.saturating_sub(80);
            let window_end = (pos + 40).min(src.len());
            assert!(
                interpolation_is_guarded(&src, pos),
                "found a raw error interpolation at byte {pos} not wrapped by \
                 human_error_detail/track_error -- it could show an API function \
                 name on a card:\n{}",
                &src[window_start..window_end]
            );
        }
    }

    // -- the scanner's own regression tests (review of #349/#426) -----------
    //
    // A source-scanning test is only as good as its patterns: the version
    // above only matched two exact `format!` literals (the alternate- and
    // plain-Display specifiers on the bound name `e`), so it could never
    // have caught the Debug specifier, an `err`/`error` bound name, a bare
    // `.to_string()`, or a whitespace variant -- and, being green either
    // way, gave no signal that it was blind to them.
    // These tests plant exactly those shapes as Rust *string* data (never
    // compiled, so they can safely look like the bug without being it) and
    // prove the scanner actually flags each one, plus that a properly
    // guarded call is correctly left alone.

    #[test]
    fn find_error_interpolations_catches_a_planted_bad_line() {
        let planted = "fn f(e: anyhow::Error) -> String { format!(\"{e:#}\") }";
        let hits = find_error_interpolations(planted);
        assert_eq!(hits.len(), 1, "{hits:?}");
        assert!(!interpolation_is_guarded(planted, hits[0]));
    }

    #[test]
    fn find_error_interpolations_recognizes_debug_other_bindings_and_to_string() {
        for planted in [
            "format!(\"{e:?}\")",
            "format!(\"{err}\")",
            "format!(\"{err:#}\")",
            "format!(\"{error:?}\")",
        ] {
            let hits = find_error_interpolations(planted);
            assert_eq!(hits.len(), 1, "did not catch: {planted}");
        }
        // Built from parts, not as a literal string constant spelling out
        // the bound name immediately followed by a call to its own
        // `ToString`: that would itself be exactly the bare pattern this
        // scanner looks for, and this file's own source is one of the
        // things `show_error_call_sites_never_show_a_failed_suffixed_api_identifier`
        // scans.
        for name in ["e", "err", "error"] {
            let planted = format!("{name}{}to_string()", '.');
            let hits = find_error_interpolations(&planted);
            assert_eq!(hits.len(), 1, "did not catch: {planted}");
        }
    }

    #[test]
    fn find_error_interpolations_tolerates_whitespace_variants() {
        for planted in [
            "format! (\"{e:#}\")",
            "format!( \"{e:#}\" )",
            "format!(\"{e:#}\" )",
        ] {
            let hits = find_error_interpolations(planted);
            assert_eq!(hits.len(), 1, "did not catch: {planted:?}");
        }
    }

    #[test]
    fn find_error_interpolations_normalizes_crlf() {
        let planted = "fn f() {\r\n    format!(\"{e:#}\")\r\n}".replace("\r\n", "\n");
        let hits = find_error_interpolations(&planted);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn find_error_interpolations_ignores_a_longer_identifier_ending_in_e() {
        // A longer identifier merely ending in the letter e (like "value")
        // must not be mistaken for the bare-`e` binding name.
        let hits = find_error_interpolations("let s = value.to_string();");
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn find_error_interpolations_ignores_an_unrelated_format_call() {
        let hits = find_error_interpolations("format!(\"{name}\")");
        assert!(hits.is_empty(), "{hits:?}");
    }

    #[test]
    fn interpolation_is_guarded_accepts_human_error_detail_and_track_error() {
        for guarded in [
            "self.card.show_error(\"x\", &human_error_detail(&format!(\"{e:#}\")));",
            "let d = self.track_error(\"x\", &format!(\"{e:#}\"));",
        ] {
            let hits = find_error_interpolations(guarded);
            assert_eq!(hits.len(), 1, "{guarded}");
            assert!(interpolation_is_guarded(guarded, hits[0]), "{guarded}");
        }
    }

    #[test]
    fn interpolation_is_guarded_rejects_a_bare_call() {
        let bare = "self.card.show_error(\"x\", &format!(\"{e:#}\"));";
        let hits = find_error_interpolations(bare);
        assert_eq!(hits.len(), 1);
        assert!(!interpolation_is_guarded(bare, hits[0]));
    }

    /// Regression test for exactly the bug this scanner shipped with once:
    /// `pack_error`'s real call site wraps two separate error-formatting
    /// expressions -- one inside `human_error_detail`, one inside
    /// `egress::redact_opaque_tokens` -- in a single statement. A naive
    /// "nearest preceding brace" boundary search finds the first
    /// expression's own literal closing brace (part of its Display
    /// placeholder text, not a real block boundary) and cuts
    /// `human_error_detail(` out of what it searches for the second
    /// expression -- wrongly reporting it unguarded.
    #[test]
    fn interpolation_is_guarded_sees_past_an_earlier_format_placeholder() {
        let guarded = "pack_error(&human_error_detail(&format!(\"{e:#}\")), \
                        &egress::redact_opaque_tokens(&format!(\"{e:#}\")));";
        let hits = find_error_interpolations(guarded);
        assert_eq!(hits.len(), 2, "{hits:?}");
        for pos in hits {
            assert!(
                interpolation_is_guarded(guarded, pos),
                "byte {pos} in: {guarded}"
            );
        }
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

    // -- palette footer label agrees with describe (issue #243) --------------

    #[test]
    fn first_provider_model_label_matches_describe_for_the_first_order_entry() {
        // #243: first_provider_model_label used to keep its own fourth
        // hand-mirrored match over the same five provider-name arms
        // (alongside provider_for_named_model/models_for/describe in
        // config.rs, #222 and #244). A recognized name whose describe()
        // model this label disagreed with would silently show the bare
        // name in the palette footer forever, with nothing to say why.
        let mut providers = Providers::default();
        providers.openai.model = "gpt-5.5-pro".to_string();
        providers.compat.push(crate::config::CompatConfig {
            name: "custom".to_string(),
            model: "compat-flagship".to_string(),
            ..Default::default()
        });
        for name in [
            "openai",
            "anthropic",
            "gemini",
            "ollama",
            "compat:custom",
            "not-a-real-provider",
        ] {
            providers.order = vec![name.to_string()];
            let label = first_provider_model_label(Mode::Auto, &providers);
            let expected = match providers.describe(name) {
                Some(d) if !d.model.is_empty() => Some(format!("{name}:{}", d.model)),
                _ => Some(name.to_string()),
            };
            assert_eq!(
                label, expected,
                "label disagrees with describe for {name:?}"
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
    /// `qwen3.5:4b` (or `WINGMAN_ROUTER_LIVE_MODEL`). Run manually (AGENTS.md build rules -- never bare
    /// `cargo test`):
    /// ```text
    /// CARGO_TARGET_DIR=... RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=2 \
    ///   cargo test router_live -- --ignored --nocapture
    /// ```
    /// Requires a local Ollama server on 127.0.0.1:11434 with the model
    /// pulled. Expects `intent == Some("review-this-email")` when that action
    /// exists in `actions::load_actions()`'s catalogue (added by a sibling
    /// agent in parallel, per #24's task description); otherwise only
    /// requires SOME valid id was named, never "none" -- either way the
    /// chosen id is printed. Unloads the model afterward (`keep_alive:
    /// "0"`), the same convention `provider/mod.rs`'s and `ollama.rs`'s own
    /// live checks already use.
    ///
    /// MEASURED 2026-09-17 (this crate's dev profile, `gemma3:4b` then, catalogue
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
    /// MEASURED 2026-10-16 (#223, `qwen3.5:4b`, real catalogue with
    /// `review-this-email`, revised prompt): 5 of 5 runs picked
    /// `review-this-email` with a prose summary and no echoed id; see
    /// `router::ROUTER_SYSTEM_PREFIX`.
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
        // `App::show_options_card` sends in production, so a passing
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
            screen: None,
        };
        let shot = capture::encode_unredacted(&raw).expect("encode synthetic PNG");

        let resolved = actions::load_actions()
            .expect("load_actions should succeed against whatever actions.toml (or none) is on this machine");
        let catalogue = palette_model::catalogue(&resolved);
        let candidates = router::candidates_from_catalogue(&catalogue);
        let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();

        let req = router::build_request(shot.png, &candidates);

        // The expansion plan's router default, overridable to compare models
        // (`WINGMAN_ROUTER_LIVE_MODEL=gemma3:4b`).
        let model =
            std::env::var("WINGMAN_ROUTER_LIVE_MODEL").unwrap_or_else(|_| "qwen3.5:4b".to_string());
        let provider = Ollama::new(DEFAULT_BASE_URL, &model, "low");
        let started = std::time::Instant::now();
        let completion = provider
            .complete(&req)
            .expect("live ollama router request should succeed");
        let elapsed = started.elapsed();

        // GPU oracle (AGENTS.md rule 6): size_vram > 0 on /api/ps, read while
        // the model is still loaded.
        let gpu = crate::provider::ollama_admin::ps(DEFAULT_BASE_URL)
            .map(|e| {
                format!(
                    "{:?}",
                    crate::provider::ollama_admin::gpu_status_for(&e, &model)
                )
            })
            .unwrap_or_else(|e| format!("unknown ({e})"));

        // Unload right after the measurement (mirrors `provider/mod.rs`'s /
        // `ollama.rs`'s own live-check convention) -- a trivial follow-up
        // request purely to release VRAM; its own result is not the
        // measurement, so a failure here is not fatal to the test.
        let mut unload_provider = Ollama::new(DEFAULT_BASE_URL, &model, "low");
        unload_provider.keep_alive = "0".to_string();
        let _ = unload_provider.complete(&router::build_request(vec![], &[]));

        let result = router::parse_router_result(
            &completion.text,
            &candidates,
            router::RouterLimits::default(),
        )
        .expect("router completion should parse against its own schema");

        eprintln!(
            "router_live: model={model} gpu={gpu} elapsed={elapsed:?} \
             summary={:?} options={:?}",
            result.summary, result.options
        );
        if let Some(usage) = completion.usage {
            eprintln!(
                "router_live token usage: input={} output={}",
                usage.input_tokens, usage.output_tokens
            );
        }

        if candidate_ids.iter().any(|id| id == "review-this-email") {
            assert_eq!(
                result.options.first().map(|o| o.intent.as_str()),
                Some("review-this-email"),
                "the \"review-this-email\" action exists in this catalogue; expected the router \
                 to pick it for an email compose window"
            );
        } else {
            assert!(
                !result.options.is_empty(),
                "expected the router to name some action id for an unambiguous email compose \
                 window, got none (\"review-this-email\" is not in this catalogue yet)"
            );
        }
    }

    use crate::app::{fallback_footer_for, noting_attribution, with_fallback_footer};
    // -- #228: the fallback footer on cards -----------------------------

    fn attribution(answered: &str, first: Option<&str>) -> crate::provider::Attribution {
        crate::provider::Attribution {
            answered: answered.to_string(),
            first_choice: first.map(str::to_string),
            cost_usd: None,
        }
    }

    #[test]
    fn footer_shows_the_cost_of_a_billed_first_choice_answer() {
        let mut a = attribution("openai:gpt-5.5", None);
        a.cost_usd = Some(0.002);
        assert_eq!(
            fallback_footer_for(Some(&a)).as_deref(),
            Some("About $0.002")
        );
    }

    #[test]
    fn footer_joins_the_fallback_note_and_the_cost_into_one_line() {
        let mut a = attribution("anthropic:claude-opus-5", Some("openai:gpt-5.5"));
        a.cost_usd = Some(0.002);
        assert_eq!(
            fallback_footer_for(Some(&a)).as_deref(),
            Some("Answered by Claude Opus 5 (GPT-5.5 was unavailable). About $0.002")
        );
    }

    #[test]
    fn footer_has_no_cost_for_a_free_or_unpriced_answer() {
        let mut a = attribution("ollama:gemma3:4b", None);
        a.cost_usd = Some(0.0);
        assert_eq!(fallback_footer_for(Some(&a)), None);
        a.cost_usd = None;
        assert_eq!(fallback_footer_for(Some(&a)), None);
    }

    /// The attribution is one global slot shared by every run, so the tests
    /// that write or take it must not interleave.
    static ATTRIBUTION_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn a_new_run_does_not_inherit_a_dropped_runs_cost() {
        let _g = ATTRIBUTION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        // Run A answered and its result was dropped before anyone took it.
        let mut a = attribution("openai:gpt-5.5", None);
        a.cost_usd = Some(0.002);
        crate::provider::record_attribution(Some(a));
        // Run B starts (the tray goes busy) and fails before any model call.
        crate::app::forget_last_attribution();
        let taken = crate::provider::take_attribution();
        assert_eq!(fallback_footer_for(taken.as_ref()), None);
    }

    #[test]
    fn going_busy_clears_the_last_attribution() {
        let src = include_str!("app.rs").replace('\r', "");
        let start = src.find("fn set_busy(").expect("set_busy exists");
        let body = &src[start..];
        let body = &body[..body.find("\n    }\n").expect("end of set_busy")];
        assert!(body.contains("forget_last_attribution()"), "{body}");
    }

    #[test]
    fn footer_is_absent_when_the_first_choice_answered() {
        let a = attribution("openai:gpt-5.5", None);
        assert_eq!(fallback_footer_for(Some(&a)), None);
        assert_eq!(fallback_footer_for(None), None);
    }

    #[test]
    fn footer_names_the_answering_model_on_a_fallback() {
        let a = attribution("ollama:gemma3:4b", Some("openai:gpt-5.5"));
        assert_eq!(
            fallback_footer_for(Some(&a)).as_deref(),
            Some("Answered by gemma3:4b (GPT-5.5 was unavailable)")
        );
    }

    #[test]
    fn card_text_carries_the_footer_only_on_a_fallback() {
        let footer = "Answered by gemma3:4b (GPT-5.5 was unavailable)";
        assert_eq!(with_fallback_footer("working", None), "working");
        assert_eq!(
            with_fallback_footer("working", Some(footer)),
            format!("working\n\n{footer}")
        );
        assert_eq!(with_fallback_footer("", Some(footer)), footer);
    }

    #[test]
    fn notices_ride_after_the_detail_one_paragraph_each() {
        let n = || vec!["Store failed.".to_string(), "Budget hit.".to_string()];
        assert_eq!(
            append_notices("answer", n()),
            "answer\n\nStore failed.\n\nBudget hit."
        );
        assert_eq!(append_notices("", n()), "Store failed.\n\nBudget hit.");
        assert_eq!(append_notices("answer", Vec::new()), "answer");
        assert_eq!(append_notices("", Vec::new()), "");
    }

    /// #620: the skipped-actions notice is one of the sources
    /// `with_pending_notices` drains, and `begin_action` queues it.
    #[test]
    fn the_skipped_actions_notice_rides_on_the_pending_notices() {
        let src = include_str!("app.rs").replace('\r', "");
        let start = src.find("fn with_pending_notices(").expect("fn exists");
        let body = &src[start..start + 400];
        assert!(body.contains("take_skipped_notice()"));
        let begin = src.find("fn begin_action<T>").expect("fn exists");
        assert!(src[begin..begin + 1500].contains("queue_skipped_from_disk()"));
    }

    /// #806: the remote-Ollama notice is checked in `begin_action`, before the
    /// screen is captured, so the screenshot is never grabbed for a press the
    /// notice replaces. Without that call the module would be wired to nothing.
    #[test]
    fn begin_action_shows_the_remote_ollama_notice_before_capture() {
        let src = include_str!("app.rs").replace('\r', "");
        let begin = src.find("fn begin_action<T>").expect("fn exists");
        let rest = &src[begin..];
        let end = rest.find("\n    fn ").expect("a later fn");
        let body = &rest[..end];
        let notice = body
            .find("show_remote_ollama_notice()")
            .expect("begin_action must call the notice");
        let capture = body.find("capture::grab_raw_for").expect("capture call");
        assert!(notice < capture, "the notice must come before the capture");
        assert!(src.contains("fn show_remote_ollama_notice(&mut self)"));
    }

    /// #918: the intent router builds its own chain and never passes through
    /// `begin_action`, so it must ask the same remote-Ollama question itself,
    /// before it captures or sends anything.
    #[test]
    fn the_router_shows_the_remote_ollama_notice_before_capture() {
        let src = include_str!("app.rs").replace('\r', "");
        let begin = src
            .find("fn show_options_card(")
            .expect("show_options_card exists");
        let rest = &src[begin..];
        let end = rest.find("\n    fn ").expect("a later fn");
        let body = &rest[..end];
        let notice = body
            .find("show_remote_ollama_notice()")
            .expect("the router must call the notice");
        let capture = body.find("capture::grab_raw_for").expect("capture call");
        // The worker that sends is started by a later fn, after the capture.
        assert!(notice < capture, "the notice must come before the capture");
    }

    /// #541: every handler that ends a run on a result card must consult the
    /// pending notices through `with_pending_notices`, so a new path cannot
    /// quietly forget them.
    #[test]
    fn every_result_card_path_consults_the_pending_notices() {
        let src = include_str!("app.rs").replace('\r', "");
        let paths = [
            "on_result",
            "on_calendar_result",
            "on_generic_action_result",
            "on_review_result",
            "on_form_fill_result",
            "on_exec_done",
        ];
        for name in paths {
            let start = src
                .find(&format!("    fn {name}("))
                .unwrap_or_else(|| panic!("{name} not found"));
            let rest = &src[start + 1..];
            let end = rest.find("\n    fn ").unwrap_or(rest.len());
            assert!(
                rest[..end].contains("with_pending_notices("),
                "{name} shows a result card without with_pending_notices"
            );
        }
    }
    /// #893: a setup card is persistent, so showing one must also arm the
    /// click-away hook. That happens in `show_setup_card`, the only place
    /// outside startup (the watcher is not installed yet there) that calls
    /// `Card::show_settings_needed`; `reload` and `edit_settings` arm their own
    /// persistent cards.
    #[test]
    fn setup_cards_are_shown_through_the_one_helper_that_arms_click_away() {
        let src = include_str!("app.rs").replace('\r', "");
        // Split on the real test module: a bare "#[cfg(test)]" also appears in
        // doc comments and on test-only items well before it.
        let production = src
            .split("#[cfg(test)]\nmod tests {")
            .next()
            .expect("split yields a first part");
        assert!(
            production.contains("fn show_setup_card("),
            "production not scanned"
        );
        assert!(production.contains("fn wnd_proc("), "production cut short");
        let body_of = |name: &str| {
            let start = production
                .find(&format!("    fn {name}("))
                .unwrap_or_else(|| panic!("{name} not found"));
            let rest = &production[start + 1..];
            &rest[..rest.find("\n    fn ").unwrap_or(rest.len())]
        };
        let direct = production.matches("card.show_settings_needed(").count();
        // The helper itself and the startup welcome card.
        assert_eq!(direct, 2, "a setup card bypasses show_setup_card");
        assert!(body_of("show_setup_card").contains("set_watch(true)"));
        assert!(body_of("reload").contains("set_watch(true)"));
        // Every early return in edit_settings that follows a card arms it.
        let edit = body_of("edit_settings");
        let returns: Vec<usize> = edit.match_indices("return;").map(|(i, _)| i).collect();
        assert!(returns.len() >= 2, "edit_settings candidates not found");
        for at in returns {
            let before = &edit[at.saturating_sub(120)..at];
            if before.contains(".show_error") {
                assert!(
                    before.contains("set_watch(true)"),
                    "edit_settings returns after a card without arming click-away"
                );
            }
        }
    }

    #[test]
    fn a_worker_result_hands_its_attribution_to_the_main_thread_once() {
        let _g = ATTRIBUTION_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let _ = crate::provider::take_attribution();
        let value = noting_attribution(Ok((
            7,
            attribution("ollama:gemma3:4b", Some("openai:gpt-5.5")),
        )))
        .unwrap();
        assert_eq!(value, 7);
        let taken = crate::provider::take_attribution();
        assert!(fallback_footer_for(taken.as_ref()).is_some());
        // A run that never reaches a model finds nothing left over.
        assert!(crate::provider::take_attribution().is_none());
    }
}
