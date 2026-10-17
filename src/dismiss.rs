//! `WH_MOUSE_LL` hook: a global click watcher used to dismiss the answer
//! card when the user clicks anywhere on screen after an answer is shown.
//!
//! Modelled closely on the `WH_KEYBOARD_LL` hook in `src/hotkey.rs` -- see
//! that file's module doc for the general shape of the problem. The short
//! version: a low-level hook requires a message loop on the installing
//! thread, so [`ClickWatcher::install`] must be called from the main
//! thread; the callback is an `extern "system" fn` and cannot carry state
//! directly, so cross-call state lives in process-wide statics.
//!
//! Unlike the keyboard hook, the only mutable cross-call state here is a
//! single armed/disarmed flag, and it needs to be read on *every* mouse
//! event system-wide (including every `WM_MOUSEMOVE`, which floods this
//! callback far more than any keyboard hook is flooded). So instead of a
//! `Mutex<..>`, the flag is a bare `AtomicBool` -- no lock, no blocking,
//! just a single relaxed load on the hot path. The target `HWND` never
//! changes after `install`, so it lives in a plain `OnceLock<isize>` (set
//! once, read with `.get()`, no lock needed either).
//!
//! Issue #729 (AGENTS.md rule 5, nothing runs while idle): the hook is only
//! in the system's hook chain while a card that wants click-away is shown.
//! [`ClickWatcher::arm`] installs it and [`ClickWatcher::disarm`] (or
//! [`disarm`], which the card calls whenever it hides, auto-dismiss included)
//! removes it, both on the UI thread that owns the message loop. The
//! observable: [`hook_installed`] is `false` whenever no card is visible.
//!
//! The callback still must never swallow a click: it always returns
//! whatever `CallNextHookEx` returns. Reporting a click to the target
//! window is a side effect running alongside the click, not a replacement
//! for it.
//!
//! # Window messages owned by this module
//!
//! [`WM_APP_DISMISS`] is the one window message this module posts to the
//! target `HWND`. It is defined here, matching the design spec's `WM_APP +
//! n` numbering, since this is the module that owns posting it; the
//! integrating agent should reference `dismiss::WM_APP_DISMISS` rather than
//! redefining it. Adding another `WM_APP_*` constant anywhere in the crate
//! also means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` (issue #163),
//! which is enforced by `wm_app_ids_registry_is_exhaustive`.

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::sync::OnceLock;

use anyhow::Result;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, PostMessageW, SetWindowsHookExW, UnhookWindowsHookEx, HHOOK, MSLLHOOKSTRUCT,
    WH_MOUSE_LL, WM_APP, WM_LBUTTONDOWN, WM_MBUTTONDOWN, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
    WM_RBUTTONDOWN,
};

/// Posted to the target `HWND` when a click lands anywhere on screen while
/// the watcher is armed. `wparam` is unused (0).
///
/// `lparam` packs the click's screen point: the low 16 bits hold `x`, the
/// high 16 bits hold `y`, each as the two's-complement bit pattern of a
/// 16-bit signed value (i.e. built the same way `MAKELPARAM`/`GET_X_LPARAM`
/// pack a `POINT` into a Win32 `lParam`). This survives negative screen
/// coordinates -- a monitor placed left of or above the primary monitor --
/// as long as the coordinate fits in `i16` (-32768..=32767), which holds
/// for any real desktop layout. Build the packed value with [`pack_point`]
/// and recover it with [`unpack_point`]; do not reach into `lparam` by
/// hand. Because `LPARAM` is `isize`, extract the packed bits with
/// `lparam.0 as u32` before calling [`unpack_point`] (a plain `as u32`
/// truncation, not a sign-extending cast).
pub const WM_APP_DISMISS: u32 = WM_APP + 5;

/// Pack a screen point into the 32-bit encoding described on
/// [`WM_APP_DISMISS`].
pub fn pack_point(x: i32, y: i32) -> u32 {
    let lo = (x as i16 as u16) as u32;
    let hi = (y as i16 as u16) as u32;
    lo | (hi << 16)
}

/// Inverse of [`pack_point`].
pub fn unpack_point(packed: u32) -> (i32, i32) {
    let x = (packed & 0xFFFF) as u16 as i16 as i32;
    let y = ((packed >> 16) & 0xFFFF) as u16 as i16 as i32;
    (x, y)
}

/// True if `msg` (a `WM_*` value taken from the low-level hook's `wparam`)
/// is one of the button-down events this watcher reports.
fn is_button_down(msg: u32) -> bool {
    msg == WM_LBUTTONDOWN || msg == WM_RBUTTONDOWN || msg == WM_MBUTTONDOWN
}

/// Pure decision the hook callback makes on every event: report it (post
/// [`WM_APP_DISMISS`]) only when armed and the event is a button-down.
/// Split out from `hook_proc` so the armed/disarmed gate can be unit-tested
/// without touching Win32.
fn should_report(armed: bool, msg: u32) -> bool {
    armed && is_button_down(msg)
}

/// True if `msg` is a wheel turn, vertical or horizontal.
fn is_wheel(msg: u32) -> bool {
    msg == WM_MOUSEWHEEL || msg == WM_MOUSEHWHEEL
}

/// #809: what the overlay wants to hear about a mouse event: any button down,
/// and the wheel. `None` when the overlay is not armed or the event is
/// neither (a move floods the callback and is never reported). The callback
/// only posts what it saw; the main thread decides what it means.
fn overlay_report(armed: bool, msg: u32) -> Option<crate::overlay::wire::Kind> {
    use crate::overlay::wire::Kind;
    if !armed {
        None
    } else if is_button_down(msg) {
        Some(Kind::Click)
    } else if is_wheel(msg) {
        Some(Kind::Wheel)
    } else {
        None
    }
}

/// The overlay's own window (raw `HWND` value) that its mouse inputs are
/// posted to, `0` while the overlay is not armed. The overlay shares the one
/// `WH_MOUSE_LL` hook with the card's click-away: the hook stays installed
/// while either needs it.
static OVERLAY_TARGET: AtomicIsize = AtomicIsize::new(0);

/// Target `HWND` (as its raw pointer value) that clicks are posted to. Set
/// once by [`ClickWatcher::install`] and never mutated afterwards, so no
/// lock is needed to read it from the hook callback.
static TARGET_HWND: OnceLock<isize> = OnceLock::new();

/// Armed/disarmed flag, read on every mouse event system-wide. Kept as a
/// bare atomic (rather than behind the `Mutex` pattern `hotkey.rs` uses for
/// its richer state) precisely so the hot path -- which for `WM_MOUSEMOVE`
/// alone can be enormous -- never blocks.
static ARMED: AtomicBool = AtomicBool::new(false);

/// The installed hook handle (`0` = not installed). The lifecycle logic lives
/// here, apart from the Win32 calls, so it can be unit-tested with fakes.
struct HookSlot {
    handle: AtomicIsize,
}

impl HookSlot {
    const fn new() -> Self {
        Self {
            handle: AtomicIsize::new(0),
        }
    }

    fn is_set(&self) -> bool {
        self.handle.load(Ordering::Relaxed) != 0
    }

    /// Installs once: a second call while installed does nothing. `false`
    /// means the install failed and nothing is installed.
    fn arm(&self, install: impl FnOnce() -> Option<isize>) -> bool {
        if self.is_set() {
            return true;
        }
        match install() {
            Some(h) if h != 0 => {
                self.handle.store(h, Ordering::Relaxed);
                true
            }
            _ => false,
        }
    }

    /// Removes the hook if installed; a no-op otherwise.
    fn disarm(&self, uninstall: impl FnOnce(isize)) {
        let h = self.handle.swap(0, Ordering::Relaxed);
        if h != 0 {
            uninstall(h);
        }
    }
}

static HOOK: HookSlot = HookSlot::new();

fn install_real_hook() -> Option<isize> {
    unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) }
        .ok()
        .map(|h| h.0 as isize)
}

fn uninstall_real_hook(h: isize) {
    // Never panic on the main thread: nothing useful to do about a failure.
    let _ = unsafe { UnhookWindowsHookEx(HHOOK(h as *mut _)) };
}

/// Whether the `WH_MOUSE_LL` hook is in the system's hook chain right now.
/// `false` whenever no card needs click-away (issue #729).
#[allow(dead_code)]
pub fn hook_installed() -> bool {
    HOOK.is_set()
}

/// Removes the hook and stops reporting clicks. Free-function twin of
/// [`ClickWatcher::disarm`] for code that has no handle (the card, when it
/// hides). Must run on the thread that installed the hook.
pub fn disarm() {
    ARMED.store(false, Ordering::Relaxed);
    if OVERLAY_TARGET.load(Ordering::Relaxed) == 0 {
        HOOK.disarm(uninstall_real_hook);
    }
}

/// #809: starts reporting clicks and the wheel to `target` for the overlay,
/// installing the shared hook if the card has not. `false`, with nothing
/// armed, when the hook cannot be installed: an overlay that cannot hear a
/// click must not show.
pub fn arm_overlay(target: HWND) -> bool {
    if !HOOK.arm(install_real_hook) {
        return false;
    }
    OVERLAY_TARGET.store(target.0 as isize, Ordering::Relaxed);
    true
}

/// #809: stops reporting to the overlay, and removes the hook unless the
/// card's click-away still needs it. Safe to call when not armed.
pub fn disarm_overlay() {
    OVERLAY_TARGET.store(0, Ordering::Relaxed);
    if !ARMED.load(Ordering::Relaxed) {
        HOOK.disarm(uninstall_real_hook);
    }
}

/// Handle for the click watcher. The hook itself is installed only between
/// [`arm`](Self::arm) and [`disarm`](Self::disarm). Must be used on the
/// thread running the Win32 message loop.
pub struct ClickWatcher;

impl ClickWatcher {
    /// Records `target`, which receives [`WM_APP_DISMISS`] while armed. Must
    /// be called on the thread that runs the message loop, and only once per
    /// process (a second call fails). Installs no hook yet.
    pub fn install(target: HWND) -> Result<Self> {
        TARGET_HWND
            .set(target.0 as isize)
            .map_err(|_| anyhow::anyhow!("ClickWatcher::install called more than once"))?;
        ARMED.store(false, Ordering::Relaxed);
        Ok(Self)
    }

    /// Install the hook and start reporting clicks. Called when a card that
    /// wants click-away becomes visible. Returns `false` if the hook could not
    /// be installed. Cards with a dismiss timer still close on it; cards with
    /// no timer (errors, "Review needs edits") then close only by their own
    /// controls or Esc, so the caller should record the failure.
    pub fn arm(&self) -> bool {
        let ok = HOOK.arm(install_real_hook);
        if ok {
            ARMED.store(true, Ordering::Relaxed);
        }
        ok
    }

    /// Remove the hook. Called while a request is in flight and whenever the
    /// card is hidden.
    pub fn disarm(&self) {
        disarm();
    }

    /// Not used by the app today; kept because the armed/disarmed gate is
    /// the whole contract of this module and is worth being able to assert.
    #[allow(dead_code)]
    pub fn is_armed(&self) -> bool {
        ARMED.load(Ordering::Relaxed)
    }
}

impl Drop for ClickWatcher {
    fn drop(&mut self) {
        disarm();
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // Per the WH_MOUSE_LL contract: if code < 0, pass through untouched and
    // do not swallow or inspect anything, regardless of what follows.
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let msg = wparam.0 as u32;

    // Fastest possible early return for WM_MOUSEMOVE (and anything else
    // that isn't a button-down or a wheel turn): no atomic load, just
    // integer compares. This message floods the callback, so this branch
    // must stay minimal.
    if !is_button_down(msg) && !is_wheel(msg) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // #809: the overlay hears clicks and the wheel. One atomic load, one post.
    let overlay_target = OVERLAY_TARGET.load(Ordering::Relaxed);
    if let Some(kind) = overlay_report(overlay_target != 0, msg) {
        let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let packed = pack_point(ms.pt.x, ms.pt.y);
        let _ = unsafe {
            PostMessageW(
                Some(HWND(overlay_target as *mut _)),
                crate::ui::overlay::WM_APP_OVERLAY_INPUT,
                WPARAM(kind as usize),
                LPARAM(packed as isize),
            )
        };
    }

    // The card's click-away only wants button-downs.
    if !is_button_down(msg) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // Disarmed: essentially nothing beyond this atomic load.
    if !should_report(ARMED.load(Ordering::Relaxed), msg) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    if let Some(&target_hwnd) = TARGET_HWND.get() {
        let ms = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let packed = pack_point(ms.pt.x, ms.pt.y);
        let hwnd = HWND(target_hwnd as *mut _);
        // Never hold any lock across this call (there is none to hold
        // here, but keep the property explicit: no allocation, no lock,
        // just a post) and never let its result change what we return.
        let _ = unsafe {
            PostMessageW(
                Some(hwnd),
                WM_APP_DISMISS,
                WPARAM(0),
                LPARAM(packed as isize),
            )
        };
    }

    // Never swallow the click: it must still reach whatever the user
    // actually clicked on.
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- pack_point / unpack_point round trip ---------------------------

    #[test]
    fn packs_and_unpacks_positive_point() {
        let packed = pack_point(1920, 1080);
        assert_eq!(unpack_point(packed), (1920, 1080));
    }

    #[test]
    fn packs_and_unpacks_origin() {
        let packed = pack_point(0, 0);
        assert_eq!(unpack_point(packed), (0, 0));
    }

    #[test]
    fn packs_and_unpacks_negative_x() {
        // A monitor placed left of the primary monitor gives negative x.
        let packed = pack_point(-1920, 200);
        assert_eq!(unpack_point(packed), (-1920, 200));
    }

    #[test]
    fn packs_and_unpacks_negative_y() {
        // A monitor placed above the primary monitor gives negative y.
        let packed = pack_point(300, -600);
        assert_eq!(unpack_point(packed), (300, -600));
    }

    #[test]
    fn packs_and_unpacks_both_negative() {
        let packed = pack_point(-100, -50);
        assert_eq!(unpack_point(packed), (-100, -50));
    }

    #[test]
    fn packs_and_unpacks_i16_extremes() {
        let packed = pack_point(i16::MIN as i32, i16::MAX as i32);
        assert_eq!(unpack_point(packed), (i16::MIN as i32, i16::MAX as i32));
    }

    #[test]
    fn packed_value_matches_expected_bit_layout() {
        // -1 as i16 is 0xFFFF; 2 stays 0x0002. Packed low bits are x,
        // high bits are y.
        let packed = pack_point(-1, 2);
        assert_eq!(packed, 0x0002_FFFF);
    }

    // -- armed/disarmed gate logic ---------------------------------------

    #[test]
    fn disarmed_never_reports() {
        assert!(!should_report(false, WM_LBUTTONDOWN));
        assert!(!should_report(false, WM_RBUTTONDOWN));
        assert!(!should_report(false, WM_MBUTTONDOWN));
    }

    #[test]
    fn armed_reports_all_three_buttons() {
        assert!(should_report(true, WM_LBUTTONDOWN));
        assert!(should_report(true, WM_RBUTTONDOWN));
        assert!(should_report(true, WM_MBUTTONDOWN));
    }

    #[test]
    fn armed_ignores_mouse_move() {
        const WM_MOUSEMOVE: u32 = 0x0200;
        assert!(!should_report(true, WM_MOUSEMOVE));
        assert!(!is_button_down(WM_MOUSEMOVE));
    }

    #[test]
    fn armed_ignores_wheel_and_button_up() {
        const WM_LBUTTONUP: u32 = 0x0202;
        const WM_MOUSEWHEEL: u32 = 0x020A;
        assert!(!should_report(true, WM_LBUTTONUP));
        assert!(!should_report(true, WM_MOUSEWHEEL));
    }

    // -- #809: the overlay's share of the hook -----------------------------

    #[test]
    fn the_overlay_hears_clicks_and_the_wheel_only_while_armed() {
        use crate::overlay::wire::Kind;
        const WM_MOUSEMOVE: u32 = 0x0200;
        const WM_LBUTTONUP: u32 = 0x0202;
        assert_eq!(overlay_report(true, WM_LBUTTONDOWN), Some(Kind::Click));
        assert_eq!(overlay_report(true, WM_RBUTTONDOWN), Some(Kind::Click));
        assert_eq!(overlay_report(true, WM_MBUTTONDOWN), Some(Kind::Click));
        assert_eq!(overlay_report(true, WM_MOUSEWHEEL), Some(Kind::Wheel));
        assert_eq!(overlay_report(true, WM_MOUSEHWHEEL), Some(Kind::Wheel));
        assert_eq!(overlay_report(true, WM_MOUSEMOVE), None);
        assert_eq!(overlay_report(true, WM_LBUTTONUP), None);
        for msg in [WM_LBUTTONDOWN, WM_MOUSEWHEEL] {
            assert_eq!(overlay_report(false, msg), None);
        }
    }

    #[test]
    fn the_card_still_ignores_the_wheel() {
        assert!(!should_report(true, WM_MOUSEWHEEL));
        assert!(!should_report(true, WM_MOUSEHWHEEL));
    }

    // -- hook lifecycle (issue #729) ------------------------------------

    use std::cell::Cell;

    #[test]
    fn nothing_is_installed_until_armed() {
        let slot = HookSlot::new();
        assert!(!slot.is_set());
    }

    #[test]
    fn arm_installs_once_and_disarm_removes_it() {
        let slot = HookSlot::new();
        let installs = Cell::new(0);
        let removed = Cell::new(0);
        assert!(slot.arm(|| {
            installs.set(installs.get() + 1);
            Some(7)
        }));
        assert!(slot.is_set());
        // Arming again while installed must not stack a second hook.
        assert!(slot.arm(|| {
            installs.set(installs.get() + 1);
            Some(8)
        }));
        assert_eq!(installs.get(), 1);
        slot.disarm(|h| {
            assert_eq!(h, 7);
            removed.set(removed.get() + 1);
        });
        assert!(!slot.is_set());
        assert_eq!(removed.get(), 1);
    }

    #[test]
    fn disarm_when_nothing_is_installed_does_nothing() {
        let slot = HookSlot::new();
        slot.disarm(|_| panic!("nothing to remove"));
        assert!(!slot.is_set());
    }

    #[test]
    fn a_failed_install_leaves_nothing_installed_and_can_retry() {
        let slot = HookSlot::new();
        assert!(!slot.arm(|| None));
        assert!(!slot.arm(|| Some(0)));
        assert!(!slot.is_set());
        assert!(slot.arm(|| Some(5)));
        assert!(slot.is_set());
    }

    #[test]
    fn can_arm_again_after_a_disarm() {
        let slot = HookSlot::new();
        assert!(slot.arm(|| Some(1)));
        slot.disarm(|_| {});
        assert!(slot.arm(|| Some(2)));
        assert!(slot.is_set());
    }

    /// Live check against the real Win32 hook (ignored by default: it touches
    /// the process-wide hook slot). Run with
    /// `cargo test hook_lifecycle_live -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn hook_lifecycle_live() {
        assert!(!hook_installed(), "no hook before any arm");
        assert!(HOOK.arm(install_real_hook), "SetWindowsHookExW failed");
        assert!(hook_installed());
        disarm();
        assert!(!hook_installed(), "hook must be gone after disarm");
    }

    #[test]
    fn is_button_down_matches_exactly_the_three_messages() {
        assert!(is_button_down(WM_LBUTTONDOWN));
        assert!(is_button_down(WM_RBUTTONDOWN));
        assert!(is_button_down(WM_MBUTTONDOWN));
        assert!(!is_button_down(0));
    }
}
