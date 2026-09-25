//! The `"clipboard"` executor (#31): the first real read-only executor.
//! Copies the confirmed proposal's `"text"` string field to the clipboard,
//! recording whatever was there before so [`Undo::undo`] restores it.
//!
//! No built-in action names `"clipboard"` as its executor yet (no
//! `text_answer`-shaped action exists) -- this is forward wiring for the
//! first one that does, the same status the action-model design doc gives
//! an unregistered proposal kind.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::ui::confirm::Confirmed;

use super::{Effect, Executor, Undo};

/// Clipboard access, abstracted so tests never touch the real OS clipboard
/// (the same failure shape rule 9 warns about for named kernel objects: a
/// test that reads or writes a real shared resource is flaky and can
/// clobber whatever the developer had copied).
// Unused outside tests until the confirm-card issue calls a resolved
// executor for real -- see `executors::mod`'s `Effect` doc comment.
#[allow(dead_code)]
pub trait ClipboardAccess: Send + Sync {
    fn get_text(&self) -> Result<String>;
    fn set_text(&self, text: &str) -> Result<()>;
    /// True if the OS clipboard holds data in any format, regardless of
    /// whether `get_text` can read it back as text. The only use: telling
    /// "the clipboard was empty" apart from "the clipboard held something
    /// this executor cannot preserve" before overwriting it (#410).
    fn has_any_content(&self) -> bool;
}

/// The real clipboard, via the crate's existing `arboard` dependency
/// (already used by `app.rs` for "copy diagnostics"). `pub(crate)`, not
/// private: `ClipboardExecutor::new()`'s return type names it as the
/// default type parameter, and that type must be nameable from every module
/// that calls `new()` (e.g. `registry.rs`), not just from inside this file.
#[allow(dead_code)]
pub(crate) struct ArboardClipboard;

#[allow(dead_code)]
impl ClipboardAccess for ArboardClipboard {
    fn get_text(&self) -> Result<String> {
        arboard::Clipboard::new()
            .and_then(|mut c| c.get_text())
            .context("could not read the clipboard")
    }

    fn set_text(&self, text: &str) -> Result<()> {
        arboard::Clipboard::new()
            .and_then(|mut c| c.set_text(text))
            .context("could not write the clipboard")
    }

    fn has_any_content(&self) -> bool {
        super::format_probe::any_clipboard_format_present()
    }
}

/// `Arc`-wrapped so `execute`'s undo closure (`'static`, runs after
/// `execute` returns) can hold its own handle to the same clipboard the
/// executor was constructed with, real or injected -- undo must restore
/// through the same clipboard it wrote to, or a test's fake would silently
/// fall back to the real OS clipboard on undo.
#[allow(dead_code)]
pub struct ClipboardExecutor<C: ClipboardAccess = ArboardClipboard> {
    clipboard: Arc<C>,
}

#[allow(dead_code)]
impl ClipboardExecutor<ArboardClipboard> {
    pub fn new() -> Self {
        Self {
            clipboard: Arc::new(ArboardClipboard),
        }
    }
}

impl Default for ClipboardExecutor<ArboardClipboard> {
    fn default() -> Self {
        Self::new()
    }
}

#[allow(dead_code)]
impl<C: ClipboardAccess> ClipboardExecutor<C> {
    pub fn with_clipboard(clipboard: C) -> Self {
        Self {
            clipboard: Arc::new(clipboard),
        }
    }
}

#[allow(dead_code)]
impl<C: ClipboardAccess + 'static> Executor for ClipboardExecutor<C> {
    fn name(&self) -> &'static str {
        "clipboard"
    }

    /// `ReadOnly` is intentional, not an oversight: see `executors::Effect`'s
    /// doc comment ("Decided (issue #402)") for why a clipboard-writing
    /// executor still counts as read-only for the confirm fast path.
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    fn execute(&self, confirmed: Confirmed<serde_json::Value>) -> Result<Undo> {
        let value = confirmed.into_value();
        let text = value
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let Some(text) = text else {
            bail!("clipboard executor: proposal has no \"text\" field to copy");
        };

        // `get_text` returning nothing is ambiguous on its own (arboard's
        // `ContentNotAvailable` covers both "empty" and "present but not
        // text"); the format probe (#410) resolves it before we overwrite
        // anything, so an unrestorable prior value is reported, not
        // silently dropped.
        let previous = self.clipboard.get_text().ok();
        let lost_unrestorable_content = previous.is_none() && self.clipboard.has_any_content();

        self.clipboard.set_text(&text)?;

        let summary = if lost_unrestorable_content {
            format!(
                "copied \"{text}\" to the clipboard. Its previous contents were in a format that could not be preserved and cannot be restored."
            )
        } else {
            format!("copied \"{text}\" to the clipboard")
        };

        let clipboard = Arc::clone(&self.clipboard);
        Ok(Undo::recording(summary, move || {
            if let Some(previous) = previous {
                clipboard.set_text(&previous)?;
            } else if lost_unrestorable_content {
                bail!(
                    "cannot undo the clipboard write: its previous contents were not text and could not be preserved"
                );
            }
            Ok(())
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::ui::confirm::Proposal;

    #[derive(Default)]
    struct FakeClipboard {
        text: RefCell<Option<String>>,
        /// Simulates a clipboard holding non-text content (e.g. an image):
        /// `get_text` still fails, but `has_any_content` must say `true`.
        holding_unrestorable_content: bool,
    }

    impl FakeClipboard {
        fn seeded(initial: &str) -> Self {
            Self {
                text: RefCell::new(Some(initial.to_string())),
                holding_unrestorable_content: false,
            }
        }

        fn holding_unrestorable_content() -> Self {
            Self {
                text: RefCell::new(None),
                holding_unrestorable_content: true,
            }
        }
    }

    // `RefCell` is not `Sync`; tests run single-threaded per test, and
    // `ClipboardAccess: Send + Sync` only needs to hold for the real
    // production type used across threads. A fake used purely within one
    // test does not need to be thread-safe -- promise the compiler it is,
    // since nothing here actually shares it across threads.
    unsafe impl Sync for FakeClipboard {}

    impl ClipboardAccess for FakeClipboard {
        fn get_text(&self) -> Result<String> {
            self.text
                .borrow()
                .clone()
                .ok_or_else(|| anyhow::anyhow!("fake clipboard is empty"))
        }

        fn set_text(&self, text: &str) -> Result<()> {
            *self.text.borrow_mut() = Some(text.to_string());
            Ok(())
        }

        fn has_any_content(&self) -> bool {
            self.text.borrow().is_some() || self.holding_unrestorable_content
        }
    }

    fn confirmed_with(
        value: serde_json::Value,
        executor: &ClipboardExecutor<FakeClipboard>,
    ) -> Confirmed<serde_json::Value> {
        crate::ui::confirm::auto_confirm_read_only(executor, Proposal::new(value)).unwrap()
    }

    #[test]
    fn clipboard_executor_is_read_only() {
        assert_eq!(ClipboardExecutor::new().effect(), Effect::ReadOnly);
    }

    /// Issue #402: a clipboard-overwriting executor still auto-confirming
    /// is a deliberate decision (see `executors::Effect`'s doc comment), not
    /// a gap. This test is the regression guard for that decision: if
    /// `Effect::ReadOnly` on `ClipboardExecutor` ever gets narrowed back to
    /// requiring a real confirmation, this is the assertion that must be
    /// updated (and the mod.rs doc comment with it) rather than one that
    /// silently starts failing.
    #[test]
    fn clipboard_executor_auto_confirms_by_design_per_issue_402() {
        let executor = ClipboardExecutor::with_clipboard(FakeClipboard::default());
        let proposal = crate::ui::confirm::Proposal::new(serde_json::json!({"text": "x"}));
        crate::ui::confirm::auto_confirm_read_only(&executor, proposal)
            .expect("clipboard executor must auto-confirm: issue #402 decided this is intended");
    }

    #[test]
    fn clipboard_executor_copies_the_text_field_to_the_clipboard() {
        let executor = ClipboardExecutor::with_clipboard(FakeClipboard::seeded("previous"));
        let confirmed = confirmed_with(serde_json::json!({"text": "new text"}), &executor);

        executor.execute(confirmed).unwrap();

        assert_eq!(executor.clipboard.get_text().unwrap(), "new text");
    }

    #[test]
    fn clipboard_executor_errors_when_text_field_is_missing() {
        let executor = ClipboardExecutor::with_clipboard(FakeClipboard::default());
        let confirmed = confirmed_with(
            serde_json::json!({"headline": "no text field here"}),
            &executor,
        );
        let err = executor
            .execute(confirmed)
            .err()
            .expect("a missing \"text\" field must be an error");
        assert!(err.to_string().contains("\"text\""));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn undo_restores_the_previous_clipboard_text() {
        let executor = ClipboardExecutor::with_clipboard(FakeClipboard::seeded("previous text"));
        let confirmed = confirmed_with(serde_json::json!({"text": "new text"}), &executor);

        let undo = executor.execute(confirmed).unwrap();
        assert_eq!(undo.summary, "copied \"new text\" to the clipboard");
        assert_eq!(executor.clipboard.get_text().unwrap(), "new text");

        undo.undo().unwrap();
        assert_eq!(
            executor.clipboard.get_text().unwrap(),
            "previous text",
            "undo must restore what was on the clipboard before execute ran"
        );
    }

    #[test]
    fn undo_is_a_no_op_when_the_clipboard_was_empty_before_execute() {
        let executor = ClipboardExecutor::with_clipboard(FakeClipboard::default());
        let confirmed = confirmed_with(serde_json::json!({"text": "new text"}), &executor);

        let undo = executor.execute(confirmed).unwrap();
        assert_eq!(
            undo.summary, "copied \"new text\" to the clipboard",
            "a genuinely empty prior clipboard is not data loss and gets no warning"
        );
        undo.undo()
            .expect("undo must not fail just because there was nothing to restore");
        assert_eq!(executor.clipboard.get_text().unwrap(), "new text");
    }

    /// Issue #410: `get_text` returning nothing is ambiguous (empty vs.
    /// "held something we cannot read as text"). The format probe resolves
    /// that before the overwrite, and the honest outcome is: the summary
    /// says content could not be preserved (a card line, not silent), and
    /// undo refuses to pretend it restored something it never had.
    #[test]
    fn execute_reports_and_undo_refuses_when_prior_content_could_not_be_preserved() {
        let executor =
            ClipboardExecutor::with_clipboard(FakeClipboard::holding_unrestorable_content());
        let confirmed = confirmed_with(serde_json::json!({"text": "new text"}), &executor);

        let undo = executor.execute(confirmed).unwrap();
        assert!(
            undo.summary.contains("could not be preserved"),
            "summary must say so honestly: {}",
            undo.summary
        );
        assert!(
            !undo.summary.contains('\u{2014}'),
            "no em dashes: {}",
            undo.summary
        );

        let err = undo
            .undo()
            .expect_err("undo must not silently claim success when it cannot restore lost content");
        assert!(err.to_string().contains("cannot"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }
}
