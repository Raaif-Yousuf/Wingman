//! Locks shared by tests in different modules.

use std::sync::{Mutex, MutexGuard};

static SHELL_QUEUE: Mutex<()> = Mutex::new(());

/// Held by every test that reads or writes the app's process-wide queue of
/// shell-open failures (`crate::app::queued_shell_open_titles` and friends),
/// so no other test's entries or `take` can change what it sees.
pub(crate) fn lock_shell_queue() -> MutexGuard<'static, ()> {
    SHELL_QUEUE.lock().unwrap_or_else(|e| e.into_inner())
}
