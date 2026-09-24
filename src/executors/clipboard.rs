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

        // Best-effort: a clipboard that was empty or held non-text content
        // restores to nothing rather than failing the whole action. These
        // two cases are indistinguishable here (arboard's
        // `ContentNotAvailable` covers both), so a prior non-text value is
        // silently lost on undo rather than merely left alone -- tracked
        // separately as issue #410, not fixed by this decision.
        let previous = self.clipboard.get_text().ok();

        self.clipboard.set_text(&text)?;

        let clipboard = Arc::clone(&self.clipboard);
        Ok(Undo::recording(
            format!("copied \"{text}\" to the clipboard"),
            move || {
                if let Some(previous) = previous {
                    clipboard.set_text(&previous)?;
                }
                Ok(())
            },
        ))
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
    }

    impl FakeClipboard {
        fn seeded(initial: &str) -> Self {
            Self {
                text: RefCell::new(Some(initial.to_string())),
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
        undo.undo()
            .expect("undo must not fail just because there was nothing to restore");
        assert_eq!(executor.clipboard.get_text().unwrap(), "new text");
    }
}
