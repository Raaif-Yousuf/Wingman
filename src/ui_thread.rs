//! Guard (#700): blocking network calls must never run on the UI thread.
//!
//! The UI thread pumps the message loop and hosts the low-level keyboard hook
//! (`hotkey.rs`). A blocking HTTP call there stalls system-wide typing past the
//! hook timeout (about 300 ms) and Windows can drop the hook.
//!
//! [`mark_current_thread`] is called by the thread that pumps messages.
//! [`blocking_call`] is called at every blocking network entry point; on a
//! marked thread it panics in a debug build and, in a release build (where a
//! panic aborts the process), leaves one line in the diagnostics report.
//!
//! The mark is per thread, so tests that call the network helpers from an
//! ordinary test thread are unaffected.

use std::cell::Cell;
use std::sync::atomic::{AtomicBool, Ordering};

thread_local! {
    static IS_UI_THREAD: Cell<bool> = const { Cell::new(false) };
}

/// Release builds warn once per process, not once per call.
static WARNED: AtomicBool = AtomicBool::new(false);

/// Marks the calling thread as the UI thread (the one that pumps messages).
pub fn mark_current_thread() {
    IS_UI_THREAD.with(|f| f.set(true));
}

/// Whether the calling thread is marked as the UI thread.
pub fn is_ui_thread() -> bool {
    IS_UI_THREAD.with(Cell::get)
}

/// Call at the top of a blocking network entry point. `what` names the call
/// (a tag or function name, never a URL or a key).
pub fn blocking_call(what: &str) {
    if !is_ui_thread() {
        return;
    }
    debug_assert!(
        false,
        "blocking network call `{what}` on the UI thread: it stalls the keyboard hook"
    );
    if !WARNED.swap(true, Ordering::Relaxed) {
        crate::diagnostics::note_warning(&format!(
            "A blocking network call ({what}) ran on the UI thread; it can stall the keyboard hook"
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ordinary_thread_is_not_the_ui_thread() {
        std::thread::spawn(|| {
            assert!(!is_ui_thread());
            blocking_call("test");
        })
        .join()
        .expect("an unmarked thread must pass the check");
    }

    #[test]
    fn a_marked_thread_fires_the_check() {
        let result = std::thread::spawn(|| {
            mark_current_thread();
            assert!(is_ui_thread());
            blocking_call("test");
        })
        .join();
        assert!(
            result.is_err(),
            "debug builds must panic on a marked thread"
        );
    }

    /// Every blocking entry point named in #700 carries the check. Scans the
    /// source (CRLF normalised) and requires a non-empty set of sites.
    #[test]
    fn every_blocking_entry_point_calls_the_check() {
        let sources = [
            ("src/provider/common.rs", include_str!("provider/common.rs")),
            ("src/connectors/oauth.rs", include_str!("connectors/oauth.rs")),
            ("src/inputs/web_page.rs", include_str!("inputs/web_page.rs")),
        ];
        let mut found = 0;
        for (name, text) in sources {
            let text = text.replace('\r', "");
            let n = text.matches("ui_thread::blocking_call(").count();
            assert!(n > 0, "{name} has no ui_thread::blocking_call");
            found += n;
        }
        assert!(found >= 8, "expected the known entry points, found {found}");
    }
}
