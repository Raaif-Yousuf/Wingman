//! One tray icon, one keyboard hook, one API call per keypress.
//!
//! Without this guard a second launch is completely silent and completely
//! wrong: two tray icons, two low-level keyboard hooks both matching the
//! Copilot key, and two billed API calls for every press. Nothing in the UI
//! hints at it, because each instance looks perfectly healthy from the inside.
//! "Start with Windows" makes it easy to hit — autostart launches one, then
//! clicking the exe launches another.
//!
//! A named mutex is the standard mechanism: the kernel owns the name, so it is
//! released even if the process is killed with the task manager, unlike a lock
//! file that would need cleaning up after a crash.

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE, HWND};
use windows::Win32::System::Threading::CreateMutexW;

/// `Local\` rather than `Global\`: the scope is the logged-in session, so two
/// different users on the same machine each get their own instance, which is
/// what they would expect — the config and the tray icon are per-user too.
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\Wingman.SingleInstance.4d1b62f0");

/// Held for the lifetime of the process. Dropping it releases the name.
pub struct InstanceLock(HANDLE);

impl Drop for InstanceLock {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Outcome of trying to become *the* instance.
pub enum Instance {
    /// This process owns the name; carry on. Keep the lock alive.
    First(InstanceLock),
    /// Another instance is already running; this process should exit.
    Already,
}

/// Try to claim the single-instance name.
///
/// A failure to create the mutex at all (which should not happen) is treated
/// as `First`: refusing to start because a lock could not be taken would be a
/// worse failure than the duplicate it is guarding against.
pub fn acquire() -> Instance {
    acquire_named(MUTEX_NAME)
}

/// The mechanism, with the name injected.
///
/// Split out so the test can claim a name of its own. Testing against
/// `MUTEX_NAME` would mean `cargo test` passes or fails depending on whether
/// the real app happens to be running -- which is exactly what it did before.
fn acquire_named(name: windows::core::PCWSTR) -> Instance {
    unsafe {
        match CreateMutexW(None, true, name) {
            Ok(handle) => {
                // CreateMutexW succeeds and returns a handle to the EXISTING
                // mutex when the name is taken, so the error code is the only
                // thing that distinguishes the two cases.
                if windows::Win32::Foundation::GetLastError() == ERROR_ALREADY_EXISTS {
                    let _ = CloseHandle(handle);
                    Instance::Already
                } else {
                    Instance::First(InstanceLock(handle))
                }
            }
            Err(_) => Instance::First(InstanceLock(HANDLE::default())),
        }
    }
}

/// What a duplicate launch should make the running instance do.
///
/// The Windows shell launches this app by AUMID with no command line at all
/// when the Copilot key is pressed, so `Ask` is the default every hardware
/// press produces; `Settings` exists only for a launch that deliberately
/// asks for it.
pub enum Activation {
    /// Run the same request a hotkey press or "Ask now" would.
    Ask,
    /// Open the Settings window, as a left-click on the tray icon does.
    Settings,
}

/// How a duplicate launch ended up (#237).
pub enum Startup {
    /// A running instance was found and told what to do; this process
    /// should exit.
    Delivered,
    /// The previous holder went away while we waited, so this process took
    /// the name and is now the instance.
    Locked(InstanceLock),
    /// The name is held and no ready window ever answered: the holder is not
    /// Wingman. Start anyway, without the lock (see [`mark_lock_squatted`]).
    Squatted,
}

/// How long a duplicate keeps looking for the running instance (absorbs a
/// genuine instance that is mid-start or mid-exit), and how often.
const POKE_RETRY_ATTEMPTS: u32 = 40;
const POKE_RETRY_WAIT_MS: u64 = 50;

/// The retry shape, with the window search, the lock attempt, the wait and
/// the delivery injected so every outcome is testable without a window.
/// Each round looks for a ready instance first, then tries the name again,
/// so an instance that is exiting (window gone, name not yet released)
/// ends in a normal locked start rather than a lockless one.
fn settle_with<T, L>(
    attempts: u32,
    mut find: impl FnMut() -> Option<T>,
    mut try_lock: impl FnMut() -> Option<L>,
    mut wait: impl FnMut(),
    deliver: impl FnOnce(T),
) -> Settled<L> {
    for attempt in 0..attempts {
        if attempt > 0 {
            wait();
        }
        if let Some(found) = find() {
            deliver(found);
            return Settled::Delivered;
        }
        if let Some(lock) = try_lock() {
            return Settled::Locked(lock);
        }
    }
    Settled::Squatted
}

/// [`Startup`] before the lock type is fixed, so tests need no real mutex.
#[derive(Debug, PartialEq, Eq)]
enum Settled<L> {
    Delivered,
    Locked(L),
    Squatted,
}

/// Called by a duplicate after `acquire` said the name is held. Hands
/// focus to the instance that is already running, or, when none ever
/// answers, tells the caller how to carry on.
///
/// Silently vanishing would look like the app failed to start, so the
/// running instance is told what to do instead. Only a window that has
/// marked itself ready ([`mark_ready`]) counts: a duplicate's own window,
/// or a half-started instance's, has no `App` behind it and would drop the
/// message.
pub fn settle_duplicate(activation: Activation) -> Startup {
    match settle_with(
        POKE_RETRY_ATTEMPTS,
        || find_ready_window(w!("Wingman.Owner.Window.4d1b62f0"), READY_PROP, None),
        || match acquire() {
            Instance::First(lock) => Some(lock),
            Instance::Already => None,
        },
        || std::thread::sleep(std::time::Duration::from_millis(POKE_RETRY_WAIT_MS)),
        |hwnd| post_activation(hwnd, activation),
    ) {
        Settled::Delivered => Startup::Delivered,
        Settled::Locked(l) => Startup::Locked(l),
        Settled::Squatted => Startup::Squatted,
    }
}

/// The window property an owner window carries once its `App` is installed.
/// A property lives on one window, not in a shared namespace, so this needs
/// no test-only variant for the real name; the tests still use their own.
const READY_PROP: windows::core::PCWSTR = w!("Wingman.Ready");

/// Marks `hwnd` as ready to receive activations. Call only after the
/// window's `App` is installed.
pub fn mark_ready(hwnd: HWND) {
    mark_ready_prop(hwnd, READY_PROP);
}

fn mark_ready_prop(hwnd: HWND, prop: windows::core::PCWSTR) {
    use windows::Win32::UI::WindowsAndMessaging::SetPropW;
    unsafe {
        let _ = SetPropW(hwnd, prop, Some(HANDLE(1 as *mut _)));
    }
}

/// The first top-level window of `class` that carries `prop` and is not
/// `skip`.
fn find_ready_window(
    class: windows::core::PCWSTR,
    prop: windows::core::PCWSTR,
    skip: Option<HWND>,
) -> Option<HWND> {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowExW, GetPropW};

    let mut after: Option<HWND> = None;
    loop {
        let found = unsafe { FindWindowExW(None, after, class, None) }
            .ok()
            .filter(|h| !h.0.is_null())?;
        let ready = !unsafe { GetPropW(found, prop) }.0.is_null();
        if ready && Some(found) != skip {
            return Some(found);
        }
        after = Some(found);
    }
}

fn post_activation(hwnd: HWND, activation: Activation) {
    use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_LBUTTONUP};

    unsafe {
        match activation {
            // Reuses the tray's own left-click message rather than calling
            // into app.rs directly, so this stays byte-for-byte what a real
            // left-click already does -- no new code path to keep in sync.
            Activation::Settings => {
                let _ = PostMessageW(
                    Some(hwnd),
                    crate::ui::tray::WM_APP_TRAY,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(WM_LBUTTONUP as isize),
                );
            }
            // A dedicated message rather than a spoofed tray click: unlike
            // Settings, "ask" is not standing in for some other UI gesture,
            // it is the primary thing a Copilot-key press means now.
            Activation::Ask => {
                let _ = PostMessageW(
                    Some(hwnd),
                    crate::app::WM_APP_ACTIVATE,
                    windows::Win32::Foundation::WPARAM(0),
                    windows::Win32::Foundation::LPARAM(0),
                );
            }
        }
    }
}

static LOCK_SQUATTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Records that the name was held by something that is not Wingman, so
/// this process started without the lock. Shown in the diagnostics report.
pub fn mark_lock_squatted() {
    LOCK_SQUATTED.store(true, std::sync::atomic::Ordering::Relaxed);
}

/// Whether this process started without the lock (see [`mark_lock_squatted`]).
pub fn lock_squatted() -> bool {
    LOCK_SQUATTED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Decide what a duplicate launch means from its `argv`, without touching
/// Win32 so it can be unit-tested without a window to post to.
///
/// argv[0] is the program path, not a flag, so it is always skipped -- a
/// program installed at a path literally containing `--settings` must not be
/// mistaken for the flag.
pub fn activation_from_args<I: IntoIterator<Item = String>>(args: I) -> Activation {
    if args.into_iter().skip(1).any(|a| a == "--settings") {
        Activation::Settings
    } else {
        Activation::Ask
    }
}

/// What a FIRST launch (nothing else running yet, i.e. `Instance::First`)
/// should do once its own window exists.
///
/// Deliberately not [`Activation`]: on this path "no flag" means "start
/// normally and do nothing more" rather than "ask", because every bare
/// first launch -- the Copilot key, the Start Menu entry, autostart at
/// login -- must not ask unasked (AGENTS.md: "a bare launch must not ask").
/// Only an explicit `--settings`, as `install.ps1` passes after a fresh
/// install, opens Settings.
#[derive(Debug, PartialEq, Eq)]
pub enum FirstLaunchAction {
    /// Start normally; every bare first launch takes this branch.
    None,
    /// Open Settings once the window exists.
    OpenSettings,
}

/// Decide what a first (non-duplicate) launch's argv means, without
/// touching Win32 so it can be unit-tested without a window to open
/// Settings on. Shares [`activation_from_args`]'s parsing so the flag can
/// never drift between the two paths.
pub fn first_launch_action<I: IntoIterator<Item = String>>(args: I) -> FirstLaunchAction {
    match activation_from_args(args) {
        Activation::Settings => FirstLaunchAction::OpenSettings,
        Activation::Ask => FirstLaunchAction::None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A name only this test uses, so the result does not depend on whether
    /// the real app is running on the machine running the tests.
    const TEST_NAME: windows::core::PCWSTR = w!("Local\\Wingman.SingleInstance.test.9f2c");

    #[test]
    fn the_first_caller_wins_and_a_second_is_refused() {
        let first = acquire_named(TEST_NAME);
        assert!(
            matches!(first, Instance::First(_)),
            "the test's own name should be free"
        );

        // A second attempt while the first lock is alive must be refused --
        // this is the whole point of the module.
        assert!(matches!(acquire_named(TEST_NAME), Instance::Already));

        drop(first);

        // Once released the name is claimable again, so quitting and
        // relaunching works rather than locking the user out until reboot.
        assert!(matches!(acquire_named(TEST_NAME), Instance::First(_)));
    }

    #[test]
    fn the_production_name_is_not_what_the_test_locks() {
        // Guards against someone "simplifying" the test back onto MUTEX_NAME,
        // which would make the suite fail whenever the app is running.
        assert!(!std::ptr::eq(TEST_NAME.0, MUTEX_NAME.0));
    }

    // -- #237: a duplicate that cannot find the running instance ------------

    #[test]
    fn an_instance_found_at_once_is_delivered_without_waiting() {
        let mut waits = 0;
        let mut delivered = None;
        let out = settle_with(
            5,
            || Some(7),
            || None::<u8>,
            || waits += 1,
            |h| delivered = Some(h),
        );
        assert_eq!(out, Settled::Delivered);
        assert_eq!(waits, 0);
        assert_eq!(delivered, Some(7));
    }

    #[test]
    fn an_instance_that_becomes_ready_on_retry_is_delivered() {
        let mut tries = 0;
        let mut waits = 0;
        let mut delivered = None;
        let out = settle_with(
            5,
            || {
                tries += 1;
                (tries == 3).then_some(9)
            },
            || None::<u8>,
            || waits += 1,
            |h| delivered = Some(h),
        );
        assert_eq!(out, Settled::Delivered);
        assert_eq!(waits, 2);
        assert_eq!(delivered, Some(9));
    }

    #[test]
    fn an_instance_that_exits_during_the_wait_gives_us_the_lock() {
        // Window gone, name released on the third look: a normal start.
        let mut looks = 0;
        let mut delivered = false;
        let out = settle_with(
            5,
            || None::<u8>,
            || {
                looks += 1;
                (looks == 3).then_some("lock")
            },
            || {},
            |_| delivered = true,
        );
        assert_eq!(out, Settled::Locked("lock"));
        assert!(!delivered);
    }

    #[test]
    fn a_holder_that_never_shows_a_window_is_a_squatter_after_the_bound() {
        let mut tries = 0;
        let mut delivered = false;
        let out = settle_with(
            5,
            || {
                tries += 1;
                None::<u8>
            },
            || None::<u8>,
            || {},
            |_| delivered = true,
        );
        assert_eq!(out, Settled::Squatted);
        assert_eq!(tries, 5);
        assert!(!delivered);
    }

    #[test]
    fn the_retry_window_is_about_two_seconds() {
        assert_eq!(POKE_RETRY_ATTEMPTS as u64 * POKE_RETRY_WAIT_MS, 2000);
    }

    /// A real window property round trip on two stock `STATIC` windows with
    /// a property name only this test uses: the unmarked one is never
    /// found, the marked one is.
    #[test]
    fn only_a_window_marked_ready_is_found() {
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DestroyWindow, WINDOW_EX_STYLE, WS_OVERLAPPED,
        };
        const TEST_PROP: windows::core::PCWSTR = w!("Wingman.Ready.test.9f2c");
        let make = || unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                w!("STATIC"),
                w!("wingman-ready-test"),
                WS_OVERLAPPED,
                0,
                0,
                0,
                0,
                None,
                None,
                None,
                None,
            )
            .expect("a stock STATIC window")
        };
        let unmarked = make();
        assert_eq!(find_ready_window(w!("STATIC"), TEST_PROP, None), None);
        let marked = make();
        mark_ready_prop(marked, TEST_PROP);
        assert_eq!(
            find_ready_window(w!("STATIC"), TEST_PROP, None),
            Some(marked)
        );
        assert_eq!(
            find_ready_window(w!("STATIC"), TEST_PROP, Some(marked)),
            None
        );
        unsafe {
            let _ = DestroyWindow(unmarked);
            let _ = DestroyWindow(marked);
        }
    }

    fn args(strs: &[&str]) -> Vec<String> {
        strs.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_args_means_ask() {
        // The Copilot key gives the shell no way to pass a command line, so
        // this is the case that has to happen every time the hardware key is
        // pressed while the app is already running.
        assert!(matches!(
            activation_from_args(args(&["wingman.exe"])),
            Activation::Ask
        ));
    }

    #[test]
    fn the_settings_flag_means_settings() {
        assert!(matches!(
            activation_from_args(args(&["wingman.exe", "--settings"])),
            Activation::Settings
        ));
    }

    #[test]
    fn an_unrelated_arg_means_ask() {
        // Anything other than the exact flag falls back to the Copilot-key
        // behaviour, rather than silently opening Settings for a typo.
        assert!(matches!(
            activation_from_args(args(&["wingman.exe", "--frobnicate"])),
            Activation::Ask
        ));
    }

    #[test]
    fn argv0_named_like_the_flag_is_not_mistaken_for_it() {
        // argv[0] is the program path, chosen by whoever launches the
        // process, not an argument the user typed -- it must never be read
        // as a flag.
        assert!(matches!(
            activation_from_args(args(&["--settings"])),
            Activation::Ask
        ));
    }

    // -- issue #149: what a FIRST launch (nothing else running yet) should do
    // once its own window exists --------------------------------------------

    #[test]
    fn a_bare_first_launch_does_nothing() {
        // The Copilot key, the Start Menu entry and autostart at login all
        // activate the exe with no arguments, and none of them may open
        // Settings unasked (AGENTS.md: "a bare launch must not ask").
        assert!(matches!(
            first_launch_action(args(&["wingman.exe"])),
            FirstLaunchAction::None
        ));
    }

    #[test]
    fn a_first_launch_with_settings_flag_opens_settings() {
        // What install.ps1's post-install step relies on: a fresh install's
        // *first* process, not a duplicate poking an already-running one.
        assert!(matches!(
            first_launch_action(args(&["wingman.exe", "--settings"])),
            FirstLaunchAction::OpenSettings
        ));
    }

    #[test]
    fn a_first_launch_with_an_unrelated_flag_does_nothing() {
        assert!(matches!(
            first_launch_action(args(&["wingman.exe", "--frobnicate"])),
            FirstLaunchAction::None
        ));
    }

    #[test]
    fn first_launch_argv0_named_like_the_flag_is_not_mistaken_for_it() {
        assert!(matches!(
            first_launch_action(args(&["--settings"])),
            FirstLaunchAction::None
        ));
    }
}
