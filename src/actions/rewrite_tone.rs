//! "Rewrite for tone" (#72): select text in any app, pick a tone, see the
//! rewrite on the preview card (editable), and on "Do it" it replaces the
//! selection in place.
//!
//! Three fixed-tone actions (shorter, friendlier, more formal) rather than
//! one action with a tone parameter: the palette has no per-action options,
//! and each one is a plain generic action, so it needs nothing a hand-written
//! `actions.toml` entry lacks. Like "Translate selection" the proposal is
//! `text_answer`, the executor is `"replace_text"` and `confirm = true`
//! because it writes; the selection is the only input. Where the control
//! cannot be written back through, the confirmed text is copied to the
//! clipboard instead and the card says so (`actions::generic_replace`).
//! Nothing is ever sent.

use super::{Action, InputKind, Prefer};

/// The ids of the three actions, in palette order.
pub const SHORTER_ID: &str = "rewrite-shorter";
pub const FRIENDLIER_ID: &str = "rewrite-friendlier";
pub const FORMAL_ID: &str = "rewrite-formal";
/// "Simplify this text" (#1278): a plain-language rewrite, listed after the
/// tones.
pub const SIMPLIFY_ID: &str = "simplify-text";

/// The shared prompt start. Only the rewrite comes back, so it can replace
/// the selection as is. Keeping facts and names is the load-bearing part: a
/// tone change must never change what the text says.
const PROMPT_START: &str = "You are given a piece of text the user selected. Rewrite it ";

/// The shared prompt end.
const PROMPT_END: &str = " Keep the meaning, facts, numbers, names, dates, URLs and the language of the original exactly: change only the wording and tone, and do not add or remove information. Keep the line breaks and formatting characters where they still make sense. Return only the rewritten text in `text`, with no notes, quotation marks or explanation. Use plain text only: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes unless the original text has them.";

/// What one tone asks for, between the shared start and end.
fn prompt(tone: &str) -> String {
    format!("{PROMPT_START}{tone}{PROMPT_END}")
}

fn tone_action(id: &str, name: &str, tone: &str) -> Action {
    Action {
        id: id.to_string(),
        name: name.to_string(),
        group: Some("Writing".to_string()),
        inputs: vec![InputKind::Selection],
        proposal: "text_answer".to_string(),
        executor: "replace_text".to_string(),
        confirm: true,
        prompt: prompt(tone),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        output: crate::actions::Output::Card,
        detector: None,
        enabled: true,
    }
}

/// The three built-in tone actions: group Writing, the selection only,
/// proposal `text_answer`, executor `replace_text`, `confirm = true`.
pub fn builtin_actions() -> Vec<Action> {
    vec![
        tone_action(
            SHORTER_ID,
            "Rewrite: shorter",
            "to be shorter and tighter. Cut filler and repetition, keep every point that matters, and do not change the tone.",
        ),
        tone_action(
            FRIENDLIER_ID,
            "Rewrite: friendlier",
            "in a warmer, friendlier tone. Sound natural and kind, not gushing, and do not make it much longer.",
        ),
        tone_action(
            FORMAL_ID,
            "Rewrite: more formal",
            "in a more formal, professional tone. Avoid slang, contractions and casual phrases, and do not make it much longer.",
        ),
        simplify_action(),
    ]
}

/// "Simplify this text": plain words and short sentences at a lower reading
/// level. Same proposal and executor as the tones. Its own prompt, because
/// the shared end tells the model to change only the wording and tone, and
/// this one must also say when meaning would be lost.
fn simplify_action() -> Action {
    let mut a = tone_action(SIMPLIFY_ID, "Simplify this text", "");
    a.prompt = "You are given a piece of text the user selected, such as a policy, a contract clause or a paper abstract. Rewrite it in plain words and short sentences, at a lower reading level, so a general reader understands it the first time. Replace jargon and long phrases with everyday words, and split long sentences. Keep the meaning, facts and the language of the original, and keep every number, name and date exactly as written. Do not add information that is not in the text. If part of it cannot be simplified without losing meaning, keep that part as written and add a last line starting with \"Note:\" that says which part and why. Keep the line breaks and formatting characters where they still make sense. Return only the rewritten text in `text`, with no quotation marks or explanation. Use plain text only: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes unless the original text has them.".to_string();
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn there_are_three_distinct_tone_actions() {
        let actions = builtin_actions();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, [SHORTER_ID, FRIENDLIER_ID, FORMAL_ID, SIMPLIFY_ID]);
        let prompts: std::collections::HashSet<&str> =
            actions.iter().map(|a| a.prompt.as_str()).collect();
        assert_eq!(prompts.len(), 4, "each tone has its own prompt");
        let names: std::collections::HashSet<&str> =
            actions.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names.len(), 4);
    }

    #[test]
    fn each_entry_confirms_then_replaces_the_selection() {
        for a in builtin_actions() {
            assert_eq!(a.inputs, vec![InputKind::Selection], "{}", a.id);
            assert_eq!(a.proposal, "text_answer", "{}", a.id);
            assert_eq!(a.executor, "replace_text", "{}", a.id);
            assert_eq!(a.group.as_deref(), Some("Writing"), "{}", a.id);
            assert!(
                a.confirm,
                "{} writes into another app, so it must confirm",
                a.id
            );
            assert!(a.enabled);
            assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
            let executor = crate::actions::resolve_executor(&a).expect("replace_text resolves");
            assert_eq!(
                executor.effect(),
                crate::executors::Effect::Writes,
                "{}",
                a.id
            );
        }
    }

    #[test]
    fn the_prompts_name_their_tone_and_keep_the_facts() {
        let by_id = |id: &str| {
            builtin_actions()
                .into_iter()
                .find(|a| a.id == id)
                .unwrap()
                .prompt
        };
        assert!(by_id(SHORTER_ID).contains("shorter"));
        assert!(by_id(FRIENDLIER_ID).contains("friendlier"));
        assert!(by_id(FORMAL_ID).contains("formal"));
        for a in builtin_actions() {
            for needle in ["Keep the meaning, facts", "only the rewritten text"] {
                assert!(a.prompt.contains(needle), "{} lacks {needle}", a.id);
            }
            assert!(!a.prompt.contains('\u{2014}'), "no em dash in model text");
        }
    }

    #[test]
    fn simplify_asks_for_plain_words_and_keeps_every_fact() {
        let p = builtin_actions()
            .into_iter()
            .find(|a| a.id == SIMPLIFY_ID)
            .unwrap()
            .prompt;
        for needle in [
            "plain words",
            "short sentences",
            "reading level",
            "cannot be simplified without losing meaning",
            "every number, name and date exactly",
        ] {
            assert!(p.contains(needle), "simplify prompt lacks {needle}");
        }
        assert!(!p.contains('\u{2014}'));
    }

    #[test]
    fn simplify_sits_right_after_formal() {
        let ids: Vec<String> = crate::actions::builtin_actions()
            .into_iter()
            .map(|a| a.id)
            .collect();
        let at = ids.iter().position(|i| i == FORMAL_ID).unwrap();
        assert_eq!(ids[at + 1], SIMPLIFY_ID);
    }

    #[test]
    fn the_actions_are_visible_by_default_and_dispatch_generically() {
        let shown = crate::actions::visible(
            crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]),
            &[],
        );
        for id in [SHORTER_ID, FRIENDLIER_ID, FORMAL_ID, SIMPLIFY_ID] {
            assert!(shown.iter().any(|r| r.action.id == id), "{id}");
            assert_eq!(
                crate::ui::palette_model::dispatch_target_for(id),
                Some(crate::ui::palette_model::DispatchTarget::Generic(
                    id.to_string()
                ))
            );
        }
    }
}
