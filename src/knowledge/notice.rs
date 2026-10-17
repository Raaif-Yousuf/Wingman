//! The knowledge scan's own queue of plain-words notices.
//!
//! These used to share the store's one-per-run slot (`store::report_text`), so a
//! harmless "some files could not be read" at start-up used the slot up and a
//! real store failure later in the run was dropped without a card (#1054). The
//! scan now keeps its own queue, drained into the next result card beside the
//! store's notice (`app::with_pending_notices`). The same text twice in a run is
//! kept once, so a repeat cannot flood a card.

use std::sync::Mutex;

/// Notices waiting for a card, oldest first, without repeats.
#[derive(Debug, Default)]
pub struct Notices {
    queue: Vec<String>,
}

impl Notices {
    pub const fn new() -> Self {
        Notices { queue: Vec::new() }
    }

    /// Adds `text` unless the same text is already waiting.
    pub fn push(&mut self, text: &str) {
        let text = text.trim();
        if !text.is_empty() && !self.queue.iter().any(|t| t == text) {
            self.queue.push(text.to_string());
        }
    }

    /// Everything waiting as one text (paragraphs), emptying the queue.
    pub fn take(&mut self) -> Option<String> {
        if self.queue.is_empty() {
            return None;
        }
        Some(std::mem::take(&mut self.queue).join("\n\n"))
    }
}

static SCAN_NOTICES: Mutex<Notices> = Mutex::new(Notices::new());

/// Queues a scan notice for the next card.
pub fn report(text: &str) {
    SCAN_NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push(text);
}

/// Takes the queued scan notices, if any. `app.rs` adds them to the next card.
pub fn take() -> Option<String> {
    SCAN_NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_different_notices_are_both_kept_in_order() {
        let mut n = Notices::new();
        n.push("first");
        n.push("second");
        assert_eq!(n.take().as_deref(), Some("first\n\nsecond"));
        assert_eq!(n.take(), None, "taking empties the queue");
    }

    #[test]
    fn the_same_notice_twice_is_kept_once() {
        let mut n = Notices::new();
        n.push("3 files in your knowledge folder could not be read.");
        n.push("3 files in your knowledge folder could not be read.");
        assert_eq!(
            n.take().as_deref(),
            Some("3 files in your knowledge folder could not be read.")
        );
    }

    #[test]
    fn a_blank_notice_is_ignored() {
        let mut n = Notices::new();
        n.push("   ");
        assert_eq!(n.take(), None);
    }
}
