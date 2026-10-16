//! "Define this word" (#75): select a word or short phrase and the card says
//! what it means, with an example sentence.
//!
//! A generic action over the selection only. It has no `screen` input on
//! purpose: the selected text alone goes to the model, so a text-only local
//! model can answer it (a screenshot would need a vision model). Which model
//! answers is decided by the configured mode and providers, as for every
//! action. The proposal is `word_definition` and the executor is
//! `"none"`, so the model's answer is the card (#441) and nothing is written
//! anywhere. `prefer.mode = "local"` records that a local model suits this
//! action; nothing reads `prefer.mode` yet (see `docs/actions.md`).

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), the same as an `actions.toml` entry.
pub const ACTION_ID: &str = "define-this-word";

/// The action's prompt. The card headline is the `definition` field, so the
/// prompt asks for it to lead with the word and its part of speech.
pub const BASE_PROMPT: &str = "You are given a word or short phrase the user selected. Define it for a general reader. Fill the fields in this order. definition: the word as written, its part of speech in brackets, a colon, then a plain definition in one short sentence, at most 90 characters in total (for example \"ephemeral (adjective): lasting a very short time\"). example: one short sentence that uses the word naturally. If the selection has several common meanings, give the most common one. If it is an idiom or phrase, define the phrase. If the selection is a long passage, define only its single hardest word. If it is not a real word or phrase, say so in definition and write \"None\" in example. Never invent a meaning. Use plain text only in every field: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Study, the selection only, proposal
/// `word_definition`, executor `none`, `confirm = false` (read-only: the
/// answer is the card).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Define this word".to_string(),
        group: Some("Study".to_string()),
        inputs: vec![InputKind::Selection],
        proposal: "word_definition".to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer {
            mode: "local".to_string(),
        },
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
    fn the_entry_is_a_read_only_answer_over_the_selection_alone() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(
            a.inputs,
            vec![InputKind::Selection],
            "no screen input: a text-only local model must be able to answer it"
        );
        assert_eq!(a.proposal, "word_definition");
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "a read-only answer needs no preview");
        assert_eq!(a.prefer.mode, "local");
        assert!(!a.prompt.contains('\u{2014}'), "no em dash in model text");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
    }

    #[test]
    fn the_prompt_names_both_fields_and_forbids_invented_meanings() {
        for needle in ["definition:", "example:", "90 characters", "Never invent"] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
    }

    #[test]
    fn a_selection_is_sent_as_text_with_no_screenshot_and_nothing_selected_is_a_card() {
        use crate::actions::generic_inputs::{plan, Gathered};
        let a = builtin_action();
        let with = Gathered {
            selection: Some("ephemeral".into()),
            ..Gathered::default()
        };
        let sent = plan(&a.inputs, &with).unwrap();
        assert!(!sent.screen, "a screenshot would need a vision model");
        assert!(sent.user.contains("ephemeral"));
        let err = plan(&a.inputs, &Gathered::default()).unwrap_err();
        assert!(err.contains("Nothing is selected"));
    }

    #[test]
    fn the_card_leads_with_the_definition_and_shows_the_example() {
        let card = result_card_text(
            "Define this word",
            "none",
            "word_definition",
            "nothing to do",
            &json!({
                "definition": "ephemeral (adjective): lasting a very short time",
                "example": "The fog was ephemeral and gone by nine."
            }),
        );
        assert_eq!(
            card.headline,
            "ephemeral (adjective): lasting a very short time"
        );
        assert_eq!(
            card.detail,
            "Definition: ephemeral (adjective): lasting a very short time\nExample: The fog was ephemeral and gone by nine."
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
