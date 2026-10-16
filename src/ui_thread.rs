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

    /// Only debug builds panic; release builds leave a diagnostics line.
    #[cfg(debug_assertions)]
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

    /// The text of each `fn` whose signature starts with `sig`, up to the next
    /// item. Source is CRLF normalised.
    fn bodies<'a>(text: &'a str, sig: &str) -> Vec<&'a str> {
        let ends = [
            "\n    fn ",
            "\nfn ",
            "\npub fn ",
            "\npub(crate) fn ",
            "\n    pub fn ",
            "\nimpl ",
            "\n    #[test]",
        ];
        text.match_indices(sig)
            .map(|(at, _)| {
                let rest = &text[at + sig.len()..];
                let end = ends.iter().filter_map(|e| rest.find(e)).min();
                &rest[..end.unwrap_or(rest.len())]
            })
            .collect()
    }

    /// Every blocking entry point named in #700 carries the check inside its
    /// own function body (not just somewhere in the file).
    #[test]
    fn every_blocking_entry_point_calls_the_check() {
        let common = include_str!("provider/common.rs").replace('\r', "");
        let oauth = include_str!("connectors/oauth.rs").replace('\r', "");
        let page = include_str!("inputs/web_page.rs").replace('\r', "");
        let app = include_str!("app.rs").replace('\r', "");
        let entry_points: [(&str, &str, &str); 9] = [
            ("common", "fn post_json(", "UreqTransport"),
            ("common", "pub(crate) fn get_text_with_timeout(", ""),
            ("common", "fn get_with_headers_raw(", ""),
            ("common", "pub(crate) fn post_json_read_body<", ""),
            (
                "common",
                "pub(crate) fn post_json_with_connect_timeout(",
                "",
            ),
            ("oauth", "fn send_form(", ""),
            ("oauth", "pub fn authorize(", ""),
            (
                "page",
                "fn resolve(&self, host: &str, timeout: Duration)",
                "",
            ),
            ("page", "let pinned = PinnedResolver(pinned);", ""),
        ];
        let mut checked = 0;
        for (file, sig, _) in entry_points {
            let text = match file {
                "common" => &common,
                "oauth" => &oauth,
                _ => &page,
            };
            let hit = if sig.starts_with("let ") {
                // Inside the page fetch: the check sits right above this line.
                let at = text.find(sig).expect(sig);
                text[..at]
                    .rsplit('\n')
                    .take(3)
                    .any(|l| l.contains("ui_thread::blocking_call("))
            } else {
                bodies(text, sig)
                    .iter()
                    .any(|b| b.contains("ui_thread::blocking_call("))
            };
            assert!(hit, "{sig} has no ui_thread::blocking_call in its body");
            checked += 1;
        }
        assert_eq!(checked, 9);
        // The thread is marked before the first window exists.
        let run = bodies(&app, "pub fn run() -> Result<()> {");
        assert!(
            run.iter()
                .any(|b| b.find("mark_current_thread()") < b.find("create_owner_window(")),
            "run() must mark the UI thread before it creates the owner window"
        );
    }
}
