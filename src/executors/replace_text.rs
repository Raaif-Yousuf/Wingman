//! The `"replace_text"` executor (#32): consumes a confirmed `replace_text`
//! proposal identifying a UIA element captured at "Look" time
//! ([`TargetRef`]: runtime id, automation id, name, control type, window
//! handle), re-resolves that same element at "Do" time, and writes the new
//! text through `ValuePattern.SetValue`. See the connector/executor design
//! docs' shape for `calendar_add` (`src/executors/calendar_add.rs`) -- this
//! file follows the same split: parse the confirmed JSON `Value` into a
//! typed proposal on the first line of `execute`, then never touch the raw
//! JSON again.
//!
//! # Stale-target protection
//!
//! A [`TargetRef`] can fail to re-resolve two different ways, and both are a
//! refusal, never a silent write:
//!
//! 1. **The element itself cannot be found again**, or more than one
//!    element now matches (ambiguous) -- [`super::target::MatchOutcome`],
//!    checked by [`super::target::resolve_index`] against a fresh walk of
//!    the window's descendants.
//! 2. **The element's current text no longer equals the text the preview
//!    showed** -- someone typed in the field between Look and Do.
//!    [`is_stale`], checked against [`ReplaceTextProposal::expected_current_text`]
//!    before any write.
//!
//! `TargetRef` re-resolution, staleness and the real UIA walk/read/write
//! machinery live in [`super::target`] (#33: extracted so `fill_form`
//! shares them instead of duplicating).
//!
//! [`Undo`] restores the exact prior text the same way: re-resolve, check
//! the text still equals what THIS executor wrote (not the original -- see
//! `do_replace`'s undo closure), refuse if it has changed again, else
//! restore.
//!
//! `ReplaceSelection`'s stale-target protection is the SAME `is_stale`
//! check as `ReplaceAll`'s (mode-agnostic, run before either mode's
//! planning step): since `(start, end)` only ever index into
//! `expected_current_text`, a byte-identical `expected_current_text` at Do
//! time guarantees those offsets still identify the exact same substring
//! they did at Look time -- there is no separate "did the SELECTION move"
//! check to make on top of that (#219), because this executor never reads
//! the live selection at Do time at all; it only ever writes the previewed
//! substitution back into the previewed text. The one thing `is_stale`
//! alone would not catch is a captured selection collapsing to zero width
//! (`start == end`) by the time this runs -- [`plan_new_value`] refuses
//! that explicitly ([`WriteRefusal::CollapsedSelection`]) rather than
//! silently turning a replace into an insertion nobody previewed.
//!
//! # Write paths
//!
//! UIA has no direct "replace" operation. Both modes end at
//! `ValuePattern.SetValue`, on a control that supports it (a control that
//! does not -- most rich editors, browser-rendered text -- refuses; no
//! keystroke synthesis is attempted here, per the task's scope):
//!
//! - **`ReplaceAll`**: `SetValue(new_text)` directly.
//! - **`ReplaceSelection`**: the proposal carries the selection as a pair of
//!   UTF-16 code-unit offsets into [`ReplaceTextProposal::expected_current_text`]
//!   (the same unit Win32 edit controls and UIA `TextPattern` ranges use).
//!   [`splice_utf16`] substitutes `new_text` into that range -- refusing a
//!   boundary that would split a surrogate pair -- and the spliced full
//!   string is written the same way `ReplaceAll` writes it.
//!
//! # Defense in depth
//!
//! Never writes to a password field (checked before any `ValuePattern` read
//! or write -- the real text of a password field is never read into this
//! process, same guard shape as `inputs::uia`/`inputs::selection`). Never
//! writes to an element whose name or automation id reads as a final-action
//! control ([`crate::executors::uia_guard::is_forbidden_target`]) -- this
//! executor only ever targets an editable field, so this is redundant with
//! [`TargetRef`] identity in the common case, but it is cheap and the task
//! brief asks for it explicitly as a second line of defense against a
//! corrupted or mismatched target.

use anyhow::{anyhow, Context, Result};
use serde_json::Value;
use std::sync::Arc;

use crate::ui::confirm::Confirmed;

use super::target::{self, is_stale, parse_target_ref, TargetRef, TextElementAccess};
use super::{Effect, Executor, Undo};

// ---------------------------------------------------------------------------
// Pure types
// ---------------------------------------------------------------------------

/// Which write path a `replace_text` proposal asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceMode {
    ReplaceAll,
    ReplaceSelection,
}

/// A confirmed `replace_text` proposal, parsed once from JSON by
/// [`parse_replace_text`] and never re-read from the raw `Value` again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceTextProposal {
    pub target: TargetRef,
    pub new_text: String,
    pub mode: ReplaceMode,
    /// The full text the preview card showed for this element at "Look"
    /// time. Doubles as the stale-target check's baseline and, for
    /// `ReplaceSelection`, the base string [`splice_utf16`] splices into.
    pub expected_current_text: String,
    /// UTF-16 code-unit offsets `(start, end)` into `expected_current_text`,
    /// present only for `ReplaceSelection`.
    pub selection: Option<(usize, usize)>,
}

/// Why [`splice_utf16`] refused to produce a spliced string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpliceError {
    StartAfterEnd,
    OutOfBounds,
    /// `start` or `end` falls between a UTF-16 surrogate pair's two code
    /// units -- splicing there would produce invalid UTF-16.
    InvalidBoundary,
}

impl std::fmt::Display for SpliceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpliceError::StartAfterEnd => write!(f, "selection start is after its end"),
            SpliceError::OutOfBounds => {
                write!(f, "selection is out of bounds for the current text")
            }
            SpliceError::InvalidBoundary => {
                write!(f, "selection boundary falls inside a multi-unit character")
            }
        }
    }
}

impl std::error::Error for SpliceError {}

/// Splices `replacement` into `full_text` at the UTF-16 code-unit range
/// `[start, end)`, the same unit a UIA `TextPattern` selection or a Win32
/// edit control's `EM_GETSEL` reports offsets in. Never operates on `char`
/// or byte indices -- see the module doc comment.
pub fn splice_utf16(
    full_text: &str,
    start: usize,
    end: usize,
    replacement: &str,
) -> std::result::Result<String, SpliceError> {
    if start > end {
        return Err(SpliceError::StartAfterEnd);
    }
    let units: Vec<u16> = full_text.encode_utf16().collect();
    if end > units.len() {
        return Err(SpliceError::OutOfBounds);
    }
    if is_mid_surrogate_pair(&units, start) || is_mid_surrogate_pair(&units, end) {
        return Err(SpliceError::InvalidBoundary);
    }

    let mut result: Vec<u16> = Vec::with_capacity(units.len() - (end - start) + replacement.len());
    result.extend_from_slice(&units[..start]);
    result.extend(replacement.encode_utf16());
    result.extend_from_slice(&units[end..]);

    String::from_utf16(&result).map_err(|_| SpliceError::InvalidBoundary)
}

/// Whether `index` sits between a high surrogate at `units[index - 1]` and a
/// low surrogate at `units[index]` -- an invalid place to start or end a
/// splice.
fn is_mid_surrogate_pair(units: &[u16], index: usize) -> bool {
    index > 0
        && index < units.len()
        && (0xD800..=0xDBFF).contains(&units[index - 1])
        && (0xDC00..=0xDFFF).contains(&units[index])
}

/// Why [`plan_new_value`] refused to produce a value to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteRefusal {
    /// `ReplaceSelection` with no `(start, end)` captured.
    NoSelectionCaptured,
    /// `ReplaceSelection` with `start == end`. A caller that builds a
    /// `ReplaceSelection` proposal always does so from a genuinely
    /// non-empty captured selection (#219: `inputs::selection`'s
    /// `plan_from_probe` already discards any selection whose text is
    /// empty), so a collapsed range reaching here means the selection
    /// moved or was deselected since the preview was shown -- refuse
    /// rather than silently turning "replace this selection" into a plain
    /// insertion the user never previewed.
    CollapsedSelection,
    InvalidSelection(SpliceError),
}

impl std::fmt::Display for WriteRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WriteRefusal::NoSelectionCaptured => {
                write!(
                    f,
                    "mode is \"replace_selection\" but no selection was captured"
                )
            }
            WriteRefusal::CollapsedSelection => write!(
                f,
                "the selection is now empty; it may have moved or been deselected since the preview was shown"
            ),
            WriteRefusal::InvalidSelection(e) => write!(f, "invalid selection: {e}"),
        }
    }
}

impl std::error::Error for WriteRefusal {}

/// The pure planning step: given the mode and the (already stale-checked)
/// current text, decides the exact full string `ValuePattern.SetValue`
/// should write. `ReplaceAll` ignores `expected_current_text`/`selection`
/// entirely; `ReplaceSelection` requires a captured selection and splices
/// through [`splice_utf16`].
pub fn plan_new_value(
    mode: ReplaceMode,
    expected_current_text: &str,
    new_text: &str,
    selection: Option<(usize, usize)>,
) -> std::result::Result<String, WriteRefusal> {
    match mode {
        ReplaceMode::ReplaceAll => Ok(new_text.to_string()),
        ReplaceMode::ReplaceSelection => {
            let (start, end) = selection.ok_or(WriteRefusal::NoSelectionCaptured)?;
            if start == end {
                return Err(WriteRefusal::CollapsedSelection);
            }
            splice_utf16(expected_current_text, start, end, new_text)
                .map_err(WriteRefusal::InvalidSelection)
        }
    }
}

// ---------------------------------------------------------------------------
// Parsing: JSON Value -> ReplaceTextProposal (calendar_add.rs's shape)
// ---------------------------------------------------------------------------

/// Parses a confirmed `replace_text` proposal's `Value`. Everything past
/// this function in the executor's call chain acts on the typed
/// [`ReplaceTextProposal`], never the raw JSON again (see the module doc
/// comment and `calendar_add.rs`'s `parse_calendar_event`, the same shape).
fn parse_replace_text(value: &Value) -> Result<ReplaceTextProposal> {
    let target_value = value
        .get("target")
        .ok_or_else(|| anyhow!("replace_text: proposal has no \"target\" field"))?;
    let target = parse_target_ref(target_value)?;

    let new_text = value
        .get("new_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("replace_text: proposal has no \"new_text\" field"))?
        .to_string();

    let mode_str = value
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("replace_text: proposal has no \"mode\" field"))?;
    let mode = match mode_str {
        "replace_all" => ReplaceMode::ReplaceAll,
        "replace_selection" => ReplaceMode::ReplaceSelection,
        other => anyhow::bail!(
            "replace_text: unknown mode \"{other}\"; expected \"replace_all\" or \"replace_selection\""
        ),
    };

    let expected_current_text = value
        .get("expected_current_text")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("replace_text: proposal has no \"expected_current_text\" field"))?
        .to_string();

    let selection = match mode {
        ReplaceMode::ReplaceSelection => {
            let start = value
                .get("selection_start")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    anyhow!(
                        "replace_text: mode is \"replace_selection\" but \"selection_start\" is missing"
                    )
                })? as usize;
            let end = value
                .get("selection_end")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    anyhow!(
                        "replace_text: mode is \"replace_selection\" but \"selection_end\" is missing"
                    )
                })? as usize;
            Some((start, end))
        }
        ReplaceMode::ReplaceAll => None,
    };

    Ok(ReplaceTextProposal {
        target,
        new_text,
        mode,
        expected_current_text,
        selection,
    })
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// `Effect::Writes`: `replace_text` always requires the preview card's "Do
/// it" confirmation (executor design doc; `auto_confirm_read_only` refuses
/// anything that is not `Effect::ReadOnly`).
pub struct ReplaceTextExecutor {
    access: Arc<dyn TextElementAccess>,
}

impl ReplaceTextExecutor {
    /// Production constructor: the real UIA-backed access.
    pub fn new() -> Self {
        Self {
            access: Arc::new(target::com::UiaTextElementAccess),
        }
    }

    /// Test constructor: an injected fake, so the refusal/undo logic in
    /// [`do_replace`] can be exercised with no live UIA element (this
    /// file's own tests) -- and, for the real integration tests below, the
    /// real `target::com::UiaTextElementAccess` explicitly.
    #[allow(dead_code)]
    pub fn with_access(access: Arc<dyn TextElementAccess>) -> Self {
        Self { access }
    }
}

impl Default for ReplaceTextExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for ReplaceTextExecutor {
    fn name(&self) -> &'static str {
        "replace_text"
    }

    fn effect(&self) -> Effect {
        Effect::Writes
    }

    fn execute(&self, confirmed: Confirmed<Value>) -> Result<Undo> {
        let value = confirmed.into_value();
        let proposal = parse_replace_text(&value)?;
        do_replace(&self.access, &proposal)
    }
}

/// The refusal chain, in order: re-resolve -> refuse a password field ->
/// refuse a forbidden (final-action-looking) target -> refuse a stale
/// target -> plan the write -> write -> record an honest [`Undo`].
fn do_replace(access: &Arc<dyn TextElementAccess>, proposal: &ReplaceTextProposal) -> Result<Undo> {
    let resolved = access
        .resolve(&proposal.target)
        .context("replace_text: could not resolve the target element")?;

    anyhow::ensure!(
        !resolved.is_password,
        "replace_text: refusing to write to a password field"
    );
    anyhow::ensure!(
        !super::uia_guard::is_forbidden_target(&resolved.name, &resolved.automation_id),
        "replace_text: refusing to write to \"{}\"; it reads as a final-action control",
        if resolved.name.is_empty() {
            resolved.automation_id.as_str()
        } else {
            resolved.name.as_str()
        }
    );
    anyhow::ensure!(
        !is_stale(&proposal.expected_current_text, &resolved.current_text),
        "replace_text: the text has changed since the preview was shown; refusing to overwrite it"
    );

    let new_full_text = plan_new_value(
        proposal.mode,
        &proposal.expected_current_text,
        &proposal.new_text,
        proposal.selection,
    )
    .map_err(|e| anyhow!("replace_text: {e}"))?;

    access
        .write(&proposal.target, &new_full_text)
        .context("replace_text: could not write the new text")?;

    let summary = format!(
        "Replaced text in \"{}\".",
        if resolved.name.is_empty() {
            "the field"
        } else {
            resolved.name.as_str()
        }
    );

    let prior_text = proposal.expected_current_text.clone();
    let expected_after_write = new_full_text;
    let target = proposal.target.clone();
    let undo_access = Arc::clone(access);

    Ok(Undo::recording(summary, move || {
        let resolved = undo_access
            .resolve(&target)
            .context("replace_text undo: could not re-resolve the target element")?;
        anyhow::ensure!(
            !is_stale(&expected_after_write, &resolved.current_text),
            "replace_text undo: the text has changed since this executor wrote it; refusing to overwrite it"
        );
        undo_access.write(&target, &prior_text)
    }))
}

#[cfg(test)]
mod tests {
    use super::target::ResolvedElement;
    use super::*;
    use std::cell::RefCell;

    // -----------------------------------------------------------------
    // Fake injectable element model (mirrors
    // executors::clipboard::ClipboardAccess's FakeClipboard pattern)
    // -----------------------------------------------------------------

    struct FakeElement {
        name: String,
        automation_id: String,
        control_type: String,
        is_password: bool,
        text: RefCell<String>,
    }

    struct FakeAccess {
        target: TargetRef,
        element: FakeElement,
        resolve_calls: RefCell<u32>,
        write_calls: RefCell<u32>,
        resolve_should_fail: bool,
    }

    // `RefCell` isn't `Sync`; this fake is never touched from more than one
    // thread in a test, the same trade-off `calendar_add.rs`'s
    // `RecordingOpener` makes.
    unsafe impl Sync for FakeAccess {}

    impl TextElementAccess for FakeAccess {
        fn resolve(&self, target: &TargetRef) -> Result<ResolvedElement> {
            *self.resolve_calls.borrow_mut() += 1;
            anyhow::ensure!(
                !self.resolve_should_fail,
                "fake: could not resolve the target"
            );
            anyhow::ensure!(*target == self.target, "fake: unexpected target");
            Ok(ResolvedElement {
                name: self.element.name.clone(),
                automation_id: self.element.automation_id.clone(),
                control_type: self.element.control_type.clone(),
                is_password: self.element.is_password,
                current_text: self.element.text.borrow().clone(),
            })
        }

        fn write(&self, target: &TargetRef, new_text: &str) -> Result<()> {
            *self.write_calls.borrow_mut() += 1;
            anyhow::ensure!(*target == self.target, "fake: unexpected target");
            *self.element.text.borrow_mut() = new_text.to_string();
            Ok(())
        }
    }

    fn sample_target() -> TargetRef {
        TargetRef {
            hwnd: 12345,
            runtime_id: vec![1, 2, 3],
            automation_id: "editField".to_string(),
            name: "Notes".to_string(),
            control_type: "Edit".to_string(),
        }
    }

    fn fake_access_named(initial_text: &str, is_password: bool, name: &str) -> Arc<FakeAccess> {
        Arc::new(FakeAccess {
            target: sample_target(),
            element: FakeElement {
                name: name.to_string(),
                automation_id: "editField".to_string(),
                control_type: "Edit".to_string(),
                is_password,
                text: RefCell::new(initial_text.to_string()),
            },
            resolve_calls: RefCell::new(0),
            write_calls: RefCell::new(0),
            resolve_should_fail: false,
        })
    }

    fn fake_access(initial_text: &str) -> Arc<FakeAccess> {
        fake_access_named(initial_text, false, "Notes")
    }

    fn confirmed(value: Value) -> Confirmed<Value> {
        crate::ui::confirm::confirm(
            crate::ui::confirm::Proposal::new(value),
            crate::ui::confirm::user_confirmed(),
        )
    }

    fn target_json(target: &TargetRef) -> Value {
        serde_json::json!({
            "hwnd": target.hwnd,
            "runtime_id": target.runtime_id,
            "automation_id": target.automation_id,
            "name": target.name,
            "control_type": target.control_type,
        })
    }

    fn replace_all_proposal(target: &TargetRef, new_text: &str, expected: &str) -> Value {
        serde_json::json!({
            "target": target_json(target),
            "new_text": new_text,
            "mode": "replace_all",
            "expected_current_text": expected,
        })
    }

    fn replace_selection_proposal(
        target: &TargetRef,
        new_text: &str,
        expected: &str,
        start: usize,
        end: usize,
    ) -> Value {
        serde_json::json!({
            "target": target_json(target),
            "new_text": new_text,
            "mode": "replace_selection",
            "expected_current_text": expected,
            "selection_start": start,
            "selection_end": end,
        })
    }

    // -- parse_replace_text --------------------------------------------

    #[test]
    fn parses_a_full_replace_all_proposal() {
        let target = sample_target();
        let value = replace_all_proposal(&target, "new text", "old text");
        let proposal = parse_replace_text(&value).unwrap();
        assert_eq!(proposal.target, target);
        assert_eq!(proposal.new_text, "new text");
        assert_eq!(proposal.mode, ReplaceMode::ReplaceAll);
        assert_eq!(proposal.expected_current_text, "old text");
        assert_eq!(proposal.selection, None);
    }

    #[test]
    fn parses_a_full_replace_selection_proposal() {
        let target = sample_target();
        let value = replace_selection_proposal(&target, "Rust", "Hello world", 6, 11);
        let proposal = parse_replace_text(&value).unwrap();
        assert_eq!(proposal.mode, ReplaceMode::ReplaceSelection);
        assert_eq!(proposal.selection, Some((6, 11)));
    }

    #[test]
    fn missing_target_is_a_named_error_not_a_panic() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value.as_object_mut().unwrap().remove("target");
        let err = parse_replace_text(&value).expect_err("no target must error");
        assert!(err.to_string().contains("target"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn missing_new_text_is_a_named_error() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value.as_object_mut().unwrap().remove("new_text");
        let err = parse_replace_text(&value).expect_err("no new_text must error");
        assert!(err.to_string().contains("new_text"));
    }

    #[test]
    fn missing_mode_is_a_named_error() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value.as_object_mut().unwrap().remove("mode");
        let err = parse_replace_text(&value).expect_err("no mode must error");
        assert!(err.to_string().contains("mode"));
    }

    #[test]
    fn unknown_mode_is_a_named_error() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value["mode"] = serde_json::json!("delete_everything");
        let err = parse_replace_text(&value).expect_err("unknown mode must error");
        assert!(err.to_string().contains("delete_everything"));
    }

    #[test]
    fn missing_expected_current_text_is_a_named_error() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value
            .as_object_mut()
            .unwrap()
            .remove("expected_current_text");
        let err = parse_replace_text(&value).expect_err("no expected_current_text must error");
        assert!(err.to_string().contains("expected_current_text"));
    }

    #[test]
    fn replace_selection_without_bounds_is_a_named_error() {
        let mut value = replace_all_proposal(&sample_target(), "new", "old");
        value["mode"] = serde_json::json!("replace_selection");
        let err = parse_replace_text(&value)
            .expect_err("replace_selection with no selection_start must error");
        assert!(err.to_string().contains("selection_start"));
    }

    // resolve_index and is_stale are now tested in `executors::target`
    // (#33: extracted, not duplicated -- see this file's module doc comment
    // and `executors::target`'s own doc comment).

    // -- splice_utf16 ---------------------------------------------------

    #[test]
    fn splices_ascii_in_the_middle() {
        assert_eq!(
            splice_utf16("Hello world", 6, 11, "Rust").unwrap(),
            "Hello Rust"
        );
    }

    #[test]
    fn splice_with_equal_start_and_end_is_a_pure_insertion() {
        assert_eq!(
            splice_utf16("Hello world", 5, 5, ",").unwrap(),
            "Hello, world"
        );
    }

    #[test]
    fn splice_deleting_a_range_when_replacement_is_empty() {
        assert_eq!(splice_utf16("Hello world", 5, 11, "").unwrap(), "Hello");
    }

    #[test]
    fn splice_around_a_surrogate_pair_keeps_it_intact() {
        // "a\u{1F600}b" (a, grinning-face emoji, b): the emoji is a
        // surrogate pair, so "a" is at index 0, the pair at indices 1-2,
        // and "b" at index 3 in UTF-16 code units.
        let text = "a\u{1F600}b";
        assert_eq!(text.encode_utf16().count(), 4);
        // Replace the emoji (indices 1..3) whole, leaving it untouched by
        // never splitting it.
        assert_eq!(splice_utf16(text, 1, 3, "X").unwrap(), "aXb");
        // Replace only "b" (index 3..4); the emoji before it is untouched.
        assert_eq!(splice_utf16(text, 3, 4, "c").unwrap(), "a\u{1F600}c");
    }

    #[test]
    fn splice_starting_mid_surrogate_pair_is_refused() {
        let text = "a\u{1F600}b";
        // Index 2 sits between the emoji's high surrogate (index 1) and low
        // surrogate (index 2): splitting there is invalid.
        assert_eq!(
            splice_utf16(text, 2, 3, "x"),
            Err(SpliceError::InvalidBoundary)
        );
    }

    #[test]
    fn splice_across_a_crlf_boundary() {
        let text = "line1\r\nline2";
        // Replace "line1" (indices 0..5), leaving \r\n and "line2" intact.
        assert_eq!(splice_utf16(text, 0, 5, "LINE1").unwrap(), "LINE1\r\nline2");
        // Replace the \r\n itself (indices 5..7) with a single space.
        assert_eq!(splice_utf16(text, 5, 7, " ").unwrap(), "line1 line2");
    }

    #[test]
    fn splice_start_after_end_is_refused() {
        assert_eq!(
            splice_utf16("Hello", 3, 1, "x"),
            Err(SpliceError::StartAfterEnd)
        );
    }

    #[test]
    fn splice_out_of_bounds_is_refused() {
        assert_eq!(
            splice_utf16("Hello", 0, 100, "x"),
            Err(SpliceError::OutOfBounds)
        );
    }

    // -- plan_new_value -----------------------------------------------------

    #[test]
    fn replace_all_ignores_current_text_and_selection() {
        let value =
            plan_new_value(ReplaceMode::ReplaceAll, "ignored", "brand new text", None).unwrap();
        assert_eq!(value, "brand new text");
    }

    #[test]
    fn replace_selection_without_a_captured_selection_is_refused() {
        let err = plan_new_value(ReplaceMode::ReplaceSelection, "Hello world", "Rust", None)
            .expect_err("replace_selection with no selection must be refused");
        assert_eq!(err, WriteRefusal::NoSelectionCaptured);
    }

    #[test]
    fn replace_selection_with_a_collapsed_range_is_refused() {
        // #219: a genuinely captured selection is never zero-width by
        // construction; reaching plan_new_value with start == end means it
        // moved or collapsed since the preview -- refuse, never silently
        // insert.
        let err = plan_new_value(
            ReplaceMode::ReplaceSelection,
            "Hello world",
            "Rust",
            Some((6, 6)),
        )
        .expect_err("a collapsed selection must be refused");
        assert_eq!(err, WriteRefusal::CollapsedSelection);
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn replace_selection_splices_the_new_text_in() {
        let value = plan_new_value(
            ReplaceMode::ReplaceSelection,
            "Hello world",
            "Rust",
            Some((6, 11)),
        )
        .unwrap();
        assert_eq!(value, "Hello Rust");
    }

    #[test]
    fn replace_selection_with_an_invalid_boundary_is_refused() {
        let text = "a\u{1F600}b";
        let err = plan_new_value(ReplaceMode::ReplaceSelection, text, "x", Some((2, 3)))
            .expect_err("an invalid boundary must be refused");
        assert_eq!(
            err,
            WriteRefusal::InvalidSelection(SpliceError::InvalidBoundary)
        );
    }

    // -- do_replace / Undo round trip on the fake element model -------------

    #[test]
    fn execute_replace_all_writes_the_new_text() {
        let access = fake_access("original text");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_all_proposal(&sample_target(), "new text", "original text");

        executor
            .execute(confirmed(proposal))
            .expect("execute must succeed");

        assert_eq!(*access.element.text.borrow(), "new text");
        assert_eq!(*access.write_calls.borrow(), 1);
    }

    #[test]
    fn undo_restores_the_prior_text() {
        let access = fake_access("original text");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_all_proposal(&sample_target(), "new text", "original text");

        let undo = executor.execute(confirmed(proposal)).unwrap();
        assert!(undo.summary.contains("Notes"));
        assert!(
            !undo.summary.contains('\u{2014}'),
            "no em dashes: {}",
            undo.summary
        );

        undo.undo().expect("undo must succeed");
        assert_eq!(*access.element.text.borrow(), "original text");
    }

    #[test]
    fn execute_replace_selection_splices_and_undo_restores() {
        let access = fake_access("Hello world");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_selection_proposal(&sample_target(), "Rust", "Hello world", 6, 11);

        let undo = executor.execute(confirmed(proposal)).unwrap();
        assert_eq!(*access.element.text.borrow(), "Hello Rust");

        undo.undo().expect("undo must succeed");
        assert_eq!(*access.element.text.borrow(), "Hello world");
    }

    #[test]
    fn stale_target_is_refused_and_never_written() {
        let access = fake_access("someone typed this");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        // expected_current_text ("original text") no longer matches the
        // fake element's actual text: simulates "someone typed in between".
        let proposal = replace_all_proposal(&sample_target(), "new text", "original text");

        let err = executor
            .execute(confirmed(proposal))
            .err()
            .expect("a stale target must be refused");

        assert!(err.to_string().contains("changed"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
        assert_eq!(
            *access.write_calls.borrow(),
            0,
            "a refused write must never call write()"
        );
        assert_eq!(*access.element.text.borrow(), "someone typed this");
    }

    #[test]
    fn password_field_is_refused_and_never_written() {
        let access = fake_access_named("secret", true, "Password");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_all_proposal(&sample_target(), "new text", "secret");

        let err = executor
            .execute(confirmed(proposal))
            .err()
            .expect("a password field must be refused");

        assert!(err.to_string().contains("password"));
        assert_eq!(*access.write_calls.borrow(), 0);
    }

    #[test]
    fn forbidden_invokable_target_is_refused_and_never_written() {
        let access = fake_access_named("value", false, "Send");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_all_proposal(&sample_target(), "new text", "value");

        let err = executor
            .execute(confirmed(proposal))
            .err()
            .expect("a forbidden target must be refused");

        assert!(err.to_string().contains("Send"));
        assert_eq!(*access.write_calls.borrow(), 0);
    }

    #[test]
    fn an_unresolvable_target_propagates_the_resolve_error() {
        let unresolvable = FakeAccess {
            target: sample_target(),
            element: FakeElement {
                name: "Notes".to_string(),
                automation_id: "editField".to_string(),
                control_type: "Edit".to_string(),
                is_password: false,
                text: RefCell::new("text".to_string()),
            },
            resolve_calls: RefCell::new(0),
            write_calls: RefCell::new(0),
            resolve_should_fail: true,
        };
        let executor = ReplaceTextExecutor::with_access(Arc::new(unresolvable));
        let proposal = replace_all_proposal(&sample_target(), "new text", "text");

        let err = executor
            .execute(confirmed(proposal))
            .err()
            .expect("an unresolvable target must propagate the resolve error");
        assert!(err.to_string().contains("resolve"));
    }

    #[test]
    fn undo_refuses_and_does_not_restore_if_text_changed_after_the_write() {
        let access = fake_access("original text");
        let executor = ReplaceTextExecutor::with_access(access.clone());
        let proposal = replace_all_proposal(&sample_target(), "new text", "original text");

        let undo = executor.execute(confirmed(proposal)).unwrap();
        assert_eq!(*access.element.text.borrow(), "new text");

        // Someone types again before Undo is invoked.
        *access.element.text.borrow_mut() = "typed after write".to_string();

        let err = undo
            .undo()
            .expect_err("undo must refuse when the text changed after the write");
        assert!(err.to_string().contains("changed"));
        assert_eq!(
            *access.element.text.borrow(),
            "typed after write",
            "a refused undo must not overwrite newer text"
        );
    }

    // -- real Win32 window: capture a real target, replace, undo, and
    // refuse a stale target -- via a real EDIT control ---------------------
    mod win32 {
        use super::*;
        use std::sync::{Once, OnceLock};
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, LoadCursorW,
            PeekMessageW, RegisterClassExW, SetWindowTextW, ShowWindow, TranslateMessage,
            CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL, IDC_ARROW, MSG, PM_REMOVE, SW_SHOWNOACTIVATE,
            WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        };

        const CLASS_NAME: windows::core::PCWSTR =
            w!("Wingman.Executors.ReplaceTextTestWindow.test.9c31af");

        static CLASS_INIT: Once = Once::new();
        static CLASS_OK: OnceLock<bool> = OnceLock::new();

        fn instance() -> HINSTANCE {
            let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            HINSTANCE(h.0)
        }

        unsafe extern "system" fn test_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        fn ensure_class_registered(hinstance: HINSTANCE) -> bool {
            CLASS_INIT.call_once(|| {
                let ok = unsafe {
                    let wc = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(test_wndproc),
                        hInstance: hinstance,
                        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassExW(&wc) != 0
                };
                let _ = CLASS_OK.set(ok);
            });
            CLASS_OK.get().copied().unwrap_or(false)
        }

        fn pump_pending_messages() {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        fn build_test_window(initial_text: windows::core::PCWSTR) -> (HWND, HWND) {
            let hinstance = instance();
            assert!(
                ensure_class_registered(hinstance),
                "RegisterClassExW for the test window class"
            );

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman replace_text test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    240,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let edit = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("EDIT"),
                    initial_text,
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                            ES_AUTOHSCROLL as u32,
                        ),
                    10,
                    10,
                    200,
                    20,
                    Some(frame),
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (edit)");
            pump_pending_messages();

            (frame, edit)
        }

        fn edit_target(frame: HWND) -> TargetRef {
            let candidates =
                crate::executors::target::com::list_all_for_test(frame).expect("list_all_for_test");
            candidates
                .into_iter()
                .find(|c| c.control_type == "Edit")
                .expect("the test window's EDIT control must be found in the walk")
        }

        fn current_edit_text(frame: HWND) -> String {
            let access = crate::executors::target::com::UiaTextElementAccess;
            let target = edit_target(frame);
            access.resolve(&target).expect("resolve").current_text
        }

        #[test]
        fn replace_all_writes_and_undo_restores_a_real_edit_control() {
            let _uia = crate::inputs::lock_uia_test();
            let (frame, _edit) = build_test_window(w!("Hello world"));

            let target = edit_target(frame);
            let access: Arc<dyn TextElementAccess> =
                Arc::new(crate::executors::target::com::UiaTextElementAccess);
            let executor = ReplaceTextExecutor::with_access(access);

            let start = std::time::Instant::now();
            let proposal = replace_all_proposal(&target, "Replaced!", "Hello world");
            let undo = executor
                .execute(confirmed(proposal))
                .expect("execute must succeed against a real EDIT control");
            let elapsed = start.elapsed();
            // MEASURED (printed verbatim on a `-- --nocapture` run): the
            // real cost of one resolve + one write against a 1-element
            // test window.
            eprintln!("replace_text execute() took {elapsed:?} against a real EDIT control");

            assert_eq!(current_edit_text(frame), "Replaced!");

            undo.undo().expect("undo must succeed");
            assert_eq!(current_edit_text(frame), "Hello world");

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        /// #219: the observable that would differ if `ReplaceSelection`
        /// were wired to nothing -- a real EDIT control, a real captured
        /// target, splicing "world" -> "Rust" at UTF-16 offsets 6..11 of
        /// "Hello world" leaves "Hello Rust", and undo restores exactly
        /// "Hello world".
        #[test]
        fn replace_selection_writes_and_undo_restores_a_real_edit_control() {
            let _uia = crate::inputs::lock_uia_test();
            let (frame, _edit) = build_test_window(w!("Hello world"));

            let target = edit_target(frame);
            let access: Arc<dyn TextElementAccess> =
                Arc::new(crate::executors::target::com::UiaTextElementAccess);
            let executor = ReplaceTextExecutor::with_access(access);

            let proposal = replace_selection_proposal(&target, "Rust", "Hello world", 6, 11);
            let undo = executor
                .execute(confirmed(proposal))
                .expect("execute must succeed against a real EDIT control");

            assert_eq!(current_edit_text(frame), "Hello Rust");

            undo.undo().expect("undo must succeed");
            assert_eq!(current_edit_text(frame), "Hello world");

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        /// #219: `ReplaceSelection`'s "own equivalent" of the `ReplaceAll`
        /// stale-target test above -- the SAME mode-agnostic `is_stale`
        /// check (see the module doc comment) must also refuse a
        /// `ReplaceSelection` write when the field's real text changed
        /// between Look and Do.
        #[test]
        fn stale_target_is_refused_for_replace_selection_when_text_changed_between_look_and_do() {
            let _uia = crate::inputs::lock_uia_test();
            let (frame, edit) = build_test_window(w!("Hello world"));

            let target = edit_target(frame);

            unsafe {
                let _ = SetWindowTextW(edit, w!("Someone typed this"));
            }

            let access: Arc<dyn TextElementAccess> =
                Arc::new(crate::executors::target::com::UiaTextElementAccess);
            let executor = ReplaceTextExecutor::with_access(access);
            let proposal = replace_selection_proposal(&target, "Rust", "Hello world", 6, 11);

            let err = executor.execute(confirmed(proposal)).err().expect(
                "a stale target must be refused for replace_selection against a real EDIT control",
            );
            assert!(err.to_string().contains("changed"));
            assert_eq!(
                current_edit_text(frame),
                "Someone typed this",
                "a refused write must not touch the control"
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }

        #[test]
        fn stale_target_is_refused_when_text_changed_between_look_and_do() {
            let _uia = crate::inputs::lock_uia_test();
            let (frame, edit) = build_test_window(w!("Hello world"));

            let target = edit_target(frame);

            // Simulate "someone typed in between": change the real
            // control's text after the target/preview was captured but
            // before Do runs.
            unsafe {
                let _ = SetWindowTextW(edit, w!("Someone typed this"));
            }

            let access: Arc<dyn TextElementAccess> =
                Arc::new(crate::executors::target::com::UiaTextElementAccess);
            let executor = ReplaceTextExecutor::with_access(access);
            let proposal = replace_all_proposal(&target, "Replaced!", "Hello world");

            let err = executor
                .execute(confirmed(proposal))
                .err()
                .expect("a stale target must be refused against a real EDIT control");
            assert!(err.to_string().contains("changed"));
            assert_eq!(
                current_edit_text(frame),
                "Someone typed this",
                "a refused write must not touch the control"
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
        }
    }
}
