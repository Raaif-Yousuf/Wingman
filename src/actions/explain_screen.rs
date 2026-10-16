//! "Explain what's on screen" (#131): the action that works on any screen. A
//! dialog gets what it asks and what each choice does, an article gets what
//! it is about, code gets what it does.
//!
//! A generic action over the selection first and the screen otherwise. The
//! proposal is `screen_explanation`: the model writes `detail` first (a few
//! plain sentences), then `headline` (one sentence that sums it up), the same
//! order and reason as the verdict card (see `actions::schema`). The executor
//! is `"none"`, so the card is the result: the headline is the collapsed
//! card and `detail` is what expands. [`card_text`] turns the proposal into
//! card text and holds the length limits in code, not only in the prompt, so
//! the headline always fits the collapsed card's 3-line budget
//! (`ui::card::HEADLINE_MAX_LINES`).
//!
//! It is a plain action. Offering it as the fallback option on every screen
//! belongs to the options card (#305); until then it is one palette entry.

use serde_json::Value;

use super::generic_answer::CardText;
use super::{Action, InputKind, Prefer};
use crate::provider::sanitize_model_text;
use crate::ui::card::{HEADLINE_CHARS_PER_LINE, HEADLINE_MAX_LINES};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "explain-this-screen";

/// The proposal kind (`actions::schema::schema_for` key).
pub const PROPOSAL_KIND: &str = "screen_explanation";

/// Longest headline, in characters: the collapsed card's whole headline
/// budget, less a little slack for word wrap.
pub const MAX_HEADLINE_CHARS: usize = HEADLINE_MAX_LINES as usize * HEADLINE_CHARS_PER_LINE - 15;
/// Longest detail, in characters.
pub const MAX_DETAIL_CHARS: usize = 700;

/// What the card says when there is nothing to explain.
pub const NOTHING_HEADLINE: &str = "Nothing to explain";

/// The action's prompt. The limits are asked for here and enforced in
/// [`card_text`].
pub const BASE_PROMPT: &str = "You are shown what is on the user's screen, as text if some was selected and as a screenshot otherwise. Explain it to someone who has not looked at it closely. First fill `detail`: two to four short plain sentences. If it is a dialog or a message, say what it asks or says and what each choice would do. If it is an error, say what it means and what to try. If it is an article or a page, say what it is about and its main point. If it is code, say what it does. If it is anything else, say what it is and what it is for. Then fill `headline`: one sentence of at most 120 characters that sums up `detail`. Use only what is shown: never guess names, numbers or consequences that are not visible. Quote error codes and file names exactly as shown. Do not tell the user to press, click or submit anything. If the screen is blank or unreadable, return an empty `headline` and an empty `detail`. Use plain text only: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `screen_explanation`, executor `none`,
/// `confirm = false` (read-only: the answer is the card).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Explain what's on screen".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
        proposal: PROPOSAL_KIND.to_string(),
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

/// One model string as clean text: lines are trimmed, runs of blank lines
/// collapse, em dashes are replaced, and the result is cut to `max`
/// characters with a trailing ellipsis.
fn clean_text(v: Option<&Value>, max: usize) -> String {
    let joined = v
        .and_then(Value::as_str)
        .unwrap_or("")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    sanitize_model_text(&joined, max)
}

/// The card text for a `screen_explanation` proposal. The headline is the
/// model's one-sentence summary on one line; the detail is the longer
/// explanation. With no headline, the first line of the detail stands in (the
/// detail stays whole); with neither, the card says [`NOTHING_HEADLINE`].
pub fn card_text(proposal: &Value) -> CardText {
    let detail = clean_text(proposal.get("detail"), MAX_DETAIL_CHARS);
    let headline = clean_text(proposal.get("headline"), MAX_HEADLINE_CHARS).replace('\n', " ");
    if headline.is_empty() && detail.is_empty() {
        return CardText {
            headline: NOTHING_HEADLINE.to_string(),
            detail: String::new(),
        };
    }
    if headline.is_empty() {
        let first = detail.lines().next().unwrap_or_default();
        return CardText {
            headline: sanitize_model_text(first, MAX_HEADLINE_CHARS),
            detail,
        };
    }
    CardText { headline, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_headline_is_collapsed_and_the_detail_expands() {
        let c = card_text(&json!({
            "detail": "The dialog says the file is open in another program. Close that program and try again.",
            "headline": "Windows cannot delete the file because another program uses it."
        }));
        assert_eq!(
            c.headline,
            "Windows cannot delete the file because another program uses it."
        );
        assert_eq!(
            c.detail,
            "The dialog says the file is open in another program. Close that program and try again."
        );
    }

    #[test]
    fn the_longest_headline_fits_the_collapsed_cards_budget() {
        let wrapped = |n: usize| n.div_ceil(HEADLINE_CHARS_PER_LINE);
        assert!(wrapped(MAX_HEADLINE_CHARS) <= HEADLINE_MAX_LINES as usize);
        let c = card_text(&json!({"headline": "word ".repeat(200), "detail": "d"}));
        assert!(c.headline.chars().count() <= MAX_HEADLINE_CHARS);
        assert!(c.headline.ends_with('\u{2026}'));
        // The prompt asks for less than the code allows.
        assert!(BASE_PROMPT.contains("at most 120 characters"));
        const PROMPT_ASKS: usize = 120;
        const _: () = assert!(PROMPT_ASKS <= MAX_HEADLINE_CHARS);
    }

    #[test]
    fn a_long_detail_is_cut_with_an_ellipsis_and_keeps_its_lines() {
        let c = card_text(
            &json!({"headline": "h", "detail": format!("one\n\n  two  \n{}", "x ".repeat(600))}),
        );
        assert!(c.detail.starts_with("one\ntwo\n"));
        assert!(c.detail.chars().count() <= MAX_DETAIL_CHARS);
        assert!(c.detail.ends_with('\u{2026}'));
    }

    #[test]
    fn a_missing_headline_borrows_the_first_line_of_the_detail() {
        let c = card_text(&json!({"headline": " ", "detail": "First point.\nSecond point."}));
        assert_eq!(c.headline, "First point.");
        assert_eq!(c.detail, "First point.\nSecond point.");
    }

    #[test]
    fn a_missing_detail_shows_the_headline_alone() {
        let c = card_text(&json!({"headline": "Only this", "detail": ""}));
        assert_eq!(c.headline, "Only this");
        assert_eq!(c.detail, "");
    }

    #[test]
    fn nothing_readable_says_nothing_to_explain() {
        for p in [
            json!({}),
            json!({"headline": "", "detail": ""}),
            json!({"headline": 3, "detail": null}),
            json!("text"),
        ] {
            let c = card_text(&p);
            assert_eq!(c.headline, NOTHING_HEADLINE, "{p}");
            assert_eq!(c.detail, "", "{p}");
        }
    }

    #[test]
    fn em_dashes_never_reach_the_card() {
        let c = card_text(&json!({"headline": "a \u{2014} b", "detail": "c \u{2014} d"}));
        assert!(!c.headline.contains('\u{2014}') && !c.detail.contains('\u{2014}'));
    }

    #[test]
    fn the_entry_is_a_read_only_answer_over_selection_then_screen() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, PROPOSAL_KIND);
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "a read-only answer needs no preview");
        assert!(!a.prompt.contains('\u{2014}'), "no em dash in model text");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
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
            .expect("the example file lists explain-this-screen");
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
