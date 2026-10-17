//! Carrying a finished detection from the worker thread to the main thread.
//! The worker has the OCR result; only the main thread may draw (it owns the
//! window and the hooks). The run finishes through the ordinary
//! `WM_APP_RESULT` path, which carries a plain answer, so the annotations wait
//! here, keyed by the run that made them. Taking them for any other run
//! drops them: a stale result must never draw marks for a screen that moved on.

use std::sync::Mutex;

use crate::capture::ScreenRect;

use super::annotation::Annotation;

/// What one finished run wants drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ready {
    /// The run (`App::run_generation`) that made it.
    pub run: u64,
    pub annotations: Vec<Annotation>,
    /// The monitor the capture covered: labels stay inside it.
    pub monitor: ScreenRect,
}

static SLOT: Mutex<Option<Ready>> = Mutex::new(None);

fn slot() -> std::sync::MutexGuard<'static, Option<Ready>> {
    // A panic elsewhere must not leave the slot unusable; the data is a plain
    // value, so a poisoned lock is still safe to read.
    SLOT.lock().unwrap_or_else(|e| e.into_inner())
}

/// Leaves `ready` for the main thread, replacing anything still waiting.
pub fn put(ready: Ready) {
    *slot() = Some(ready);
}

/// What run `run` left, once. Anything left by another run is dropped.
pub fn take(run: u64) -> Option<Ready> {
    match slot().take() {
        Some(r) if r.run == run => Some(r),
        _ => None,
    }
}

/// Drops whatever is waiting (a new run starts, or the app is quitting).
pub fn clear() {
    *slot() = None;
}

#[cfg(test)]
pub(crate) static TEST_LOCK: Mutex<()> = Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn ready(run: u64) -> Ready {
        Ready {
            run,
            annotations: Vec::new(),
            monitor: ScreenRect {
                left: 0,
                top: 0,
                right: 10,
                bottom: 10,
            },
        }
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn a_run_takes_what_it_left_once() {
        let _g = lock();
        clear();
        put(ready(7));
        assert_eq!(take(7), Some(ready(7)));
        assert_eq!(take(7), None, "taken once");
    }

    #[test]
    fn another_runs_result_is_dropped_not_drawn() {
        let _g = lock();
        clear();
        put(ready(7));
        assert_eq!(take(8), None);
        assert_eq!(take(7), None, "the stale entry is gone too");
    }

    #[test]
    fn a_newer_result_replaces_an_unclaimed_one() {
        let _g = lock();
        clear();
        put(ready(1));
        put(ready(2));
        assert_eq!(take(1), None);
        put(ready(2));
        assert_eq!(take(2), Some(ready(2)));
    }

    #[test]
    fn clear_empties_the_slot() {
        let _g = lock();
        put(ready(3));
        clear();
        assert_eq!(take(3), None);
    }
}
