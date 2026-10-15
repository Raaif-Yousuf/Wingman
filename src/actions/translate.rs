//! "Translate selection" (#68): select text in any app, run the action, see
//! the English translation on the preview card (editable), and on "Do it"
//! the translation replaces the selection in place.
//!
//! A generic action: proposal `text_answer`, executor `"replace_text"`, with
//! `confirm = true` because it writes. The selection is the only input (no
//! screen fallback: there would be nothing to write over). Where the control
//! cannot be written back through (no UIA text range, or a selection cut to
//! the length limit) the confirmed text is copied to the clipboard instead
//! and the card says so (`actions::generic_replace`). Nothing is ever sent.
//! To translate into another language, copy this block into `actions.toml`
//! and change the prompt.

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), the same as an `actions.toml` entry.
pub const ACTION_ID: &str = "translate-selection";

/// The action's prompt. Only the translation comes back, so it can replace
/// the selection as is.
pub const BASE_PROMPT: &str = "You are given a piece of text the user selected. Translate it into English and return only the translation in `text`. Keep the meaning, tone, line breaks, numbers, names, URLs and formatting characters exactly as they were. Do not add notes, quotation marks, a language name or an explanation. If the text is already in English, return it unchanged. Use plain text only: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes unless the original text has them.";

/// The built-in action: group Writing, the selection only, proposal
/// `text_answer`, executor `replace_text`, `confirm = true`.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Translate selection".to_string(),
        group: Some("Writing".to_string()),
        inputs: vec![InputKind::Selection],
        proposal: "text_answer".to_string(),
        executor: "replace_text".to_string(),
        confirm: true,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_confirms_then_replaces_the_selection() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.inputs, vec![InputKind::Selection]);
        assert_eq!(a.proposal, "text_answer");
        assert_eq!(a.executor, "replace_text");
        assert!(a.confirm, "it writes into another app, so it must confirm");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        let executor = crate::actions::resolve_executor(&a).expect("replace_text resolves");
        assert_eq!(executor.effect(), crate::executors::Effect::Writes);
    }

    #[test]
    fn the_prompt_asks_for_the_translation_alone() {
        for needle in [
            "Translate it into English",
            "only the translation",
            "unchanged",
        ] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
        assert!(
            !BASE_PROMPT.contains('\u{2014}'),
            "no em dash in model text"
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
