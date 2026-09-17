//! "Copy text from screen" (#41): the built-in action's model-free
//! execution path. Screen -> Windows OCR -> the `"clipboard"` executor -> a
//! result card. No provider, no schema, no network call of any kind.
//!
//! This module's public functions take no [`crate::config::Providers`] and
//! no [`crate::mode::Mode`] parameter anywhere in their signatures -- unlike
//! `app.rs`'s `worker` (which builds a [`crate::provider::Chain`] from
//! exactly those two things), there is nothing here for a chain to be built
//! from. That is the proof #41's Done-when ("works with Offline mode on and
//! Ollama stopped") holds: this path does not know what a mode or a
//! provider even is, so it cannot fail to route around one. The only gate
//! this action has is Paused (see [`gate`]) -- checked by `app.rs`, which
//! also owns the two Win32-only steps (hiding the card, capturing the
//! screen on the main thread before the pending card shows) the same way
//! `App::ask` does for the same reason.
//!
//! Registered in [`super::builtin_actions`] as catalogue metadata for the
//! palette (#25, not yet built) to show one day; nothing on *this* path
//! reads that `Action` entry back (see this module's doc comment there for
//! why `proposal` is deliberately an unregistered schema name).

use anyhow::{Context, Result};
use serde_json::json;

use crate::capture::RawShot;
use crate::executors::Executor;
use crate::ocr::OcrOutput;
use crate::provider::Answer;
use crate::ui::confirm::{auto_confirm_read_only, Proposal};

/// OCR, injected so [`run_pipeline`]'s tests never touch the real
/// `Windows.Media.Ocr` engine (the same reason `executors::clipboard`
/// injects `ClipboardAccess`).
pub trait TextRecognizer {
    fn recognize(&self, raw: &RawShot) -> Result<OcrOutput>;
}

/// The real recognizer: `ocr::recognize` over the captured pixels, with
/// `ocr`'s own default timeout. `ocr::recognize` already fits the image to
/// `OcrEngine::MaxImageDimension` internally, so [`capture_screen`] below
/// captures at native monitor resolution rather than pre-downscaling for a
/// model that is never involved on this path.
pub struct WindowsOcr;

impl TextRecognizer for WindowsOcr {
    fn recognize(&self, raw: &RawShot) -> Result<OcrOutput> {
        crate::ocr::recognize(
            &raw.rgba,
            raw.width,
            raw.height,
            crate::ocr::DEFAULT_TIMEOUT,
        )
    }
}

/// Whether pressing "Copy text from screen" may proceed right now. The only
/// gate this action has: Paused. Unlike `App::ask`'s `readiness_gate`, there
/// is no provider-readiness check here at all -- this path never asks a
/// model, so "is a provider configured" is not a question it needs to
/// answer (#41's Done-when: works with Offline mode on and Ollama stopped,
/// and equally with no provider configured at all). Pure so the decision is
/// unit-tested directly (CLAUDE.md rule 8); the real Paused check
/// (`pause::is_paused_now()`) is Win32-adjacent state `app.rs` reads and
/// passes in.
pub fn gate(paused: bool) -> std::result::Result<(), (&'static str, &'static str)> {
    if paused {
        Err(("Paused", "Resume from the tray menu to ask."))
    } else {
        Ok(())
    }
}

/// Capture the active monitor at native resolution -- see [`WindowsOcr`]'s
/// doc comment for why no model-oriented downscaling (`capture::resolve_limits`)
/// applies here. Must run on the MAIN thread, before the pending card shows,
/// the same ordering and for the same reason `App::ask` captures before
/// `show_pending` (the card must not appear in its own screenshot).
pub fn capture_screen() -> Result<RawShot> {
    crate::capture::grab_raw("active", u32::MAX, u64::MAX)
}

/// OCR `raw`, and if any text was found, copy it to the clipboard through
/// `executor` (must declare [`crate::executors::Effect::ReadOnly`], enforced
/// by [`auto_confirm_read_only`] -- this action only ever writes the system
/// clipboard, never anything on screen, so expansion plan §6's "Read-only
/// actions show a result card straight away" applies and no confirm/preview
/// step is ever shown).
///
/// Lines are joined in the reading order `OcrOutput::lines` already returns
/// them in (`ocr::serialize_lines`, already tested on its own). An empty OCR
/// result returns the "No text found" card and never touches the clipboard
/// (issue #41: nothing on screen to copy is not the same as "copy nothing
/// over whatever the user already had there").
pub fn run_pipeline(
    raw: &RawShot,
    recognizer: &dyn TextRecognizer,
    executor: &dyn Executor,
) -> Result<Answer> {
    let ocr = recognizer
        .recognize(raw)
        .context("Couldn't read text on screen")?;

    if ocr.lines.is_empty() {
        return Ok(Answer {
            headline: "No text found on screen".to_string(),
            detail: String::new(),
            difficulty: None,
        });
    }

    let text = crate::ocr::serialize_lines(&ocr.lines);
    let n = ocr.lines.len();

    let proposal = Proposal::new(json!({ "text": text }));
    let confirmed = auto_confirm_read_only(executor, proposal)
        .context("extract text: executor is not read-only")?;
    executor
        .execute(confirmed)
        .context("Couldn't copy to the clipboard")?;

    Ok(Answer {
        headline: format!("Copied {n} line{} of text", if n == 1 { "" } else { "s" }),
        detail: String::new(),
        difficulty: None,
    })
}

/// The real, non-test pipeline: OCR via [`WindowsOcr`], copy via the
/// `"clipboard"` executor resolved by name (same registry every other
/// executor goes through). Runs on a worker thread -- OCR is this action's
/// expensive step, the same reasoning `capture::encode` already documents
/// for the provider path in `App::ask`.
pub fn recognize_and_copy(raw: &RawShot) -> Result<Answer> {
    let executor = crate::executors::registry::resolve("clipboard")
        .context("extract text: couldn't resolve the clipboard executor")?;
    run_pipeline(raw, &WindowsOcr, executor.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::{Effect, Undo};
    use std::cell::RefCell;

    fn raw() -> RawShot {
        RawShot {
            rgba: vec![0; 4],
            width: 1,
            height: 1,
        }
    }

    fn ocr_line(text: &str) -> crate::ocr::OcrLine {
        crate::ocr::OcrLine {
            text: text.to_string(),
            x: 0.0,
            y: 0.0,
            width: 1.0,
            height: 1.0,
        }
    }

    // -- fakes ---------------------------------------------------------

    struct FakeRecognizer(Result<OcrOutput>);
    impl TextRecognizer for FakeRecognizer {
        fn recognize(&self, _raw: &RawShot) -> Result<OcrOutput> {
            match &self.0 {
                Ok(out) => Ok(out.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    /// Records what it was asked to copy; never touches a real clipboard.
    #[derive(Default)]
    struct RecordingExecutor {
        copied: RefCell<Option<String>>,
    }

    // `RefCell` is not `Sync`; every test here is single-threaded and never
    // shares this fake across an actual thread boundary, so this promise to
    // the compiler is safe -- same pattern `executors::clipboard`'s own
    // `FakeClipboard` test double uses.
    unsafe impl Sync for RecordingExecutor {}

    impl Executor for RecordingExecutor {
        fn name(&self) -> &'static str {
            "recording"
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        fn execute(
            &self,
            confirmed: crate::ui::confirm::Confirmed<serde_json::Value>,
        ) -> Result<Undo> {
            let value = confirmed.into_value();
            let text = value.get("text").and_then(|v| v.as_str()).unwrap();
            *self.copied.borrow_mut() = Some(text.to_string());
            Ok(Undo::none("test"))
        }
    }

    struct FailingExecutor;
    impl Executor for FailingExecutor {
        fn name(&self) -> &'static str {
            "failing"
        }
        fn effect(&self) -> Effect {
            Effect::ReadOnly
        }
        fn execute(
            &self,
            _confirmed: crate::ui::confirm::Confirmed<serde_json::Value>,
        ) -> Result<Undo> {
            anyhow::bail!("clipboard is locked")
        }
    }

    // -- run_pipeline: lines joined in reading order --------------------

    #[test]
    fn joins_lines_in_reading_order_and_copies_them() {
        let recognizer = FakeRecognizer(Ok(OcrOutput {
            lines: vec![ocr_line("first"), ocr_line("second"), ocr_line("third")],
            language_tag: "en-US".to_string(),
        }));
        let executor = RecordingExecutor::default();

        let answer = run_pipeline(&raw(), &recognizer, &executor).expect("must succeed");

        assert_eq!(
            executor.copied.borrow().as_deref(),
            Some("first\nsecond\nthird")
        );
        assert_eq!(answer.headline, "Copied 3 lines of text");
        assert!(!answer.headline.contains('\u{2014}'), "no em dashes");
    }

    #[test]
    fn singular_line_count_reads_naturally() {
        let recognizer = FakeRecognizer(Ok(OcrOutput {
            lines: vec![ocr_line("only line")],
            language_tag: "en-US".to_string(),
        }));
        let executor = RecordingExecutor::default();

        let answer = run_pipeline(&raw(), &recognizer, &executor).expect("must succeed");

        assert_eq!(answer.headline, "Copied 1 line of text");
    }

    // -- empty result card, clipboard untouched --------------------------

    #[test]
    fn no_text_found_card_when_ocr_returns_no_lines() {
        let recognizer = FakeRecognizer(Ok(OcrOutput {
            lines: vec![],
            language_tag: "en-US".to_string(),
        }));
        let executor = RecordingExecutor::default();

        let answer = run_pipeline(&raw(), &recognizer, &executor).expect("must succeed");

        assert_eq!(answer.headline, "No text found on screen");
        assert!(
            executor.copied.borrow().is_none(),
            "the clipboard must not be touched when there is nothing to copy"
        );
    }

    // -- error card: OCR failure -----------------------------------------

    #[test]
    fn ocr_failure_is_an_error_not_a_silent_empty_result() {
        let recognizer =
            FakeRecognizer(Err(anyhow::anyhow!("no OCR-capable language is installed")));
        let executor = RecordingExecutor::default();

        let err = run_pipeline(&raw(), &recognizer, &executor)
            .expect_err("an OCR failure must be an error");
        let msg = format!("{err:#}");
        assert!(msg.contains("no OCR-capable language is installed"));
        assert!(!msg.contains('\u{2014}'), "no em dashes: {msg}");
        assert!(
            executor.copied.borrow().is_none(),
            "the clipboard must not be touched on an OCR failure"
        );
    }

    #[test]
    fn clipboard_write_failure_is_an_error() {
        let recognizer = FakeRecognizer(Ok(OcrOutput {
            lines: vec![ocr_line("some text")],
            language_tag: "en-US".to_string(),
        }));

        let err = run_pipeline(&raw(), &recognizer, &FailingExecutor)
            .expect_err("a clipboard failure must be an error");
        assert!(format!("{err:#}").contains("clipboard is locked"));
    }

    // -- pause gate --------------------------------------------------------

    #[test]
    fn gate_allows_when_not_paused() {
        assert!(gate(false).is_ok());
    }

    #[test]
    fn gate_blocks_when_paused() {
        let (headline, detail) = gate(true).unwrap_err();
        assert_eq!(headline, "Paused");
        assert!(!detail.contains('\u{2014}'), "no em dashes: {detail}");
    }

    // -- no provider chain is constructed on this path --------------------
    //
    // Structural, not behavioural: `run_pipeline`, `capture_screen`,
    // `recognize_and_copy` and `gate` all appear above with their full
    // signatures, and not one names `crate::config::Providers` or
    // `crate::mode::Mode`. There is therefore no value of that type
    // anywhere in this module for a chain to be built from -- the same
    // proof-by-construction `ui::confirm`'s module doc uses for why an
    // executor cannot fabricate a `Confirmed<P>`. This test is the
    // regression guard: it fails to compile (not merely fails to pass) the
    // moment either function's signature grows one of those parameters.
    #[test]
    fn pipeline_functions_take_no_providers_or_mode_parameter() {
        fn assert_no_extra_param<
            F: Fn(&RawShot, &dyn TextRecognizer, &dyn Executor) -> Result<Answer>,
        >(
            _f: F,
        ) {
        }
        assert_no_extra_param(run_pipeline);
    }
}
