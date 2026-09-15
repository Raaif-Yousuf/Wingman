//! Orchestration: the hidden owner window, the single message loop, and the
//! state machine that ties hotkeys, capture, providers and the card together.
//!
//! This module is wiring only — every piece of real logic lives in the module
//! that owns it. The one rule that matters here: the main thread owns every
//! `HWND` and runs the only message loop. Work that can block (capture is
//! quick, the API call is not) happens on a worker thread, which reports back
//! exclusively by `PostMessageW`.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetWindowLongPtrW, PostQuitMessage, RegisterClassExW, SetWindowLongPtrW, TranslateMessage,
    CW_USEDEFAULT, GWLP_USERDATA, MSG, SW_SHOWNORMAL, WINDOW_EX_STYLE, WM_APP, WM_DESTROY,
    WM_NCCREATE, WNDCLASSEXW, WS_OVERLAPPED,
};

use crate::capture;
use crate::config::Config;
use crate::dismiss::{unpack_point, ClickWatcher, WM_APP_DISMISS};
use crate::hotkey::{
    chord_to_string, Chord, HotkeyHook, HK_PRIMARY, HK_SECONDARY, WM_APP_HOTKEY, WM_APP_LEARNED,
};
use crate::provider::{Answer, Chain, Shot};
use crate::ui::card::Card;
use crate::ui::tray::{cmd, decode, MenuChoice, Tray, WM_APP_TRAY};

/// Posted by the worker when a request finishes. `lparam` is
/// `Box::into_raw(Box::new(Result<Answer, String>))`; the handler takes
/// ownership and must reconstruct the `Box` to free it.
pub const WM_APP_RESULT: u32 = WM_APP + 3;

const WINDOW_CLASS: PCWSTR = w!("CopilotAsk.Owner.Window.4d1b62f0");

/// How long to wait after hiding a visible card before capturing, so the
/// compositor has actually taken it off the screen. Without this the old card
/// can end up inside the screenshot we send to the model.
const CARD_SETTLE_MS: u64 = 60;

struct App {
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
}

pub fn run() -> Result<()> {
    // Before any window exists, so the card's metrics are right on a mixed-DPI
    // setup (the XPS panel next to an external monitor).
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    let instance: HINSTANCE = unsafe { GetModuleHandleW(None)?.into() };
    let config = Config::load().unwrap_or_default();
    let chain = Arc::new(config.build_chain());

    let hwnd = create_owner_window(instance)?;

    let mut card = Card::new(instance).context("creating the notification card")?;
    card.set_text_scale(config.ui.text_scale);
    let tray = Tray::new(hwnd, instance).context("creating the tray icon")?;

    let mut app = Box::new(App {
        config,
        chain,
        card,
        tray,
        hook: None,
        watcher: None,
        busy: false,
        last: None,
    });
    app.refresh_tray_labels();

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
            w!("copilot-ask"),
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
    /// The whole flow: hide any stale card, grab the screen, then hand the
    /// bytes to a worker so the message loop stays responsive during the call.
    fn ask(&mut self) {
        if self.busy {
            return;
        }

        if !matches!(self.card.state(), crate::ui::card::CardState::Hidden) {
            self.card.hide();
            std::thread::sleep(Duration::from_millis(CARD_SETTLE_MS));
        }

        // Capture runs here, on the main thread, and must happen before the
        // pending card is shown — otherwise the card is in its own screenshot.
        let shot = match capture::grab(&self.config.capture.monitor, self.config.capture.max_edge) {
            Ok(s) => s,
            Err(e) => {
                self.card
                    .show_error("Couldn't capture the screen", &format!("{e:#}"));
                return;
            }
        };

        if self.chain.ready_provider_names().is_empty() {
            let path = Config::path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "config.toml".into());
            self.card.show_error(
                "No API key — open Edit settings",
                &format!("Add a key under [providers.openai] or [providers.anthropic] in:\n{path}"),
            );
            return;
        }

        self.busy = true;
        // Disarmed for the whole in-flight window: a click while the spinner
        // is up must not touch the card.
        self.set_watch(false);
        self.card.show_pending();

        let chain = Arc::clone(&self.chain);
        let prompt = self.config.ui.prompt.clone();
        let target = self.hwnd_isize();
        std::thread::spawn(move || {
            let result: std::result::Result<Answer, String> =
                worker(&chain, &shot, &prompt).map_err(|e| format!("{e:#}"));
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
        self.busy = false;
        match result {
            Ok(answer) => {
                self.card.show_answer(
                    &answer.headline,
                    &answer.detail,
                    self.config.ui.card_seconds,
                );
                self.last = Some(answer);
                self.set_watch(true);
            }
            Err(e) => {
                let headline = first_line(&e, 88);
                self.card.show_error(&headline, &e);
                self.last = Some(Answer {
                    headline,
                    detail: e,
                });
                self.set_watch(true);
            }
        }
    }

    fn copy_last(&mut self) {
        let Some(answer) = &self.last else {
            self.card.show_answer("Nothing to copy yet", "", 4);
            return;
        };
        let text = if answer.detail.is_empty() {
            answer.headline.clone()
        } else {
            format!("{}\n\n{}", answer.headline, answer.detail)
        };
        match arboard::Clipboard::new().and_then(|mut c| c.set_text(text)) {
            Ok(()) => self.card.show_answer("Copied", "", 3),
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
            Ok(()) => self.card.show_answer(&format!("Bound to {name}"), "", 4),
            Err(e) => self.card.show_error(
                &format!("Bound to {name} — but not saved"),
                &format!("It will work until you quit.\n\n{e:#}"),
            ),
        }
    }

    fn reload(&mut self) {
        match Config::load() {
            Ok(config) => {
                self.config = config;
                self.chain = Arc::new(self.config.build_chain());
                self.card.set_text_scale(self.config.ui.text_scale);
                if let Some(hook) = &self.hook {
                    hook.set_bindings(self.config.hotkeys.primary, self.config.hotkeys.secondary);
                }
                self.refresh_tray_labels();
                self.card.show_answer("Settings reloaded", "", 3);
            }
            Err(e) => self
                .card
                .show_error("Couldn't reload settings", &format!("{e:#}")),
        }
    }

    fn edit_settings(&mut self) {
        let Ok(path) = Config::path() else {
            self.card
                .show_error("Couldn't locate config.toml", "%APPDATA% is not readable.");
            return;
        };
        // Make sure the file exists before asking the shell to open it.
        if !path.exists() {
            let _ = self.config.save();
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
            Ok(()) => self.card.show_answer(&model, "", 3),
            Err(e) => self.card.show_error(
                &format!("Using {model} — but not saved"),
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
        self.tray.set_models(
            &p.openai.models,
            &p.openai.model,
            &p.anthropic.models,
            &p.anthropic.model,
        );

        let ready = self.chain.ready_provider_names();
        let tip = if ready.is_empty() {
            "copilot-ask — no API key configured".to_string()
        } else {
            format!("copilot-ask — {} · {primary}", ready.join(", "))
        };
        self.tray.set_tooltip(&tip);
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

fn worker(chain: &Chain, shot: &Shot, prompt: &str) -> Result<Answer> {
    chain.ask(shot, prompt)
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
}

use std::os::windows::ffi::OsStrExt;

extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCCREATE {
        OWNER_HWND.with(|h| h.set(hwnd.0 as isize));
        return unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) };
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
