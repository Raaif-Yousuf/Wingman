//! Code actions on a selection (#73): select code in an editor and ask for an
//! explanation, a fix or a set of tests.
//!
//! Three generic actions in the Code group, all on the selection only (code
//! cannot be read reliably off a screenshot, and a fix has to be written back
//! over something):
//!
//! - **Explain this code**: a read-only answer on the card (`text_answer`,
//!   executor `"none"`). The first line is the headline, the rest shows on
//!   expand.
//! - **Fix this code**: the corrected selection on the preview card
//!   (editable), and on "Do it" it replaces the selection in place
//!   (`replace_text`, like "Translate selection"). Where the control cannot be
//!   written back through, the confirmed text is copied instead and the card
//!   says so.
//! - **Write tests for this code**: the tests go on the clipboard after the
//!   user confirms (`clipboard`), never over the code. Inserting them next to
//!   the code needs an insert executor, which does not exist yet.
//!
//! Nothing is ever sent from the editor and nothing is run.

use super::{Action, InputKind, Prefer};

/// The ids of the three actions, in palette order.
pub const EXPLAIN_ID: &str = "explain-this-code";
pub const FIX_ID: &str = "fix-this-code";
pub const TESTS_ID: &str = "write-tests-for-code";

/// The explain prompt. The first line becomes the card headline, so it is
/// fixed to one short sentence.
pub const EXPLAIN_PROMPT: &str = "You are given a piece of code the user selected. Explain what it does to a developer who has not seen it. Return it in `text`. The first line is one plain sentence of at most 85 characters saying what the code does. After a blank line, give a short walk through of the steps in plain sentences, and mention anything surprising, risky or likely to break. Do not rewrite the code and do not paste large parts of it back; quote a name or a short expression only when it helps. Never invent functions, files or libraries that the selection does not show. Use plain text only: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The fix prompt. Returning the whole selection (not a diff) is what lets it
/// replace the selection as is; changing as little as possible keeps the
/// review on the card short.
pub const FIX_PROMPT: &str = "You are given a piece of code the user selected. Find the bug or bugs in it and return the whole corrected code in `text`, so it can replace the selection as is. Change as little as possible: keep the names, structure, comments, formatting and style of the original, and fix only what is wrong. Do not add features, refactor, or explain. If you find nothing wrong, return the code unchanged. Return only the code: no markdown fences, no notes, no explanation before or after. Use the same programming language as the code. Never invent functions, files or libraries that the selection does not show.";

/// The tests prompt. The tests land on the clipboard, so a complete,
/// paste-ready block is the goal.
pub const TESTS_PROMPT: &str = "You are given a piece of code the user selected. Write unit tests for it and return them in `text`, ready to paste. Use the test framework that fits the language and that the code already suggests (for example pytest for Python, Jest for JavaScript or TypeScript, the built-in test module for Rust, JUnit for Java, xUnit for C#); if the code gives no hint, pick the most common one. Cover the normal cases, the edge cases and the error cases you can see in the code. Give each test a clear name and keep each one short. Do not include the code under test itself. Return only the test code: no markdown fences, no notes, no explanation before or after. Use the same programming language as the code. Never invent functions, files or libraries that the selection does not show.";

/// Shown instead of calling the model when the selection was cut to the
/// length limit: an explanation, a fix or tests for part of the code would
/// read as if they covered all of it. (Fix is also refused by the generic
/// replace path with its own message.)
pub const TOO_LONG_MESSAGE: &str =
    "The selection is too long to read in full. Select less code and try again.";

/// Whether `action_id` is one of the code actions, which refuse a selection
/// that was cut to the length limit.
pub fn refuses_cut_selection(action_id: &str) -> bool {
    [EXPLAIN_ID, FIX_ID, TESTS_ID].contains(&action_id)
}

fn code_action(id: &str, name: &str, executor: &str, confirm: bool, prompt: &str) -> Action {
    Action {
        id: id.to_string(),
        name: name.to_string(),
        group: Some("Code".to_string()),
        inputs: vec![InputKind::Selection],
        proposal: "text_answer".to_string(),
        executor: executor.to_string(),
        confirm,
        prompt: prompt.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        output: crate::actions::Output::Card,
        detector: None,
        enabled: true,
    }
}

/// The three built-in code actions: group Code, the selection only, proposal
/// `text_answer`. Explain is read-only (`none`, no confirm); Fix writes
/// (`replace_text`, confirms); Tests copies (`clipboard`, confirms).
pub fn builtin_actions() -> Vec<Action> {
    vec![
        code_action(
            EXPLAIN_ID,
            "Explain this code",
            "none",
            false,
            EXPLAIN_PROMPT,
        ),
        code_action(FIX_ID, "Fix this code", "replace_text", true, FIX_PROMPT),
        code_action(
            TESTS_ID,
            "Write tests for this code",
            "clipboard",
            true,
            TESTS_PROMPT,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executors::Effect;

    fn by_id(id: &str) -> Action {
        builtin_actions().into_iter().find(|a| a.id == id).unwrap()
    }

    #[test]
    fn there_are_three_distinct_code_actions_on_the_selection() {
        let actions = builtin_actions();
        let ids: Vec<&str> = actions.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(ids, [EXPLAIN_ID, FIX_ID, TESTS_ID]);
        for a in &actions {
            assert_eq!(a.inputs, vec![InputKind::Selection], "{}", a.id);
            assert_eq!(a.group.as_deref(), Some("Code"), "{}", a.id);
            assert_eq!(a.proposal, "text_answer", "{}", a.id);
            assert!(a.enabled);
            assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        }
        let prompts: std::collections::HashSet<&str> =
            actions.iter().map(|a| a.prompt.as_str()).collect();
        assert_eq!(prompts.len(), 3, "each action has its own prompt");
    }

    #[test]
    fn every_code_action_refuses_a_cut_selection_and_others_do_not() {
        for id in [EXPLAIN_ID, FIX_ID, TESTS_ID] {
            assert!(refuses_cut_selection(id), "{id}");
        }
        assert!(!refuses_cut_selection("translate-selection"));
        assert!(!refuses_cut_selection(""));
        assert!(!TOO_LONG_MESSAGE.contains('\u{2014}'));
    }

    #[test]
    fn explain_is_a_read_only_answer() {
        let a = by_id(EXPLAIN_ID);
        assert_eq!(a.executor, "none");
        assert!(!a.confirm);
        let executor = crate::actions::resolve_executor(&a).expect("none resolves");
        assert_eq!(executor.effect(), Effect::ReadOnly);
    }

    #[test]
    fn fix_confirms_before_it_writes_over_the_selection() {
        let a = by_id(FIX_ID);
        assert_eq!(a.executor, "replace_text");
        assert!(a.confirm, "it writes into another app, so it must confirm");
        let executor = crate::actions::resolve_executor(&a).expect("replace_text resolves");
        assert_eq!(executor.effect(), Effect::Writes);
    }

    #[test]
    fn tests_go_to_the_clipboard_after_a_confirm_never_over_the_code() {
        let a = by_id(TESTS_ID);
        assert_eq!(a.executor, "clipboard");
        assert!(a.confirm);
        assert_ne!(a.executor, "replace_text");
    }

    #[test]
    fn the_prompts_say_what_each_action_returns() {
        assert!(EXPLAIN_PROMPT.contains("first line"));
        assert!(FIX_PROMPT.contains("replace the selection as is"));
        assert!(FIX_PROMPT.contains("unchanged"));
        assert!(TESTS_PROMPT.contains("Do not include the code under test"));
        for p in [EXPLAIN_PROMPT, FIX_PROMPT, TESTS_PROMPT] {
            assert!(!p.contains('\u{2014}'), "no em dash in model text");
            assert!(p.contains("Never invent"), "{p}");
        }
    }

    #[test]
    fn the_explain_card_headline_is_the_first_line() {
        let proposal = serde_json::json!({
            "text": "Sums the even numbers in a list.\n\nIt loops over the list and skips odd values."
        });
        let card = crate::actions::generic_answer::result_card_text(
            "Explain this code",
            "none",
            "text_answer",
            "",
            &proposal,
        );
        assert_eq!(card.headline, "Sums the even numbers in a list.");
        assert!(card.detail.contains("It loops over the list"));
    }

    #[test]
    fn the_actions_are_visible_by_default_and_dispatch_generically() {
        let shown = crate::actions::visible(
            crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]),
            &[],
        );
        for id in [EXPLAIN_ID, FIX_ID, TESTS_ID] {
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
