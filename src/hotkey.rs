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

pub const HK_PRIMARY: usize = 1;
pub const HK_SECONDARY: usize = 2;

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
    Armed { which: usize, deadline: Instant },
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
                (LearnState::Armed { which, deadline }, LearnOutcome::PassThrough)
            } else {
                (LearnState::Idle, LearnOutcome::Captured { which, chord })
            }
        }
    }
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
    /// [`WM_APP_LEARNED`]. Must be called on the thread that runs the
    /// message loop, and only once per process (a second call fails).
    pub fn install(target: HWND, primary: Chord, secondary: Chord) -> Result<Self> {
        STATE
            .set(Mutex::new(HookShared {
                target_hwnd: target.0 as isize,
                primary,
                secondary,
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
                dwExtraInfo: 0,
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

    // Pause (issue #20): every chord passes through completely unchanged
    // while paused, including the Copilot key -- no swallowing, no Ctrl-tap
    // workaround, no STATE lock, no learn-mode interaction. This check comes
    // before the STATE lookup so pausing costs the hot path exactly one
    // atomic load plus a clock read (see `pause::is_paused_now`), never a
    // lock and never an allocation.
    if crate::pause::is_paused_now() {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let Some(state) = STATE.get() else {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };

    let kb = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let chord = current_chord(kb.vkCode);

    // Snapshot what we need and release the lock before doing anything that
    // could take a while (PostMessage, SendInput) -- never hold it across
    // those calls.
    let (target_hwnd, primary, secondary, learn) = match state.lock() {
        Ok(s) => (s.target_hwnd, s.primary, s.secondary, s.learn),
        Err(_) => return unsafe { CallNextHookEx(None, code, wparam, lparam) },
    };

    if let LearnState::Armed { .. } = learn {
        let (new_learn, outcome) = on_keydown(learn, chord, Instant::now());
        if let Ok(mut s) = state.lock() {
            s.learn = new_learn;
        }
        return match outcome {
            LearnOutcome::Captured { which, chord } => {
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

                LRESULT(1)
            }
            LearnOutcome::PassThrough => unsafe { CallNextHookEx(None, code, wparam, lparam) },
        };
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
        let (state, outcome) = on_keydown(LearnState::Idle, chord(0x41, false, false, false, false), Instant::now());
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
            assert_eq!(outcome, LearnOutcome::PassThrough, "vk=0x{vk:X} should be ignored");
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

    // -- needs_win_release_workaround -------------------------------------

    #[test]
    fn win_chord_needs_the_release_workaround() {
        // The primary binding, Win+Shift+F23: swallowing it must run the tap.
        assert!(needs_win_release_workaround(&chord(0x86, false, true, false, true)));
    }

    #[test]
    fn non_win_chord_does_not_need_the_release_workaround() {
        // Ctrl+Shift+/: no Win key involved, no Start-menu tracking to cancel.
        assert!(!needs_win_release_workaround(&chord(0xBF, true, true, false, false)));
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
        assert_eq!(chord_to_string(&chord(0x41, false, false, false, false)), "A");
        assert_eq!(chord_to_string(&chord(0x30, false, false, false, false)), "0");
    }

    #[test]
    fn renders_all_modifiers_in_order() {
        let c = chord(0x1B, true, true, true, true);
        assert_eq!(chord_to_string(&c), "Ctrl+Alt+Win+Shift+Esc");
    }

    #[test]
    fn falls_back_to_vk_hex_for_unmapped_codes() {
        // 0x07 is unassigned in the VK table.
        assert_eq!(chord_to_string(&chord(0x07, false, false, false, false)), "VK(0x07)");
    }
}
