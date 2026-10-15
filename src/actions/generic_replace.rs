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

/// What a replace needs from "Look": the selection's UIA identity and the
/// text that was selected (what the model was shown). `replace_plan` boxes
/// it, because it is much bigger than the rest of a message payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplaceSelection {
    pub target: SelectionTarget,
    pub selected_text: String,
}

/// What the card says when the text could not be replaced in place and was
/// copied instead.
pub const COPIED_INSTEAD_DETAIL: &str =
    "Wingman could not replace the text in place here, so the result is on the clipboard.";

/// The replace to offer for what was gathered, or `None` when the text
/// cannot be written back safely (the caller then copies it instead).
/// `None` for: no UIA target (a clipboard-fallback or value-only control),
/// a selection cut to the length limit (replacing it would drop the rest),
/// a password field, and a blank selection.
pub fn replace_plan(gathered: &Gathered) -> Option<Box<ReplaceSelection>> {
    if gathered.selection_in_password_field || gathered.selection_truncated {
        return None;
    }
    let target = gathered.selection_target.clone()?;
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
/// (blank text would delete the selection).
pub fn review_context(plan: &ReplaceSelection, confirmed: &Value) -> Option<ReviewContext> {
    let new_text = confirmed.get("text")?.as_str()?;
    if new_text.trim().is_empty() {
        return None;
    }
    Some(ReviewContext {
        target: ReplaceTarget::Selection(plan.target.clone()),
        original_text: plan.selected_text.clone(),
        new_text: new_text.to_string(),
    })
}

/// Shown instead of calling the model when the selection was cut to the
/// length limit: a translation of part of it could not replace all of it.
pub const TOO_LONG_MESSAGE: &str =
    "The selection is too long to replace in place. Select less text and try again.";

/// Whether the selection was cut to the length limit.
pub fn too_long(gathered: &Gathered) -> bool {
    gathered.selection_truncated
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
            Some(r) => format!("{r}. Copying the text also failed: {e:#}"),
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
    fn a_readable_selection_with_a_target_can_be_replaced() {
        let plan = replace_plan(&gathered()).expect("replaceable");
        assert_eq!(plan.target, target());
        assert_eq!(plan.selected_text, "bonjour");
    }

    #[test]
    fn no_target_a_cut_selection_a_password_or_a_blank_selection_are_not_replaced() {
        let mut no_target = gathered();
        no_target.selection_target = None;
        let mut cut = gathered();
        cut.selection_truncated = true;
        let mut password = gathered();
        password.selection_in_password_field = true;
        let mut blank = gathered();
        blank.selection = Some("  \n".into());
        let mut none = gathered();
        none.selection = None;
        for (name, g) in [
            ("no target", no_target),
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
