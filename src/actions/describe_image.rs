//! "Describe this image" (#113): one press on a chart, photo or diagram and
//! the card offers one sentence of alt text, ready to copy.
//!
//! A generic action over the screen. It lists `screen` only: `region` would
//! capture the same active monitor (`actions::generic_inputs`), so naming it
//! would promise a crop that does not happen. The model needs vision, so
//! this works with a vision-capable model, cloud or local. The proposal is
//! `text_answer` and the executor is `"clipboard"` with `confirm = true`:
//! the confirm card shows the alt text and lets the user fix a word, and
//! "Do it" copies it. The text is on the card before anything is copied, so
//! nothing the model wrote is lost if the copy is declined.

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), the same as an `actions.toml` entry.
pub const ACTION_ID: &str = "describe-image-alt-text";

/// The action's prompt. 125 characters is the usual screen-reader cutoff for
/// alt text. The alt text comes back in `text` so the clipboard
/// executor copies it as is.
pub const BASE_PROMPT: &str = "You are shown a screenshot that contains an image, chart, graph, photo or diagram. Pick the main one: the largest, or the one in the foreground. Write alt text for it in `text`: one plain sentence of at most 125 characters, for someone who cannot see it. Say what it shows and, for a chart or graph, the one thing it says (the trend or the biggest value, and what is measured). Do not start with \"image of\", \"picture of\" or \"chart of\". Use only what is visible: never guess names, numbers or places that are not shown. If there is no image, chart or photo on the screen, say so in one sentence. Use plain text only: no markdown, no quotation marks around the sentence, no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Work, the screen, proposal `text_answer`,
/// executor `clipboard`, `confirm = true` (the user can edit the sentence
/// before it is copied).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Describe image (alt text)".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Screen],
        proposal: "text_answer".to_string(),
        executor: "clipboard".to_string(),
        confirm: true,
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
    use serde_json::json;

    #[test]
    fn the_entry_confirms_then_copies_a_screen_description() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(
            a.inputs,
            vec![InputKind::Screen],
            "region would not crop, so it is not listed"
        );
        assert_eq!(a.proposal, "text_answer");
        assert_eq!(a.executor, "clipboard");
        assert!(
            a.confirm,
            "the user checks the sentence before it is copied"
        );
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the clipboard executor resolves");
    }

    #[test]
    fn the_prompt_asks_for_one_short_sentence_and_no_guessing() {
        for needle in [
            "one plain sentence",
            "125 characters",
            "`text`",
            "image of",
            "never guess",
            "no image",
        ] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
        assert!(
            !BASE_PROMPT.contains('\u{2014}'),
            "no em dash in model text"
        );
    }

    #[test]
    fn the_proposal_has_the_field_the_clipboard_executor_copies() {
        // The shape `text_answer`'s schema produces: one `text` string, which
        // is the field the clipboard executor copies.
        let schema = crate::actions::schema::schema_for("text_answer", false).unwrap();
        assert_eq!(schema["required"], json!(["text"]));
    }

    #[test]
    fn the_example_file_prompt_is_the_built_in_prompt() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("actions.example.toml");
        let text = std::fs::read_to_string(path).unwrap().replace('\r', "");
        let file = crate::actions::parse_actions_file(&text).unwrap();
        let a = file
            .actions
            .iter()
            .find(|a| a.id == ACTION_ID)
            .expect("the example file lists describe-image-alt-text");
        assert!(!a.prompt.is_empty());
        assert_eq!(a.prompt.replace('\r', ""), BASE_PROMPT);
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
