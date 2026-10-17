//! "Explain this error" (#66): read an error message on screen (a dialog, a
//! stack trace, a failed build) and answer on the card with what it means,
//! the likely cause and the fix.
//!
//! Nothing here needs new Rust beyond the entry itself. It is a generic
//! action: the selected text when there is one (a trace the user
//! highlighted), the screen otherwise (a dialog that cannot be selected).
//! The proposal is `error_explanation` and the executor is `"none"`, so the model's
//! answer is the card (#441) and nothing is written anywhere.

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), the same as an `actions.toml` entry.
pub const ACTION_ID: &str = "explain-this-error";

/// The action's prompt. The card headline is the first line of `text`, so
/// the prompt fixes a short first line and a plain layout for the rest.
pub const BASE_PROMPT: &str = "You are shown an error message: an error dialog, a stack trace, a failed build or a log line, as text or as a screenshot of the screen. Find the error and explain it to someone who has not seen it before. Fill the fields in this order. meaning: what the error means, in one plain sentence of at most 85 characters. cause: the most likely cause, in one or two sentences. fix: the first thing to try, in one or two sentences, with a command or setting name when there is one. Quote error codes and file names exactly as shown and never invent one. If several errors are visible, explain the one in the foreground dialog or the first one in the trace. If you are not sure of the cause, say so in the cause field rather than guessing. If no error is visible, say so in meaning and write \"None\" in cause and fix. Use plain text only in every field: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `error_explanation`, executor `none`, `confirm =
/// false` (read-only: the answer is the card).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Explain this error".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
        proposal: "error_explanation".to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        output: crate::actions::Output::Card,
        detector: None,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::generic_answer::result_card_text;
    use serde_json::json;

    #[test]
    fn the_entry_is_a_read_only_answer_over_selection_then_screen() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, "error_explanation");
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "a read-only answer needs no preview");
        assert!(!a.prompt.contains('\u{2014}'), "no em dash in model text");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
    }

    #[test]
    fn the_prompt_names_the_three_fields_and_never_invent() {
        for needle in [
            "meaning:",
            "cause:",
            "fix:",
            "85 characters",
            "never invent",
        ] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
    }

    #[test]
    fn the_card_shows_the_meaning_as_the_headline_and_all_three_fields() {
        let card = result_card_text(
            "Explain this error",
            "none",
            "error_explanation",
            "nothing to do",
            &json!({
                "meaning": "The file is locked by another program.",
                "cause": "Another app has it open.",
                "fix": "Close that app and retry."
            }),
        );
        assert_eq!(card.headline, "The file is locked by another program.");
        assert_eq!(
            card.detail,
            "Meaning: The file is locked by another program.\nCause: Another app has it open.\nFix: Close that app and retry."
        );
    }

    #[test]
    fn the_action_is_visible_by_default_and_dispatches_generically() {
        let shown = crate::actions::visible(
            crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]),
            &[],
        );
        assert!(shown.iter().any(|r| r.action.id == ACTION_ID));
        assert_eq!(
            crate::ui::palette_model::dispatch_target_for(ACTION_ID),
            Some(crate::ui::palette_model::DispatchTarget::Generic(
                ACTION_ID.to_string()
            ))
        );
    }
}
