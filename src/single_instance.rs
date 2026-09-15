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
use windows::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

/// `Local\` rather than `Global\`: the scope is the logged-in session, so two
/// different users on the same machine each get their own instance, which is
/// what they would expect — the config and the tray icon are per-user too.
const MUTEX_NAME: windows::core::PCWSTR = w!("Local\\CopilotAsk.SingleInstance.4d1b62f0");

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
    unsafe {
        match CreateMutexW(None, true, MUTEX_NAME) {
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

/// Hand focus to the instance that is already running.
///
/// Called just before a duplicate exits. Silently vanishing would look like
/// the app failed to start, so the running instance opens its settings window
/// instead — the same thing a left-click on the tray icon does, which is the
/// most likely reason someone launched it a second time.
pub fn poke_existing() {
    use windows::Win32::UI::WindowsAndMessaging::{FindWindowW, PostMessageW, WM_LBUTTONUP};

    unsafe {
        let Ok(hwnd) = FindWindowW(w!("CopilotAsk.Owner.Window.4d1b62f0"), None) else {
            return;
        };
        if hwnd.0.is_null() {
            return;
        }
        let _ = PostMessageW(
            Some(hwnd),
            crate::ui::tray::WM_APP_TRAY,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(WM_LBUTTONUP as isize),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_first_caller_wins_and_a_second_is_refused() {
        let first = acquire();
        assert!(
            matches!(first, Instance::First(_)),
            "nothing else should hold the name in a fresh test process"
        );

        // A second attempt while the first lock is alive must be refused --
        // this is the whole point of the module.
        assert!(matches!(acquire(), Instance::Already));

        drop(first);

        // Once released the name is claimable again, so quitting and
        // relaunching works rather than locking the user out until reboot.
        assert!(matches!(acquire(), Instance::First(_)));
    }
}
