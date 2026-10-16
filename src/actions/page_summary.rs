//! "Summarize this page" (#67): a two-line summary of the page in the window
//! (the selected text if there is one, else the page text read through UI
//! Automation (#104), else the screen), with about five bullets behind a click.
//!
//! The model fills `bullets`, then `line1` and `line2` (the bullets first, so
//! the two lines are written from points it has already committed to; see
//! `actions::schema`). The executor is `"none"`: the card is the result.
//! [`card_text`] turns the proposal into card text. The first line says what
//! the page is, the second what matters; both are on the collapsed card, and
//! the expanded card adds the bullets. The length limits are enforced here in
//! code (as #301 did for the verdict card), not only asked for in the prompt.
//!
//! Not in this action: pinning the summary to the window being read (#325)
//! and having it ready before the key is pressed (#330).

use serde_json::Value;

use super::generic_answer::CardText;
use super::{Action, InputKind, Prefer};
use crate::provider::sanitize_model_text;
use crate::ui::card::HEADLINE_CHARS_PER_LINE;

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "summarize-page";

/// The proposal kind (`actions::schema::schema_for` key).
pub const PROPOSAL_KIND: &str = "page_summary";

/// Longest first line, in characters: one wrapped line of the collapsed card
/// (`ui::card::HEADLINE_CHARS_PER_LINE`).
pub const MAX_LINE1_CHARS: usize = HEADLINE_CHARS_PER_LINE;
/// Longest second line: two wrapped lines, with a little slack for word
/// wrap. Together with line 1 that is the card's 3-line headline budget
/// (`ui::card::HEADLINE_MAX_LINES`), so neither line is ellipsized.
pub const MAX_LINE2_CHARS: usize = 2 * HEADLINE_CHARS_PER_LINE - 10;
/// Most bullets on the expanded card.
pub const MAX_BULLETS: usize = 5;
/// Longest bullet, in characters.
pub const MAX_BULLET_CHARS: usize = 160;

/// A key the app adds to a proposal when the page was read from a different
/// site than the one the user confirmed (a redirect). The model never
/// fills it and it is not in the schema.
pub const READ_FROM_KEY: &str = "read_from";

/// What the card says when there is nothing to summarize.
pub const NOTHING_HEADLINE: &str = "Nothing to summarize";

/// The action's prompt. The limits are asked for here and enforced in
/// [`card_text`].
pub const BASE_PROMPT: &str = "You are shown a long page (an article, a report, an email thread, a document), as text (the selection if there is one, otherwise the page text read from the window) and as a screenshot only when no text could be read. Summarize what is on it. Fill `bullets` first: at most 5 plain-text points, one short sentence each, in the order they appear on the page. Then write `line1`: one short sentence, at most 50 characters, that says what this page is or announces. Then write `line2`: one sentence, at most 90 characters, that says what matters most to a reader. Use only what the page says: do not add facts, numbers or opinions that are not shown. If there is no readable page content, return an empty `line1`, an empty `line2` and no bullets. Use plain text only: no markdown, no bullet characters, no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `page_summary`, executor `none`,
/// `confirm = false` (read-only, nothing to confirm).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Summarize this page".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::PageText, InputKind::Screen],
        proposal: PROPOSAL_KIND.to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

/// One model string as one clean line: whitespace runs (newlines included)
/// become single spaces, em dashes are replaced, and the result is cut to
/// `max` characters with a trailing ellipsis.
fn clean_line(v: Option<&Value>, max: usize) -> String {
    let joined = v
        .and_then(Value::as_str)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    sanitize_model_text(&joined, max)
}

/// The card text for a `page_summary` proposal. The headline is the two lines
/// (one per line); the detail is the bullets, one `- ` line each. A proposal
/// with a single line shows that line alone; one with neither shows
/// [`NOTHING_HEADLINE`], even if it has bullets, because bullets without a
/// summary line are not a summary.
pub fn card_text(proposal: &Value) -> CardText {
    let line1 = clean_line(proposal.get("line1"), MAX_LINE1_CHARS);
    let line2 = clean_line(proposal.get("line2"), MAX_LINE2_CHARS);
    let headline = [line1, line2]
        .into_iter()
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    if headline.is_empty() {
        return CardText {
            headline: NOTHING_HEADLINE.to_string(),
            detail: String::new(),
        };
    }
    let bullets = proposal
        .get("bullets")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|b| clean_line(Some(b), MAX_BULLET_CHARS))
                .filter(|b| !b.is_empty())
                .take(MAX_BULLETS)
                .map(|b| format!("- {b}"))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    // A redirect: the user confirmed one site and another was read.
    let read_from = proposal
        .get(READ_FROM_KEY)
        .and_then(Value::as_str)
        .map(|h| clean_line(Some(&Value::from(h)), 253))
        .filter(|h| !h.is_empty())
        .map(|h| format!("Read from {h} (the link redirected there)."));
    let detail = match read_from {
        Some(note) if bullets.is_empty() => note,
        Some(note) => format!("{note}\n{bullets}"),
        None => bullets,
    };
    CardText { headline, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_page_read_from_another_site_names_that_site_on_the_card() {
        let c = card_text(&json!({
            "bullets": ["Point."],
            "line1": "A page.",
            "line2": "It matters.",
            READ_FROM_KEY: "b.example"
        }));
        assert!(c.detail.starts_with("Read from b.example"), "{}", c.detail);
        assert!(c.detail.contains("- Point."), "{}", c.detail);
        assert!(!c.headline.contains("b.example"));
        // Still shown when the model gave no bullets.
        let c = card_text(&json!({"line1": "A page.", READ_FROM_KEY: "b.example"}));
        assert!(c.detail.contains("b.example"), "{}", c.detail);
        // A summary of the page the user asked for says nothing extra.
        let c = card_text(&json!({"line1": "A page.", "bullets": ["P."]}));
        assert_eq!(c.detail, "- P.");
        assert!(!c.detail.contains('\u{2014}'));
    }

    #[test]
    fn two_lines_and_the_bullets_land_on_the_card() {
        let c = card_text(&json!({
            "bullets": ["Rates held at 5.25%", "Two cuts signalled"],
            "line1": "Fed holds rates at 5.25%, signals two cuts.",
            "line2": "Mortgage and savings rates likely to ease from November."
        }));
        assert_eq!(
            c.headline,
            "Fed holds rates at 5.25%, signals two cuts.\nMortgage and savings rates likely to ease from November."
        );
        assert_eq!(c.detail, "- Rates held at 5.25%\n- Two cuts signalled");
    }

    #[test]
    fn a_long_line_is_cut_to_the_limit_with_an_ellipsis() {
        let long = "word ".repeat(60);
        let c = card_text(&json!({"line1": long, "line2": "ok", "bullets": []}));
        let first = c.headline.lines().next().unwrap();
        assert!(first.chars().count() <= MAX_LINE1_CHARS, "{first}");
        assert!(first.ends_with('\u{2026}'));
        assert_eq!(c.headline.lines().nth(1), Some("ok"));
    }

    #[test]
    fn the_longest_pair_fits_the_collapsed_cards_headline_budget() {
        use crate::ui::card::HEADLINE_MAX_LINES;
        let wrapped = |n: usize| n.div_ceil(HEADLINE_CHARS_PER_LINE);
        assert!(wrapped(MAX_LINE1_CHARS) + wrapped(MAX_LINE2_CHARS) <= HEADLINE_MAX_LINES as usize);
        let c = card_text(&json!({
            "line1": "a".repeat(300), "line2": "b ".repeat(300), "bullets": []
        }));
        let (l1, l2) = c.headline.split_once('\n').unwrap();
        assert!(
            wrapped(l1.chars().count()) + wrapped(l2.chars().count())
                <= HEADLINE_MAX_LINES as usize
        );
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
            .expect("the example file lists summarize-page");
        assert!(!a.prompt.is_empty());
        assert_eq!(a.prompt.replace('\r', ""), BASE_PROMPT);
    }

    #[test]
    fn a_line_with_newlines_is_one_line() {
        let c = card_text(&json!({"line1": "a\nb\r\n c", "line2": "d", "bullets": []}));
        assert_eq!(c.headline, "a b c\nd");
    }

    #[test]
    fn bullets_are_capped_in_number_and_length() {
        let bullets: Vec<String> = (0..9)
            .map(|i| format!("point {i} {}", "x".repeat(300)))
            .collect();
        let c = card_text(&json!({"line1": "a", "line2": "b", "bullets": bullets}));
        let lines: Vec<&str> = c.detail.lines().collect();
        assert_eq!(lines.len(), MAX_BULLETS);
        for l in lines {
            assert!(l.starts_with("- point "));
            assert!(l.chars().count() <= MAX_BULLET_CHARS + 2, "{l}");
        }
    }

    #[test]
    fn blank_bullets_are_dropped_and_do_not_use_up_the_cap() {
        let c = card_text(&json!({
            "line1": "a", "line2": "b",
            "bullets": ["", "  ", "1", "2", "3", "4", "5", "6"]
        }));
        assert_eq!(c.detail, "- 1\n- 2\n- 3\n- 4\n- 5");
    }

    #[test]
    fn em_dashes_never_reach_the_card() {
        let c = card_text(&json!({
            "line1": "a \u{2014} b", "line2": "c", "bullets": ["d \u{2014} e"]
        }));
        assert!(!c.headline.contains('\u{2014}') && !c.detail.contains('\u{2014}'));
    }

    #[test]
    fn one_line_alone_shows_just_that_line() {
        let c = card_text(&json!({"line1": "Only this", "line2": " ", "bullets": []}));
        assert_eq!(c.headline, "Only this");
        assert_eq!(c.detail, "");
    }

    #[test]
    fn no_lines_means_nothing_to_summarize_even_with_bullets() {
        for p in [
            json!({}),
            json!({"line1": "", "line2": "", "bullets": ["x"]}),
            json!({"line1": 3, "line2": null}),
            json!("text"),
        ] {
            let c = card_text(&p);
            assert_eq!(c.headline, NOTHING_HEADLINE, "{p}");
            assert_eq!(c.detail, "", "{p}");
        }
    }

    #[test]
    fn the_action_entry_is_wired_the_way_the_issue_describes() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(
            a.inputs,
            vec![InputKind::Selection, InputKind::PageText, InputKind::Screen]
        );
        assert_eq!(a.proposal, PROPOSAL_KIND);
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "read-only, the card is the result");
        assert!(!a.prompt.contains('\u{2014}'));
        assert!(a
            .prompt
            .contains(&format!("at most {MAX_LINE1_CHARS} characters")));
        assert!(a
            .prompt
            .contains(&format!("at most {MAX_LINE2_CHARS} characters")));
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
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
