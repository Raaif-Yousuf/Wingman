//! Replacing the selection from a generic (`actions.toml`) action (#68).
//!
//! A generic action whose executor is `"replace_text"` and whose inputs list
//! `selection` writes its confirmed `text` over the text the user selected.
//! The selection is read with its UIA identity (`SelectionTarget`) on the
//! worker thread; [`replace_plan`] decides whether that identity is safe to
//! write through, and [`review_context`] turns the confirmed card value into
//! the same `ReviewContext` "Review this email" writes with, so both go
//! through `review_email::do_review_confirmed` and the executor's stale-target
//! check. Pure logic, unit-tested without a window.

use serde_json::Value;

use super::generic_inputs::Gathered;
use super::review_email::{do_review_confirmed, ReplaceTarget, ReviewContext};
use crate::executors::Executor;
use crate::inputs::selection::SelectionTarget;
use crate::ui::confirm::Confirmed;

/// What a replace needs from "Look": the selection's UIA identity (`None`
/// when it can only be copied, not written back) and the text that was
/// selected (what the model was shown). `replace_plan` boxes it, because it
/// is much bigger than the rest of a message payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceSelection {
    pub target: Option<SelectionTarget>,
    pub selected_text: String,
}

impl ReplaceSelection {
    /// The text of the whole control, when known: the line endings to use
    /// when the selection itself has no line break to copy them from.
    fn eol_hint(&self) -> &str {
        self.target.as_ref().map_or("", |t| t.full_text.as_str())
    }

    /// Whether "Do it" will write over the selection (otherwise it copies).
    pub fn will_replace(&self) -> bool {
        self.target.is_some()
    }
}

/// What the card says when the text could not be replaced in place and was
/// copied instead.
pub const COPIED_INSTEAD_DETAIL: &str =
    "Wingman could not replace the text in place here, so the result is on the clipboard.";

/// The replace to offer for what was gathered, or `None` when there is
/// nothing safe to offer. `None` for: a selection cut to the length limit
/// (replacing it would drop the rest), a password field, and a blank
/// selection. With no UIA target (a clipboard-fallback or value-only control)
/// the plan has no target, and "Do it" copies the text instead of writing it.
pub fn replace_plan(gathered: &Gathered) -> Option<Box<ReplaceSelection>> {
    if gathered.selection_in_password_field || gathered.selection_truncated {
        return None;
    }
    let target = gathered.selection_target.clone();
    let selected_text = gathered.selection.clone()?;
    if selected_text.trim().is_empty() {
        return None;
    }
    Some(Box::new(ReplaceSelection {
        target,
        selected_text,
    }))
}

/// The `ReviewContext` for a confirmed `text_answer`-shaped value: its `text`
/// is the new selection. `None` when the confirmed value has no usable text
/// (blank text would delete the selection) or the plan has no target to
/// write through.
pub fn review_context(plan: &ReplaceSelection, confirmed: &Value) -> Option<ReviewContext> {
    let target = plan.target.clone()?;
    let new_text = confirmed.get("text")?.as_str()?;
    if new_text.trim().is_empty() {
        return None;
    }
    Some(ReviewContext {
        target: ReplaceTarget::Selection(target),
        original_text: plan.selected_text.clone(),
        new_text: match_line_endings_hinted(&plan.selected_text, plan.eol_hint(), new_text),
    })
}

/// The proposal with its `text` given the selection's line endings, so the
/// card shows, and a write or a copy-instead uses, text that matches the
/// document. A proposal with no text is returned as it was.
pub fn normalize_proposal(plan: &ReplaceSelection, proposal: Value) -> Value {
    let mut proposal = proposal;
    let matched = proposal
        .get("text")
        .and_then(Value::as_str)
        .map(|t| match_line_endings_hinted(&plan.selected_text, plan.eol_hint(), t));
    if let (Some(text), Some(slot)) = (matched, proposal.get_mut("text")) {
        *slot = Value::String(text);
    }
    proposal
}

/// The model's text with the selection's own line endings: a model returns
/// `\n`, and writing that into a CRLF document leaves a mix. The dominant
/// line ending of `original` (CRLF, LF or CR; a tie goes to CRLF) is applied
/// to every line break in `new`, and a trailing line break is kept or dropped
/// to match whether `original` had one. An `original` with no line break at
/// all leaves `new`'s line breaks alone (nothing to match), but still drops
/// a trailing one the selection did not have.
pub fn match_line_endings(original: &str, new: &str) -> String {
    match_line_endings_hinted(original, "", new)
}

/// Counts of CRLF, lone LF and lone CR line breaks in `text`.
fn count_breaks(text: &str) -> (usize, usize, usize) {
    let (mut crlf, mut lf, mut cr) = (0usize, 0usize, 0usize);
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' if chars.peek() == Some(&'\n') => {
                crlf += 1;
                chars.next();
            }
            '\r' => cr += 1,
            '\n' => lf += 1,
            _ => {}
        }
    }
    (crlf, lf, cr)
}

/// [`match_line_endings`] where `hint` (the whole control's text) supplies
/// the line ending when `original` has no line break of its own: a one-line
/// selection in a CRLF document still gets CRLF for any break the model adds.
pub fn match_line_endings_hinted(original: &str, hint: &str, new: &str) -> String {
    let (mut crlf, mut lf, mut cr) = count_breaks(original);
    if crlf + lf + cr == 0 {
        (crlf, lf, cr) = count_breaks(hint);
    }
    // Normalise the model's breaks to LF first, then pick the target.
    let mut text = new.replace("\r\n", "\n").replace('\r', "\n");
    let original_ends_with_break = original.ends_with(['\n', '\r']);
    if !original_ends_with_break {
        if let Some(stripped) = text.strip_suffix('\n') {
            text = stripped.to_string();
        }
    }
    let eol = if crlf + lf + cr == 0 {
        return text;
    } else if crlf >= lf && crlf >= cr {
        "\r\n"
    } else if lf >= cr {
        "\n"
    } else {
        "\r"
    };
    if original_ends_with_break && !text.ends_with('\n') {
        text.push('\n');
    }
    if eol == "\n" {
        text
    } else {
        text.replace('\n', eol)
    }
}

/// Shown instead of calling the model when the selection was cut to the
/// length limit: a translation of part of it could not replace all of it.
pub const TOO_LONG_MESSAGE: &str =
    "The selection is too long to replace in place. Select less text and try again.";

/// Whether the selection was cut to the length limit.
pub fn too_long(gathered: &Gathered) -> bool {
    gathered.selection_truncated
}

/// What the card says when the model's text is the selection as it was, so
/// there is nothing to write.
pub const NO_CHANGES_HEADLINE: &str = "No changes needed";

/// Whether the model's `text` is the selection unchanged (ignoring line
/// ending style and trailing whitespace), so a write would change nothing.
/// False when the proposal has no text.
pub fn is_unchanged(selected_text: &str, proposal: &Value) -> bool {
    let Some(text) = proposal.get("text").and_then(Value::as_str) else {
        return false;
    };
    match_line_endings(selected_text, text).trim_end() == selected_text.trim_end()
}

/// The preview card's title: says so when "Do it" will copy the text to the
/// clipboard rather than replace the selection.
pub fn preview_title(action_name: &str, will_replace: bool) -> String {
    if will_replace {
        action_name.to_string()
    } else {
        format!("{action_name} (copies to the clipboard)")
    }
}

/// What happened when the confirmed text was applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceReport {
    /// The confirmed text was blank: nothing was written or copied.
    NothingToReplace,
    /// The selection was replaced; carries the executor's summary.
    Replaced(String),
    /// The text was copied to the clipboard instead; carries the card detail.
    Copied(String),
    /// Neither the replace nor the copy worked; carries the reason.
    Failed(String),
}

/// Applies a confirmed `text_answer` value: writes it over the selection
/// when there is a `plan` and the write succeeds, and otherwise copies it to
/// the clipboard so the user's confirmed (possibly edited) text is never
/// lost. A blank text does neither. The executors are passed in so this is
/// tested with fakes; `App` passes the real `replace_text` and `clipboard`.
pub fn replace_or_copy(
    replace: &dyn Executor,
    copy: &dyn Executor,
    plan: Option<&ReplaceSelection>,
    confirmed: Confirmed<Value>,
) -> ReplaceReport {
    let blank = confirmed
        .value()
        .get("text")
        .and_then(Value::as_str)
        .map_or(true, |t| t.trim().is_empty());
    if blank {
        return ReplaceReport::NothingToReplace;
    }
    // #694: text the user edited in the preview is matched to the selection's
    // line endings too, so a copy-instead (no target, or a failed write)
    // puts the same text on the clipboard that a write would have used. Same
    // already-confirmed decision, re-wrapped (see `do_review_confirmed`).
    let confirmed = match plan {
        Some(p) => crate::ui::confirm::confirm(
            crate::ui::confirm::Proposal::new(normalize_proposal(p, confirmed.into_value())),
            crate::ui::confirm::user_confirmed(),
        ),
        None => confirmed,
    };
    let Some(context) = plan.and_then(|p| review_context(p, confirmed.value())) else {
        return copy_report(copy, confirmed, COPIED_INSTEAD_DETAIL.to_string(), None);
    };
    match do_review_confirmed(replace, &context) {
        Ok(undo) => ReplaceReport::Replaced(undo.summary),
        Err(e) => {
            let reason = format!("{e:#}");
            let detail = format!(
                "Could not replace in place: {}. The text is on the clipboard.",
                reason.trim_end_matches('.')
            );
            copy_report(copy, confirmed, detail, Some(reason))
        }
    }
}

fn copy_report(
    copy: &dyn Executor,
    confirmed: Confirmed<Value>,
    detail: String,
    replace_error: Option<String>,
) -> ReplaceReport {
    match copy.execute(confirmed) {
        Ok(_) => ReplaceReport::Copied(detail),
        Err(e) => ReplaceReport::Failed(match replace_error {
            Some(r) => format!(
                "{}. Copying the text also failed: {e:#}",
                r.trim_end_matches('.')
            ),
            None => format!("{e:#}"),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn target() -> SelectionTarget {
        SelectionTarget {
            hwnd: 7,
            runtime_id: vec![42, 1],
            automation_id: "editor".into(),
            name: "Body".into(),
            control_type: "Edit".into(),
            full_text: "Say bonjour please".into(),
            start: 4,
            end: 11,
        }
    }

    fn gathered() -> Gathered {
        Gathered {
            selection: Some("bonjour".into()),
            selection_target: Some(target()),
            ..Gathered::default()
        }
    }

    #[test]
    fn model_line_breaks_take_the_selections_line_endings() {
        // A CRLF document gets CRLF, not a mix.
        assert_eq!(match_line_endings("a\r\nb\r\nc", "x\ny\nz"), "x\r\ny\r\nz");
        // An LF document stays LF, even if the model sent CRLF.
        assert_eq!(match_line_endings("a\nb", "x\r\ny"), "x\ny");
        // A CR-only selection (old Mac text) gets CR.
        assert_eq!(match_line_endings("a\rb\rc", "x\ny"), "x\ry");
        // Mixed: the dominant ending wins.
        assert_eq!(
            match_line_endings("a\r\nb\r\nc\nd", "x\ny\nz"),
            "x\r\ny\r\nz"
        );
        assert_eq!(match_line_endings("a\nb\nc\r\nd", "x\r\ny"), "x\ny");
        // A model that already used CRLF is not doubled.
        assert_eq!(match_line_endings("a\r\nb", "x\r\ny"), "x\r\ny");
    }

    #[test]
    fn a_trailing_line_break_is_kept_or_dropped_to_match_the_selection() {
        // The model adds a final newline the selection did not have.
        assert_eq!(match_line_endings("a\r\nb", "x\ny\n"), "x\r\ny");
        assert_eq!(match_line_endings("one line", "fixed\n"), "fixed");
        // The selection ended with one and the model dropped it.
        assert_eq!(match_line_endings("a\r\nb\r\n", "x\ny"), "x\r\ny\r\n");
        assert_eq!(match_line_endings("a\n", "x"), "x\n");
        // Both have one: unchanged apart from the ending style.
        assert_eq!(match_line_endings("a\r\nb\r\n", "x\ny\n"), "x\r\ny\r\n");
    }

    #[test]
    fn text_with_no_line_breaks_to_match_is_left_alone() {
        assert_eq!(match_line_endings("one line", "x\ny"), "x\ny");
        assert_eq!(match_line_endings("", "x"), "x");
        assert_eq!(match_line_endings("a\r\nb", ""), "");
    }

    #[test]
    fn the_replace_context_carries_the_matched_line_endings() {
        let plan = ReplaceSelection {
            target: Some(target()),
            selected_text: "a\r\nb".into(),
        };
        let ctx = review_context(&plan, &json!({"text": "x\ny\n"})).unwrap();
        assert_eq!(ctx.new_text, "x\r\ny");
        assert_eq!(ctx.original_text, "a\r\nb");
    }

    #[test]
    fn a_one_line_selection_takes_the_line_ending_of_the_whole_control() {
        assert_eq!(
            match_line_endings_hinted("one line", "a\r\none line\r\nz", "x\ny"),
            "x\r\ny"
        );
        // The selection's own breaks win over the hint.
        assert_eq!(
            match_line_endings_hinted("a\nb", "a\r\nb\r\nc\r\n", "x\r\ny"),
            "x\ny"
        );
        // No breaks anywhere: left alone, but the extra trailing one goes.
        assert_eq!(match_line_endings_hinted("one", "one", "x\ny\n"), "x\ny");
        let mut g = gathered();
        g.selection_target.as_mut().unwrap().full_text = "p\r\nSay bonjour please\r\n".into();
        let plan = replace_plan(&g).unwrap();
        let ctx = review_context(&plan, &json!({"text": "a\nb"})).unwrap();
        assert_eq!(ctx.new_text, "a\r\nb");
    }

    #[test]
    fn the_proposal_text_is_matched_before_it_is_shown_or_copied() {
        let plan = ReplaceSelection {
            target: None,
            selected_text: "a\r\nb".into(),
        };
        let p = normalize_proposal(&plan, json!({"text": "x\ny\n", "other": 1}));
        assert_eq!(p, json!({"text": "x\r\ny", "other": 1}));
        // No text: unchanged.
        assert_eq!(normalize_proposal(&plan, json!({"a": 1})), json!({"a": 1}));
    }

    #[test]
    fn a_selection_with_no_target_is_copied_not_written() {
        let mut g = gathered();
        g.selection_target = None;
        let plan = replace_plan(&g).expect("still offered, as a copy");
        assert!(!plan.will_replace());
        assert_eq!(plan.selected_text, "bonjour");
        assert_eq!(review_context(&plan, &json!({"text": "x"})), None);
        assert!(replace_plan(&gathered()).unwrap().will_replace());
    }

    #[test]
    fn text_identical_to_the_selection_is_no_change() {
        let p = |t: &str| json!({"text": t});
        assert!(is_unchanged("let a = 1;", &p("let a = 1;")));
        // A trailing newline or CRLF/LF difference is not a change.
        assert!(is_unchanged("a\r\nb", &p("a\nb\n")));
        assert!(is_unchanged("a\nb\n", &p("a\nb")));
        assert!(!is_unchanged("let a = 1;", &p("let a = 2;")));
        assert!(!is_unchanged("a b", &p("a  b")));
        assert!(!is_unchanged("a", &json!({})));
        assert!(!NO_CHANGES_HEADLINE.contains('\u{2014}'));
    }

    #[test]
    fn a_readable_selection_with_a_target_can_be_replaced() {
        let plan = replace_plan(&gathered()).expect("replaceable");
        assert_eq!(plan.target, Some(target()));
        assert_eq!(plan.selected_text, "bonjour");
    }

    #[test]
    fn a_cut_selection_a_password_or_a_blank_selection_are_not_replaced() {
        let mut cut = gathered();
        cut.selection_truncated = true;
        let mut password = gathered();
        password.selection_in_password_field = true;
        let mut blank = gathered();
        blank.selection = Some("  \n".into());
        let mut none = gathered();
        none.selection = None;
        for (name, g) in [
            ("cut", cut),
            ("password", password),
            ("blank", blank),
            ("no selection", none),
        ] {
            assert_eq!(replace_plan(&g), None, "{name}");
        }
    }

    #[test]
    fn the_confirmed_text_becomes_a_replace_selection_context() {
        let plan = replace_plan(&gathered()).unwrap();
        let ctx = review_context(&plan, &json!({"text": "hello"})).expect("context");
        assert_eq!(ctx.new_text, "hello");
        assert_eq!(ctx.original_text, "bonjour");
        assert_eq!(ctx.target, ReplaceTarget::Selection(target()));
    }

    #[test]
    fn a_user_edit_in_the_card_is_what_gets_written() {
        let plan = replace_plan(&gathered()).unwrap();
        let ctx = review_context(&plan, &json!({"text": "hello (edited)"})).unwrap();
        assert_eq!(ctx.new_text, "hello (edited)");
    }

    #[test]
    fn blank_or_missing_confirmed_text_never_deletes_the_selection() {
        let plan = replace_plan(&gathered()).unwrap();
        for v in [
            json!({"text": ""}),
            json!({"text": "  "}),
            json!({}),
            json!({"text": 3}),
            json!("x"),
        ] {
            assert_eq!(review_context(&plan, &v), None, "{v}");
        }
    }

    // -- replace_or_copy: the confirmed step, with injected executors -------

    use crate::executors::{Effect, Executor, Undo};
    use crate::ui::confirm::{confirm, user_confirmed, Confirmed, Proposal};
    use std::sync::Mutex;

    struct Fake {
        name: &'static str,
        result: Mutex<Option<anyhow::Result<Undo>>>,
        seen: Mutex<Option<Value>>,
    }

    impl Fake {
        fn new(name: &'static str, result: anyhow::Result<Undo>) -> Self {
            Fake {
                name,
                result: Mutex::new(Some(result)),
                seen: Mutex::new(None),
            }
        }
        fn ran(&self) -> bool {
            self.seen.lock().unwrap().is_some()
        }
    }

    impl Executor for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn effect(&self) -> Effect {
            Effect::Writes
        }
        fn execute(&self, confirmed: Confirmed<Value>) -> anyhow::Result<Undo> {
            *self.seen.lock().unwrap() = Some(confirmed.into_value());
            self.result.lock().unwrap().take().expect("run once")
        }
    }

    fn confirmed(v: Value) -> Confirmed<Value> {
        confirm(Proposal::new(v), user_confirmed())
    }

    #[test]
    fn a_successful_replace_reports_it_and_copies_nothing() {
        let replace = Fake::new("replace_text", Ok(Undo::none("wrote it")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let plan = replace_plan(&gathered());
        let report = replace_or_copy(
            &replace,
            &copy,
            plan.as_deref(),
            confirmed(json!({"text": "hello"})),
        );
        assert_eq!(report, ReplaceReport::Replaced("wrote it".into()));
        assert!(replace.ran());
        assert!(!copy.ran());
    }

    #[test]
    fn a_failed_replace_keeps_the_edited_text_on_the_clipboard() {
        let replace = Fake::new("replace_text", Err(anyhow::anyhow!("the text changed")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let plan = replace_plan(&gathered());
        let report = replace_or_copy(
            &replace,
            &copy,
            plan.as_deref(),
            confirmed(json!({"text": "hello (edited)"})),
        );
        let ReplaceReport::Copied(detail) = report else {
            panic!("expected a copy, got {report:?}");
        };
        assert!(
            detail.contains("Could not replace in place: the text changed"),
            "{detail}"
        );
        assert!(detail.contains("on the clipboard"), "{detail}");
        assert_eq!(
            copy.seen.lock().unwrap().clone(),
            Some(json!({"text": "hello (edited)"})),
            "the clipboard gets exactly what the user confirmed"
        );
    }

    #[test]
    fn a_failed_replace_copies_the_edited_text_with_the_selections_line_endings() {
        // #694: an edited multi-line value must not land on the clipboard
        // with LF when the selection was CRLF.
        let replace = Fake::new("replace_text", Err(anyhow::anyhow!("the text changed")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let plan = ReplaceSelection {
            target: Some(target()),
            selected_text: "a\r\nb".into(),
        };
        let report = replace_or_copy(
            &replace,
            &copy,
            Some(&plan),
            confirmed(json!({"text": "x\ny\nz"})),
        );
        assert!(matches!(report, ReplaceReport::Copied(_)), "{report:?}");
        assert_eq!(
            copy.seen.lock().unwrap().clone(),
            Some(json!({"text": "x\r\ny\r\nz"}))
        );
    }

    #[test]
    fn a_none_target_plan_copies_matched_text_and_never_calls_the_replace_executor() {
        let replace = Fake::new("replace_text", Ok(Undo::none("wrote it")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let plan = ReplaceSelection {
            target: None,
            selected_text: "a\r\nb".into(),
        };
        let report = replace_or_copy(
            &replace,
            &copy,
            Some(&plan),
            confirmed(json!({"text": "x\ny"})),
        );
        assert_eq!(report, ReplaceReport::Copied(COPIED_INSTEAD_DETAIL.into()));
        assert!(!replace.ran());
        assert_eq!(
            copy.seen.lock().unwrap().clone(),
            Some(json!({"text": "x\r\ny"}))
        );
    }

    #[test]
    fn no_plan_copies_without_trying_to_replace() {
        let replace = Fake::new("replace_text", Ok(Undo::none("wrote it")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let report = replace_or_copy(&replace, &copy, None, confirmed(json!({"text": "hello"})));
        assert_eq!(report, ReplaceReport::Copied(COPIED_INSTEAD_DETAIL.into()));
        assert!(!replace.ran());
    }

    #[test]
    fn blank_text_replaces_nothing_and_never_blanks_the_clipboard() {
        for v in [json!({"text": ""}), json!({"text": "  "}), json!({})] {
            let replace = Fake::new("replace_text", Ok(Undo::none("x")));
            let copy = Fake::new("clipboard", Ok(Undo::none("x")));
            let plan = replace_plan(&gathered());
            let report = replace_or_copy(&replace, &copy, plan.as_deref(), confirmed(v.clone()));
            assert_eq!(report, ReplaceReport::NothingToReplace, "{v}");
            assert!(!replace.ran() && !copy.ran(), "{v}");
        }
    }

    #[test]
    fn when_the_copy_fails_too_the_report_is_a_failure_naming_both() {
        let replace = Fake::new("replace_text", Err(anyhow::anyhow!("no value pattern")));
        let copy = Fake::new("clipboard", Err(anyhow::anyhow!("clipboard busy")));
        let plan = replace_plan(&gathered());
        let report = replace_or_copy(
            &replace,
            &copy,
            plan.as_deref(),
            confirmed(json!({"text": "hello"})),
        );
        let ReplaceReport::Failed(why) = report else {
            panic!("expected a failure, got {report:?}");
        };
        assert!(
            why.contains("no value pattern") && why.contains("clipboard busy"),
            "{why}"
        );
    }

    #[test]
    fn the_preview_says_when_it_will_copy_and_a_cut_selection_is_refused_up_front() {
        assert_eq!(
            preview_title("Translate selection", true),
            "Translate selection"
        );
        assert_eq!(
            preview_title("Translate selection", false),
            "Translate selection (copies to the clipboard)"
        );
        let mut cut = gathered();
        cut.selection_truncated = true;
        assert!(too_long(&cut));
        assert!(!too_long(&gathered()));
        assert!(!TOO_LONG_MESSAGE.contains('\u{2014}'));
    }
}
