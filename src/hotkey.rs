//! `WH_KEYBOARD_LL` hook: chord matching, learn mode, and the Win-key release
//! workaround.
//!
//! See `docs/superpowers/specs/2026-09-14-copilot-ask-design.md`, section
//! "Hotkeys", for the behaviour this implements.
//!
//! A low-level keyboard hook requires a message loop on the thread that
//! installs it, so [`HotkeyHook::install`] must be called from the main
//! thread. The hook callback (`hook_proc`) is an `extern "system" fn` and
//! cannot carry state directly, so all shared state lives behind a single
//! `OnceLock<Mutex<HookShared>>`. The lock is only ever held for the few
//! instructions needed to snapshot or update that state -- never across a
//! `PostMessageW` call -- and the pass-through path (the overwhelming
//! majority of keystrokes on the system) takes the lock once, does no
//! allocation, and returns. Keeping this fast matters: Windows silently
//! unhooks a `WH_KEYBOARD_LL` callback that takes too long.
//!
//! # Window messages owned by this module
//!
//! `WM_APP_HOTKEY` and `WM_APP_LEARNED` are the two window messages this
//! module posts to the target `HWND` (see the spec's "Window messages"
//! table). They are defined here, matching the spec's `WM_APP + n` values
//! exactly, since this is the module that owns posting them; the integrating
//! agent should reference `hotkey::WM_APP_HOTKEY` / `hotkey::WM_APP_LEARNED`
//! rather than redefining them. Adding another `WM_APP_*` constant anywhere
//! in the crate also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS`
//! (issue #163), which is enforced by `wm_app_ids_registry_is_exhaustive`.

use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYBD_EVENT_FLAGS,
    KEYEVENTF_KEYUP, VIRTUAL_KEY, VK_CONTROL, VK_LCONTROL, VK_LMENU, VK_LSHIFT, VK_LWIN, VK_MENU,
    VK_RCONTROL, VK_RMENU, VK_RSHIFT, VK_RWIN, VK_SHIFT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, KBDLLHOOKSTRUCT,
    WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// Posted to the target `HWND` when a bound chord fires. `wparam` is
/// [`HK_PRIMARY`] or [`HK_SECONDARY`]; `lparam` is unused (0).
pub const WM_APP_HOTKEY: u32 = WM_APP + 2;
/// Posted to the target `HWND` when learn mode captures a key. `wparam` is
/// the `which` value passed to [`HotkeyHook::start_learning`]; `lparam` is
/// `Box::into_raw(Box::new(chord)) as isize` -- the receiver takes ownership
/// and must reconstruct the `Box` (e.g. `Box::from_raw(lparam.0 as *mut
/// Chord)`) to free it.
pub const WM_APP_LEARNED: u32 = WM_APP + 4;
/// Posted to the target `HWND` when the configured pause-toggle chord fires
/// (issue #181; `hotkeys.pause` in config, no default binding). `wparam`
/// and `lparam` are unused (0) -- `app.rs` decides pause vs. resume from its
/// own current `PauseState`, not from anything carried on this message.
pub const WM_APP_PAUSE_TOGGLE: u32 = WM_APP + 7;
/// Posted to the target `HWND` when the configured Quick Ask palette chord
/// fires (issue #25; `hotkeys.palette` in config, no default binding).
/// `wparam`/`lparam` are unused (0) -- `App::toggle_palette` decides show
/// vs. hide from the palette's own current visibility, not from anything
/// carried on the message. Unlike [`WM_APP_PAUSE_TOGGLE`], this chord is
/// checked in the ordinary (not-paused) match path alongside
/// primary/secondary -- it does not fire while paused.
pub const WM_APP_PALETTE_TOGGLE: u32 = WM_APP + 10;

pub const HK_PRIMARY: usize = 1;
pub const HK_SECONDARY: usize = 2;

/// `dwExtraInfo` tag applied to every synthetic input event Wingman itself
/// injects via `SendInput`, anywhere in the crate -- the ONE constant every
/// such call site uses: [`ctrl_input`] below,
/// `inputs::selection::win32::inject_events`'s Ctrl+C fallback, and
/// `executors::target::inject_unicode_events`'s typed-input fallback
/// (`fill_form` and every other executor that falls back to typing reach
/// `SendInput` only through that one function). Issue #209: before this,
/// `inputs::selection` and `executors::target` each defined their own
/// private copy of the same value, and nothing in this file ever read
/// either of them -- `hook_proc` matched a synthetic keydown exactly like a
/// real one.
///
/// `hook_proc` treats any keydown tagged with exactly this value as
/// Wingman's own synthetic input and lets it pass straight through: never
/// swallowed, never matched against a hotkey or the pause chord, never fed
/// to learn mode. Deliberately NOT a blanket check of `LLKHF_INJECTED` (the
/// low-level hook's own flag for "some process injected this event",
/// without saying which): some third-party key remapper may legitimately
/// inject the Copilot key, or any other configured chord, on the user's
/// behalf -- this crate's own hotkey pitfall notes ("Learn mode exists
/// because Dell firmware may emit something else") already treat remapped
/// input as an expected source, not a hostile one. Filtering by this
/// specific tag instead means only Wingman's OWN synthetic events are ever
/// ignored; a genuinely `LLKHF_INJECTED` event carrying any other
/// `dwExtraInfo` value (including a real remapper's own tag, or none) is
/// still treated exactly like a real keypress.
pub const INJECTED_MARKER: usize = 0x5749_4E47; // ASCII "WING"

const LEARN_TIMEOUT: Duration = Duration::from_secs(10);

/// A key combination: a trigger virtual-key plus the modifier keys held with
/// it. This is a "shared contract" type (see the spec) -- its shape is fixed
/// and other modules (config, tray, app) depend on it exactly as written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
pub struct Chord {
    pub vk: u32,
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub win: bool,
}

/// Learn-mode state machine, pure and independent of the hook callback so it
/// can be unit-tested without Win32.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum LearnState {
    Idle,
    /// Armed for binding slot `which` (see [`HK_PRIMARY`] / [`HK_SECONDARY`]),
    /// expiring at `deadline`.
    Armed {
        which: usize,
        deadline: Instant,
    },
}

/// Result of feeding a keydown into the learn-mode state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnOutcome {
    /// Not learning (was idle, timed out just now, or the key was a bare
    /// modifier that must be ignored so it isn't captured as the chord's
    /// trigger). The caller should let the key proceed to normal hotkey
    /// matching / `CallNextHookEx`.
    PassThrough,
    /// A real trigger key was captured for binding slot `which`.
    Captured { which: usize, chord: Chord },
}

/// Advance the learn-mode state machine on a keydown.
///
/// `chord` is the full chord built from the event (trigger vk + current
/// modifier state) -- this doubles as the "vk plus mods" the caller observed.
/// `now` is the current time; tests pass synthetic instants (`Instant::now()
/// +/- Duration`) to exercise the timeout without sleeping.
pub fn on_keydown(state: LearnState, chord: Chord, now: Instant) -> (LearnState, LearnOutcome) {
    match state {
        LearnState::Idle => (LearnState::Idle, LearnOutcome::PassThrough),
        LearnState::Armed { which, deadline } => {
            if now >= deadline {
                (LearnState::Idle, LearnOutcome::PassThrough)
            } else if is_bare_modifier(chord.vk) {
                // A bare modifier keydown (e.g. the Shift the user is about
                // to hold down before pressing the real trigger key) must
                // not itself become the captured chord. Stay armed.
                (
                    LearnState::Armed { which, deadline },
                    LearnOutcome::PassThrough,
                )
            } else {
                (LearnState::Idle, LearnOutcome::Captured { which, chord })
            }
        }
    }
}

/// #185: whether the keydown that produced `outcome` from [`on_keydown`]
/// should still be checked against the ordinary primary/secondary hotkey
/// match, rather than being treated as fully handled by learn mode.
///
/// Only [`LearnOutcome::Captured`] is fully handled (it was consumed as the
/// new binding). Every `PassThrough` -- a bare modifier kept armed, *or* a
/// stale/expired deadline -- must still get its normal chance to fire the
/// hotkey. Without this, the very keydown whose learn-mode deadline just
/// expired is silently handed to `CallNextHookEx` without ever being
/// compared against `primary`/`secondary`: if that keydown happens to BE
/// the user's actual configured hotkey (the likely case for the first press
/// after Resume, when learn mode was armed and then the deadline went stale
/// while paused -- issue #185's second-order effect), `ask()` never fires
/// on that press, only on the next one.
pub fn should_check_hotkey_match(outcome: LearnOutcome) -> bool {
    !matches!(outcome, LearnOutcome::Captured { .. })
}

/// True for a virtual-key that is itself a modifier (either the generic or
/// left/right-specific form): Shift, Ctrl, Alt, or Win alone.
fn is_bare_modifier(vk: u32) -> bool {
    if vk > u16::MAX as u32 {
        return false;
    }
    matches!(
        VIRTUAL_KEY(vk as u16),
        VK_SHIFT
            | VK_LSHIFT
            | VK_RSHIFT
            | VK_CONTROL
            | VK_LCONTROL
            | VK_RCONTROL
            | VK_MENU
            | VK_LMENU
            | VK_RMENU
            | VK_LWIN
            | VK_RWIN
    )
}

/// Exact-match comparison of a live chord against a stored binding.
fn matches(chord: &Chord, binding: &Chord) -> bool {
    chord.vk == binding.vk
        && chord.ctrl == binding.ctrl
        && chord.shift == binding.shift
        && chord.alt == binding.alt
        && chord.win == binding.win
}

/// Packs a [`Chord`] into a single `u64` for [`PAUSE_CHORD`] (issue #181):
/// the lock-free atomic the hook's paused fast path reads instead of taking
/// `STATE`'s `Mutex`. `vk` (a Windows virtual-key code, always in `0..=255`
/// in practice) sits in the low 16 bits, one bit each for the four
/// modifiers above that. `0` is never produced by a real chord -- every
/// Windows virtual-key code is nonzero -- so it doubles as the "no pause
/// chord configured" sentinel ([`unpack_pause_chord`] relies on this).
fn pack_chord(c: Chord) -> u64 {
    (c.vk as u64 & 0xFFFF)
        | (c.ctrl as u64) << 16
        | (c.shift as u64) << 17
        | (c.alt as u64) << 18
        | (c.win as u64) << 19
}

/// Inverse of [`pack_chord`].
fn unpack_chord(packed: u64) -> Chord {
    Chord {
        vk: (packed & 0xFFFF) as u32,
        ctrl: packed & (1 << 16) != 0,
        shift: packed & (1 << 17) != 0,
        alt: packed & (1 << 18) != 0,
        win: packed & (1 << 19) != 0,
    }
}

/// Outcome of checking a keydown's chord against the configured
/// pause-toggle chord (issue #181), independent of Win32/atomics so it is
/// unit-tested directly (AGENTS.md rule 8). Fires the same whether the app
/// is currently paused or running -- `app.rs`'s `WM_APP_PAUSE_TOGGLE`
/// handler decides pause-vs-resume from its own `PauseState`, not from
/// anything computed here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseHotkeyOutcome {
    /// The keydown matched the configured pause chord: swallow it and post
    /// [`WM_APP_PAUSE_TOGGLE`]. This is the one outcome that must still be
    /// checked while paused -- see `hook_proc`'s doc comment on the paused
    /// fast path.
    Toggle,
    /// No match, or no pause chord configured at all (`pause_chord` is
    /// `None` -- issue #181's "no default binding" decision): ordinary
    /// paused/not-paused handling decides what happens to this keydown.
    PassThrough,
}

/// Pure decision `hook_proc` makes both on the paused fast path and on the
/// ordinary (not-paused) path, so the two can never disagree on what counts
/// as a pause-toggle press.
pub fn pause_hotkey_outcome(chord: &Chord, pause_chord: Option<Chord>) -> PauseHotkeyOutcome {
    match pause_chord {
        Some(pc) if matches(chord, &pc) => PauseHotkeyOutcome::Toggle,
        _ => PauseHotkeyOutcome::PassThrough,
    }
}

/// Target `HWND` [`WM_APP_PAUSE_TOGGLE`] is posted to, mirrored from
/// `HookShared::target_hwnd` into its own atomic (set once at
/// [`HotkeyHook::install`], never mutated afterward) so the paused fast path
/// can reach it without taking `STATE`'s `Mutex` -- see [`PAUSE_CHORD`]'s
/// doc comment for why that matters.
static TARGET_HWND: AtomicIsize = AtomicIsize::new(0);

/// Lock-free packed pause-toggle chord the hook's hot path reads on every
/// keydown, paused or not (issue #181). `0` = unconfigured (see
/// [`pack_chord`]'s doc comment). Written only by
/// [`HotkeyHook::set_pause_chord`], read only by `hook_proc` via
/// [`pause_hotkey_outcome`] -- this is what lets the paused fast path check
/// "does this keydown toggle Pause" without ever touching `STATE`'s `Mutex`,
/// matching `pause.rs`'s own `PAUSE_DEADLINE` pattern (see that module's
/// "hook's fast path" doc section) for exactly the same reason: Windows
/// silently unhooks a `WH_KEYBOARD_LL` callback that takes too long.
static PAUSE_CHORD: AtomicU64 = AtomicU64::new(0);

/// Render a chord for display, e.g. `Ctrl+Shift+/` or `Win+Shift+F23`.
///
/// Modifier order is Ctrl, Alt, Win, Shift, so Shift (when present) always
/// sits immediately before the trigger key, matching the two examples in the
/// spec. Common virtual-keys get readable names; anything else falls back to
/// `VK(0xNN)`.
pub fn chord_to_string(c: &Chord) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);
    if c.ctrl {
        parts.push("Ctrl".to_string());
    }
    if c.alt {
        parts.push("Alt".to_string());
    }
    if c.win {
        parts.push("Win".to_string());
    }
    if c.shift {
        parts.push("Shift".to_string());
    }
    parts.push(vk_name(c.vk));
    parts.join("+")
}

fn vk_name(vk: u32) -> String {
    match vk {
        0x08 => "Backspace".to_string(),
        0x09 => "Tab".to_string(),
        0x0D => "Enter".to_string(),
        0x1B => "Esc".to_string(),
        0x20 => "Space".to_string(),
        0x21 => "PageUp".to_string(),
        0x22 => "PageDown".to_string(),
        0x23 => "End".to_string(),
        0x24 => "Home".to_string(),
        0x25 => "Left".to_string(),
        0x26 => "Up".to_string(),
        0x27 => "Right".to_string(),
        0x28 => "Down".to_string(),
        0x2D => "Insert".to_string(),
        0x2E => "Delete".to_string(),
        0x30..=0x39 => ((vk as u8) as char).to_string(), // '0'..'9'
        0x41..=0x5A => ((vk as u8) as char).to_string(), // 'A'..'Z'
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),    // F1..F24
        0xBA => ";".to_string(),
        0xBB => "=".to_string(),
        0xBC => ",".to_string(),
        0xBD => "-".to_string(),
        0xBE => ".".to_string(),
        0xBF => "/".to_string(),
        0xC0 => "`".to_string(),
        0xDB => "[".to_string(),
        0xDC => "\\".to_string(),
        0xDD => "]".to_string(),
        0xDE => "'".to_string(),
        _ => format!("VK(0x{vk:02X})"),
    }
}

/// Shared, cross-call state for the hook callback. Kept free of raw Win32
/// handle types so the containing `Mutex` is trivially `Send`/`Sync`; the
/// target `HWND` is stored as its raw pointer value.
struct HookShared {
    target_hwnd: isize,
    primary: Chord,
    secondary: Chord,
    /// Issue #25: the Quick Ask palette chord, `None` by default (no default
    /// binding, matching `pause`'s own decision). Unlike `pause`'s chord,
    /// this lives in the ordinary locked state (not a lock-free atomic):
    /// the paused fast path never checks it, since this chord must NOT fire
    /// while paused -- see [`WM_APP_PALETTE_TOGGLE`]'s doc comment.
    palette: Option<Chord>,
    learn: LearnState,
}

static STATE: OnceLock<Mutex<HookShared>> = OnceLock::new();

/// Owns the installed `WH_KEYBOARD_LL` hook. Must be created on, and
/// outlive, the thread running the Win32 message loop.
pub struct HotkeyHook {
    hhook: HHOOK,
}

impl HotkeyHook {
    /// Installs the hook. `target` receives [`WM_APP_HOTKEY`] /
    /// [`WM_APP_LEARNED`] / [`WM_APP_PAUSE_TOGGLE`]. Must be called on the
    /// thread that runs the message loop, and only once per process (a
    /// second call fails).
    pub fn install(target: HWND, primary: Chord, secondary: Chord) -> Result<Self> {
        // Mirrored outside STATE's Mutex so the paused fast path can reach
        // it lock-free (see TARGET_HWND's doc comment). Set once, here,
        // before the hook can possibly fire.
        TARGET_HWND.store(target.0 as isize, Ordering::Relaxed);

        STATE
            .set(Mutex::new(HookShared {
                target_hwnd: target.0 as isize,
                primary,
                secondary,
                palette: None,
                learn: LearnState::Idle,
            }))
            .map_err(|_| anyhow::anyhow!("HotkeyHook::install called more than once"))?;

        let hhook = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook_proc), None, 0) }
            .context("SetWindowsHookExW(WH_KEYBOARD_LL) failed")?;

        Ok(Self { hhook })
    }

    /// Replace both bindings. Safe to call from the thread that owns the
    /// message loop while the hook is live.
    pub fn set_bindings(&self, primary: Chord, secondary: Chord) {
        if let Some(state) = STATE.get() {
            if let Ok(mut s) = state.lock() {
                s.primary = primary;
                s.secondary = secondary;
            }
        }
    }

    /// Set or clear the pause-toggle chord (issue #181; `hotkeys.pause` in
    /// config, `None` by default -- no default binding). Lock-free: writes
    /// straight to [`PAUSE_CHORD`], the atomic the hot path reads; there is
    /// nothing here for `STATE`'s `Mutex` to protect. Safe to call from the
    /// thread that owns the message loop while the hook is live, same as
    /// [`HotkeyHook::set_bindings`].
    pub fn set_pause_chord(&self, chord: Option<Chord>) {
        PAUSE_CHORD.store(chord.map(pack_chord).unwrap_or(0), Ordering::Relaxed);
    }

    /// Set or clear the Quick Ask palette chord (issue #25; `hotkeys.palette`
    /// in config, `None` by default -- no default binding). Locked, not
    /// lock-free, unlike [`HotkeyHook::set_pause_chord`]: see [`HookShared::palette`]'s
    /// doc comment for why this chord does not need the paused fast path at
    /// all. Safe to call from the thread that owns the message loop while
    /// the hook is live.
    pub fn set_palette_chord(&self, chord: Option<Chord>) {
        if let Some(state) = STATE.get() {
            if let Ok(mut s) = state.lock() {
                s.palette = chord;
            }
        }
    }

    /// Arm learn mode for binding slot `which`. The next non-modifier keydown
    /// is captured instead of matched (see [`WM_APP_LEARNED`]). Expires after
    /// 10 seconds if nothing is pressed.
    pub fn start_learning(&self, which: usize) {
        if let Some(state) = STATE.get() {
            if let Ok(mut s) = state.lock() {
                s.learn = LearnState::Armed {
                    which,
                    deadline: Instant::now() + LEARN_TIMEOUT,
                };
            }
        }
    }

    /// Disarm learn mode without capturing anything.
    pub fn cancel_learning(&self) {
        if let Some(state) = STATE.get() {
            if let Ok(mut s) = state.lock() {
                s.learn = LearnState::Idle;
            }
        }
    }
}

impl Drop for HotkeyHook {
    fn drop(&mut self) {
        // Never panic on the main thread: ignore failure, there is nothing
        // useful to do about it during teardown.
        let _ = unsafe { UnhookWindowsHookEx(self.hhook) };
    }
}

/// Build the chord implied by a keydown: `vk` from the event, modifiers from
/// live keyboard state.
fn current_chord(vk: u32) -> Chord {
    Chord {
        vk,
        ctrl: key_down(VK_CONTROL.0 as i32),
        shift: key_down(VK_SHIFT.0 as i32),
        alt: key_down(VK_MENU.0 as i32),
        win: key_down(VK_LWIN.0 as i32) || key_down(VK_RWIN.0 as i32),
    }
}

fn key_down(vk: i32) -> bool {
    (unsafe { GetAsyncKeyState(vk) } as u16 & 0x8000) != 0
}

/// Build a single Ctrl key-event input (down or up) for [`send_ctrl_tap`].
/// Tagged with [`INJECTED_MARKER`] (issue #209) like every other synthetic
/// input this crate injects, so `hook_proc` never treats its own Win-release
/// workaround as a real keypress -- harmless today only because no
/// configured chord happens to be shaped like a bare Ctrl tap, exactly the
/// latent trap #209 was filed about.
fn ctrl_input(key_up: bool) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VK_CONTROL,
                wScan: 0,
                dwFlags: if key_up {
                    KEYEVENTF_KEYUP
                } else {
                    KEYBD_EVENT_FLAGS(0)
                },
                time: 0,
                dwExtraInfo: INJECTED_MARKER,
            },
        },
    }
}

/// The Win-key release problem (see the spec): swallowing a Win-chord
/// keydown (returning 1 from the LL hook) leaves LWin/RWin logically held
/// down as far as the shell's Start-menu tracking is concerned, because the
/// keyup that would normally cancel it never gets a chance to combine with
/// anything else first. Left alone, the eventual physical keyup opens the
/// Start menu. Injecting a harmless Ctrl down+up in between gives the shell
/// something else to see first, which cancels the pending Start-menu
/// activation; a lone Ctrl tap has no effect otherwise.
fn send_ctrl_tap() {
    let inputs = [ctrl_input(false), ctrl_input(true)];
    unsafe {
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }
}

/// Whether swallowing this chord's keydown (returning 1 from the hook, as
/// both an ordinary matched hotkey and a captured learn-mode chord do) needs
/// the [`send_ctrl_tap`] workaround for the Win-key release problem. Pure
/// and allocation-free so the hook callback can call it on every swallowed
/// event without risking the "hook takes too long, Windows silently unhooks
/// it" failure mode.
fn needs_win_release_workaround(chord: &Chord) -> bool {
    chord.win
}

/// Issue #209: whether `hook_proc` should treat a keydown as Wingman's own
/// synthetic input -- checked first, before reading `kb.vkCode` or any live
/// modifier state, so a matching event never reaches hotkey matching,
/// pause-toggle matching, or learn mode at all. `true` only for exactly
/// [`INJECTED_MARKER`]; every other value (including `0`, an untagged real
/// keypress, and any other process's own tag on a genuinely
/// `LLKHF_INJECTED` event) is real input as far as this crate is concerned
/// -- see `INJECTED_MARKER`'s doc comment for why this deliberately does
/// not check the hook's `LLKHF_INJECTED` flag at all. Pure and
/// allocation-free, same reasoning as [`needs_win_release_workaround`]: the
/// hook callback cannot afford anything slower than a comparison.
fn is_own_synthetic_input(dw_extra_info: usize) -> bool {
    dw_extra_info == INJECTED_MARKER
}

/// Reads [`PAUSE_CHORD`] and unpacks it, lock-free. `None` if no pause
/// chord is configured (the packed sentinel `0`).
fn load_pause_chord() -> Option<Chord> {
    match PAUSE_CHORD.load(Ordering::Relaxed) {
        0 => None,
        packed => Some(unpack_chord(packed)),
    }
}

/// Posts [`WM_APP_PAUSE_TOGGLE`] to the installed hook's target window and
/// runs the Win-release workaround if `chord` needs it -- the two things
/// both call sites in `hook_proc` do when [`pause_hotkey_outcome`] returns
/// `Toggle`. Reads [`TARGET_HWND`] rather than `STATE`'s locked copy so the
/// paused call site stays lock-free.
fn post_pause_toggle(chord: &Chord) {
    let hwnd = HWND(TARGET_HWND.load(Ordering::Relaxed) as *mut _);
    let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_PAUSE_TOGGLE, WPARAM(0), LPARAM(0)) };
    if needs_win_release_workaround(chord) {
        send_ctrl_tap();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Per the WH_KEYBOARD_LL contract: if code < 0, pass through untouched
    // and do not swallow, regardless of anything else.
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let msg = wparam.0 as u32;
    if msg != WM_KEYDOWN && msg != WM_SYSKEYDOWN {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };

    // Issue #209: Wingman's own synthetic input (the Win-release Ctrl tap,
    // learn mode's own tap, the selection-reading Ctrl+C fallback, the
    // typed-input fallback) must never be treated as a real keypress --
    // checked before anything else, including the paused branch below, so
    // it can never accidentally toggle Pause or match a hotkey either.
    if is_own_synthetic_input(kb.dwExtraInfo) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let chord = current_chord(kb.vkCode);

    // Pause (issue #20) / pause-toggle chord (issue #181): while paused,
    // every chord passes through completely unchanged EXCEPT the configured
    // pause-toggle chord, which still fires -- the one chord the paused
    // hook does not pass through, and the hook's only exception to "no
    // swallowing, no Ctrl-tap workaround, no STATE lock, no learn-mode
    // interaction" while paused. Both checks here are lock-free
    // (`is_paused_now`: one atomic load plus a clock read; `load_pause_chord`:
    // one more atomic load) -- no STATE mutex, so pausing still costs the
    // hot path no more than a couple of atomic loads, never a lock.
    if crate::pause::is_paused_now() {
        if pause_hotkey_outcome(&chord, load_pause_chord()) == PauseHotkeyOutcome::Toggle {
            post_pause_toggle(&chord);
            return LRESULT(1);
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let Some(state) = STATE.get() else {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };

    // Snapshot what we need and release the lock before doing anything that
    // could take a while (PostMessage, SendInput) -- never hold it across
    // those calls.
    let (target_hwnd, primary, secondary, palette, learn) = match state.lock() {
        Ok(s) => (s.target_hwnd, s.primary, s.secondary, s.palette, s.learn),
        Err(_) => return unsafe { CallNextHookEx(None, code, wparam, lparam) },
    };

    if let LearnState::Armed { .. } = learn {
        let (new_learn, outcome) = on_keydown(learn, chord, Instant::now());
        if let Ok(mut s) = state.lock() {
            s.learn = new_learn;
        }
        if let LearnOutcome::Captured { which, chord } = outcome {
            let boxed = Box::into_raw(Box::new(chord));
            let hwnd = HWND(target_hwnd as *mut _);
            let _ = unsafe {
                PostMessageW(
                    Some(hwnd),
                    WM_APP_LEARNED,
                    WPARAM(which),
                    LPARAM(boxed as isize),
                )
            };

            // Learn mode swallows this keydown the same way the ordinary
            // match branch below swallows a bound hotkey; a captured
            // chord that involves Win needs the same workaround, or
            // learning a Win-involving chord flickers the Start menu
            // (issue #151).
            if needs_win_release_workaround(&chord) {
                send_ctrl_tap();
            }

            return LRESULT(1);
        }
        // #185: `LearnOutcome::PassThrough` here means either a bare
        // modifier kept armed, or the deadline just expired -- neither
        // consumed this keydown, so (unlike a `return` here in the old
        // code) fall through to the ordinary hotkey match below instead of
        // handing it straight to `CallNextHookEx`. `should_check_hotkey_match`
        // exists only to make this decision unit-testable.
        debug_assert!(should_check_hotkey_match(outcome));
    }

    // #181: pause-toggle chord, checked after learn mode (which takes
    // priority above -- learning a binding is never hijacked by this) and
    // before the ordinary primary/secondary match, using the same
    // lock-free atomic the paused branch above reads.
    if pause_hotkey_outcome(&chord, load_pause_chord()) == PauseHotkeyOutcome::Toggle {
        post_pause_toggle(&chord);
        return LRESULT(1);
    }

    // #25: the palette chord, checked alongside primary/secondary (not
    // exempted from the paused early-return above, unlike the pause-toggle
    // chord) -- opening the palette while paused would defeat the point of
    // pausing.
    if let Some(pc) = palette {
        if matches(&chord, &pc) {
            let hwnd = HWND(target_hwnd as *mut _);
            let _ =
                unsafe { PostMessageW(Some(hwnd), WM_APP_PALETTE_TOGGLE, WPARAM(0), LPARAM(0)) };
            if needs_win_release_workaround(&pc) {
                send_ctrl_tap();
            }
            return LRESULT(1);
        }
    }

    let which = if matches(&chord, &primary) {
        Some((HK_PRIMARY, primary))
    } else if matches(&chord, &secondary) {
        Some((HK_SECONDARY, secondary))
    } else {
        None
    };

    if let Some((which, matched)) = which {
        let hwnd = HWND(target_hwnd as *mut _);
        let _ = unsafe { PostMessageW(Some(hwnd), WM_APP_HOTKEY, WPARAM(which), LPARAM(0)) };

        if needs_win_release_workaround(&matched) {
            send_ctrl_tap();
        }

        return LRESULT(1);
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chord(vk: u32, ctrl: bool, shift: bool, alt: bool, win: bool) -> Chord {
        Chord {
            vk,
            ctrl,
            shift,
            alt,
            win,
        }
    }

    // -- matches() -----------------------------------------------------

    #[test]
    fn matches_exact_chord() {
        let c = chord(0xBF, true, true, false, false);
        let b = chord(0xBF, true, true, false, false);
        assert!(matches(&c, &b));
    }

    #[test]
    fn matches_rejects_modifier_mismatch() {
        let c = chord(0xBF, true, true, false, false);
        let b = chord(0xBF, true, false, false, false); // shift differs
        assert!(!matches(&c, &b));
    }

    #[test]
    fn matches_rejects_vk_mismatch() {
        let c = chord(0xBF, true, true, false, false);
        let b = chord(0x86, true, true, false, false);
        assert!(!matches(&c, &b));
    }

    // -- learn-mode state machine --------------------------------------

    #[test]
    fn learn_idle_passes_everything_through() {
        let (state, outcome) = on_keydown(
            LearnState::Idle,
            chord(0x41, false, false, false, false),
            Instant::now(),
        );
        assert_eq!(state, LearnState::Idle);
        assert_eq!(outcome, LearnOutcome::PassThrough);
    }

    #[test]
    fn learn_captures_real_trigger_key() {
        let now = Instant::now();
        let armed = LearnState::Armed {
            which: HK_SECONDARY,
            deadline: now + Duration::from_secs(5),
        };
        let (state, outcome) = on_keydown(armed, chord(0xBF, true, true, false, false), now);
        assert_eq!(state, LearnState::Idle);
        assert_eq!(
            outcome,
            LearnOutcome::Captured {
                which: HK_SECONDARY,
                chord: chord(0xBF, true, true, false, false),
            }
        );
    }

    #[test]
    fn learn_ignores_bare_modifier_and_stays_armed() {
        let now = Instant::now();
        let deadline = now + Duration::from_secs(5);
        let armed = LearnState::Armed {
            which: HK_PRIMARY,
            deadline,
        };
        // VK_LSHIFT held alone, no other modifiers yet resolved.
        let (state, outcome) = on_keydown(armed, chord(0xA0, false, true, false, false), now);
        assert_eq!(
            state,
            LearnState::Armed {
                which: HK_PRIMARY,
                deadline
            }
        );
        assert_eq!(outcome, LearnOutcome::PassThrough);
    }

    #[test]
    fn learn_bare_modifier_covers_all_variants() {
        let now = Instant::now();
        for vk in [
            0x10u32, 0xA0, 0xA1, // Shift, LShift, RShift
            0x11, 0xA2, 0xA3, // Control, LControl, RControl
            0x12, 0xA4, 0xA5, // Menu, LMenu, RMenu
            0x5B, 0x5C, // LWin, RWin
        ] {
            let armed = LearnState::Armed {
                which: HK_PRIMARY,
                deadline: now + Duration::from_secs(5),
            };
            let (_, outcome) = on_keydown(armed, chord(vk, false, false, false, false), now);
            assert_eq!(
                outcome,
                LearnOutcome::PassThrough,
                "vk=0x{vk:X} should be ignored"
            );
        }
    }

    #[test]
    fn learn_times_out() {
        let now = Instant::now();
        let armed = LearnState::Armed {
            which: HK_PRIMARY,
            deadline: now - Duration::from_millis(1), // already expired
        };
        let (state, outcome) = on_keydown(armed, chord(0x41, false, false, false, false), now);
        assert_eq!(state, LearnState::Idle);
        assert_eq!(outcome, LearnOutcome::PassThrough);
    }

    // -- should_check_hotkey_match (#185, pure) ----------------------------

    #[test]
    fn captured_should_not_be_checked_against_hotkey_match() {
        assert!(!should_check_hotkey_match(LearnOutcome::Captured {
            which: HK_PRIMARY,
            chord: chord(0x41, false, false, false, false),
        }));
    }

    #[test]
    fn pass_through_should_still_be_checked_against_hotkey_match() {
        assert!(should_check_hotkey_match(LearnOutcome::PassThrough));
    }

    #[test]
    fn a_keydown_whose_deadline_just_expired_still_falls_through_to_hotkey_matching() {
        // #185's second-order effect, exercised end to end through the pure
        // functions `hook_proc` calls (not `hook_proc` itself, which is
        // Win32 glue -- see the module's `hook_proc` for the wiring this
        // proves is correct): a stale `Armed` deadline must not cause the
        // keydown that discovers it to be dropped from hotkey matching.
        let now = Instant::now();
        let armed = LearnState::Armed {
            which: HK_PRIMARY,
            deadline: now - Duration::from_millis(1), // already expired
        };
        let real_hotkey_chord = chord(0x86, false, true, false, true); // Win+Shift+F23-ish
        let (state, outcome) = on_keydown(armed, real_hotkey_chord, now);
        assert_eq!(state, LearnState::Idle);
        assert_eq!(outcome, LearnOutcome::PassThrough);
        assert!(
            should_check_hotkey_match(outcome),
            "the same keydown that timed out learn mode must still get a chance to match \
             the ordinary hotkey -- otherwise it's silently dropped instead of triggering ask()"
        );
    }

    // -- needs_win_release_workaround -------------------------------------

    #[test]
    fn win_chord_needs_the_release_workaround() {
        // The primary binding, Win+Shift+F23: swallowing it must run the tap.
        assert!(needs_win_release_workaround(&chord(
            0x86, false, true, false, true
        )));
    }

    #[test]
    fn non_win_chord_does_not_need_the_release_workaround() {
        // Ctrl+Shift+/: no Win key involved, no Start-menu tracking to cancel.
        assert!(!needs_win_release_workaround(&chord(
            0xBF, true, true, false, false
        )));
    }

    // -- chord_to_string -------------------------------------------------

    #[test]
    fn renders_ctrl_shift_slash() {
        let c = chord(0xBF, true, true, false, false);
        assert_eq!(chord_to_string(&c), "Ctrl+Shift+/");
    }

    #[test]
    fn renders_win_shift_f23() {
        let c = chord(0x86, false, true, false, true);
        assert_eq!(chord_to_string(&c), "Win+Shift+F23");
    }

    #[test]
    fn renders_letter_and_digit() {
        assert_eq!(
            chord_to_string(&chord(0x41, false, false, false, false)),
            "A"
        );
        assert_eq!(
            chord_to_string(&chord(0x30, false, false, false, false)),
            "0"
        );
    }

    #[test]
    fn renders_all_modifiers_in_order() {
        let c = chord(0x1B, true, true, true, true);
        assert_eq!(chord_to_string(&c), "Ctrl+Alt+Win+Shift+Esc");
    }

    #[test]
    fn falls_back_to_vk_hex_for_unmapped_codes() {
        // 0x07 is unassigned in the VK table.
        assert_eq!(
            chord_to_string(&chord(0x07, false, false, false, false)),
            "VK(0x07)"
        );
    }

    // -- pack_chord / unpack_chord (issue #181) -----------------------------

    #[test]
    fn pack_unpack_round_trips_every_modifier_combination() {
        for ctrl in [false, true] {
            for shift in [false, true] {
                for alt in [false, true] {
                    for win in [false, true] {
                        let c = chord(0x86, ctrl, shift, alt, win);
                        assert_eq!(unpack_chord(pack_chord(c)), c);
                    }
                }
            }
        }
    }

    #[test]
    fn pack_unpack_round_trips_win_shift_f23() {
        // The Copilot key -- also the one chord most likely to collide with
        // the primary binding, so worth its own named case.
        let c = chord(0x86, false, true, false, true);
        assert_eq!(unpack_chord(pack_chord(c)), c);
    }

    #[test]
    fn pack_unpack_round_trips_ctrl_shift_slash() {
        let c = chord(0xBF, true, true, false, false);
        assert_eq!(unpack_chord(pack_chord(c)), c);
    }

    #[test]
    fn a_chord_with_no_modifiers_and_a_real_vk_never_packs_to_the_unset_sentinel() {
        // Every real Windows virtual-key code is nonzero, so `0` (the
        // "unconfigured" sentinel `load_pause_chord` relies on) can only
        // come from `vk == 0`, which no learn-mode capture or hand-edited
        // config can plausibly produce for a genuine trigger key.
        let c = chord(0x41, false, false, false, false); // 'A', no modifiers
        assert_ne!(pack_chord(c), 0);
    }

    #[test]
    fn zero_is_the_unset_sentinel() {
        let c = chord(0, false, false, false, false);
        assert_eq!(pack_chord(c), 0);
    }

    // -- pause_hotkey_outcome (issue #181, pure) -----------------------------

    #[test]
    fn matching_chord_toggles_pause() {
        // The task's own framing: while paused, an event matching the
        // configured pause chord must toggle. `pause_hotkey_outcome` does
        // not look at the paused/running state at all -- it is the same
        // decision either way, which is exactly what lets `hook_proc` reuse
        // it on both the paused fast path and the ordinary path.
        let pause_chord = chord(0x13, false, false, false, false); // Pause/Break
        assert_eq!(
            pause_hotkey_outcome(&pause_chord, Some(pause_chord)),
            PauseHotkeyOutcome::Toggle
        );
    }

    #[test]
    fn other_chords_pass_through_while_a_pause_chord_is_configured() {
        let pause_chord = chord(0x13, false, false, false, false);
        let other = chord(0x41, false, false, false, false); // 'A'
        assert_eq!(
            pause_hotkey_outcome(&other, Some(pause_chord)),
            PauseHotkeyOutcome::PassThrough
        );
    }

    #[test]
    fn every_chord_passes_through_when_no_pause_chord_is_configured() {
        // Issue #181's "no default binding" decision: `None` must never be
        // treated as "matches everything".
        let any = chord(0x86, false, true, false, true); // the Copilot key
        assert_eq!(
            pause_hotkey_outcome(&any, None),
            PauseHotkeyOutcome::PassThrough
        );
    }

    #[test]
    fn pause_chord_match_respects_modifiers_like_the_ordinary_match_does() {
        let pause_chord = chord(0x13, true, false, false, false); // Ctrl+Pause
        let same_key_no_ctrl = chord(0x13, false, false, false, false);
        assert_eq!(
            pause_hotkey_outcome(&same_key_no_ctrl, Some(pause_chord)),
            PauseHotkeyOutcome::PassThrough
        );
    }

    // -- is_own_synthetic_input (issue #209, pure) --------------------------

    #[test]
    fn own_marker_is_recognized_as_synthetic() {
        assert!(is_own_synthetic_input(INJECTED_MARKER));
    }

    #[test]
    fn untagged_real_input_is_not_synthetic() {
        // dwExtraInfo == 0 is what an ordinary, un-injected keypress carries.
        assert!(!is_own_synthetic_input(0));
    }

    #[test]
    fn a_different_tag_is_not_treated_as_our_own_synthetic_input() {
        // Some other process's own dwExtraInfo tag on a genuinely
        // LLKHF_INJECTED event (a key remapper, say) must NOT be filtered --
        // see INJECTED_MARKER's doc comment for why this crate deliberately
        // does not do a blanket LLKHF_INJECTED check: that would also
        // ignore a legitimate remapper injecting the Copilot key itself.
        assert!(!is_own_synthetic_input(0xDEAD_BEEF));
    }

    #[test]
    fn a_hotkey_shaped_like_our_own_synthetic_input_would_still_be_swallowed_by_matches_alone() {
        // Proves #209's own scenario is real, not hypothetical: if a future
        // contributor binds a hotkey to bare Ctrl -- exactly the shape
        // send_ctrl_tap's own workaround injects -- `matches()` alone
        // (hook_proc's very next check after is_own_synthetic_input) would
        // happily fire for it. is_own_synthetic_input is what stops that
        // synthetic tap from ever reaching this comparison at all.
        let bare_ctrl_hotkey = chord(VK_CONTROL.0 as u32, false, false, false, false);
        assert!(matches(&bare_ctrl_hotkey, &bare_ctrl_hotkey));
        assert!(is_own_synthetic_input(INJECTED_MARKER));
    }

    // -- every SendInput call site tags its INPUT (issue #209) --------------

    #[test]
    fn every_sendinput_call_site_tags_its_input_with_the_shared_marker() {
        // An INPUT built without this tag is invisible to
        // is_own_synthetic_input, so a SendInput call site that forgets it
        // is exactly the #209 trap again. Text-scanned the same way
        // app.rs's wm_app_ids_registry_is_exhaustive counts WM_APP_*
        // declarations: a new file that calls SendInput needs adding here.
        for (name, src) in [
            ("hotkey.rs", include_str!("hotkey.rs")),
            ("inputs/selection.rs", include_str!("inputs/selection.rs")),
            ("executors/target.rs", include_str!("executors/target.rs")),
        ] {
            let calls = src.matches("SendInput(").count();
            let tagged = src.matches("dwExtraInfo: INJECTED_MARKER").count();
            assert!(
                calls > 0,
                "{name} no longer calls SendInput -- update this test's file list"
            );
            assert!(
                tagged > 0,
                "{name} calls SendInput {calls} time(s) but no \
                 \"dwExtraInfo: INJECTED_MARKER\" tag was found in it -- every synthetic \
                 INPUT this crate builds must be tagged so hook_proc can recognize and \
                 ignore it"
            );
        }
    }
}
