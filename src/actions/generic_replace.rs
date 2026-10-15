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
use super::review_email::{ReplaceTarget, ReviewContext};
use crate::inputs::selection::SelectionTarget;

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
}
