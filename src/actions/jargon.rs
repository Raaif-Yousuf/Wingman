//! "Jargon to plain English" (#335): the technical terms and acronyms on
//! screen, each with a short plain meaning, on the card.
//!
//! A generic action over the selection first and the screen otherwise. The
//! proposal is `jargon_terms`: a list of `{term, meaning}`. The executor is
//! `"none"`, so the card is the result. This is the card version only; putting
//! each label next to the term on screen is the overlay (#325).
//!
//! The limits are asked for in the prompt and enforced in [`card_text`]: at
//! most [`MAX_TERMS`] terms, one entry per term (the first), a meaning cut to
//! [`MAX_MEANING_CHARS`]. The meanings are the model's own words, not
//! looked-up facts, so the prompt keeps them to what the text itself shows and
//! the card is plain model output.

use std::collections::HashSet;

use serde_json::Value;

use super::generic_answer::CardText;
use super::{Action, InputKind, Prefer};
use crate::provider::sanitize_model_text;
use crate::ui::card::{HEADLINE_CHARS_PER_LINE, HEADLINE_MAX_LINES};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "explain-jargon";

/// The proposal kind (`actions::schema::schema_for` key).
pub const PROPOSAL_KIND: &str = "jargon_terms";

/// Most terms kept from one screen. More are counted, not shown.
pub const MAX_TERMS: usize = 12;
/// Longest plain meaning, in characters.
pub const MAX_MEANING_CHARS: usize = 80;
/// Longest term, in characters (a long phrase is not a term).
pub const MAX_TERM_CHARS: usize = 40;
/// Longest headline, in characters: the collapsed card's whole headline
/// budget, less a little slack for word wrap.
pub const MAX_HEADLINE_CHARS: usize = HEADLINE_MAX_LINES as usize * HEADLINE_CHARS_PER_LINE - 15;
/// Longest card detail, in characters. Terms past it are counted, not shown.
pub const MAX_DETAIL_CHARS: usize = 1100;

/// What the card says when there is no jargon to explain.
pub const NOTHING_HEADLINE: &str = "No jargon found";

/// The action's prompt. The limits are asked for here and enforced in
/// [`card_text`].
pub const BASE_PROMPT: &str = "You are shown some text (as text if some was selected, as a screenshot otherwise). Find the jargon, technical terms and acronyms that a general reader would not know, and explain each in plain English. Fill `terms` with at most 12 entries, in the order they first appear. Each entry has `term`, copied exactly as shown, and `meaning`, one plain sentence of at most 80 characters that a non-expert understands, with no jargon of its own. List each term once, at its first appearance. Skip ordinary words, names of people and places, and anything a general reader already knows. If a term could mean different things in different fields, give the meaning that fits the text and never invent a definition: if you are not sure what it means here, leave it out. If there is no jargon, return an empty list. Use plain text only: no markdown and no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Study, the selection when there is one and the
/// screen otherwise, proposal `jargon_terms`, executor `none`, `confirm =
/// false` (read-only: the answer is the card).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Jargon to plain English".to_string(),
        group: Some("Study".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
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

/// One model string as a single clean line, cut to `max` characters.
fn one_line(v: Option<&Value>, max: usize) -> String {
    let joined = v
        .and_then(Value::as_str)
        .unwrap_or("")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    sanitize_model_text(&joined, max)
}

/// The `Term: meaning` lines of a proposal, in order, with blank entries
/// dropped and a repeated term (any letter case) kept only the first time.
/// Also the count of entries past [`MAX_TERMS`], which are not listed.
fn entries(proposal: &Value) -> (Vec<String>, usize) {
    let items = proposal
        .get("terms")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let mut seen = HashSet::new();
    let mut lines = Vec::new();
    let mut extra = 0;
    for item in items {
        let term = one_line(item.get("term"), MAX_TERM_CHARS);
        let meaning = one_line(item.get("meaning"), MAX_MEANING_CHARS);
        if term.is_empty() || meaning.is_empty() || !seen.insert(term.to_lowercase()) {
            continue;
        }
        if lines.len() == MAX_TERMS {
            extra += 1;
        } else {
            lines.push(format!("{term}: {meaning}"));
        }
    }
    (lines, extra)
}

/// The card text for a `jargon_terms` proposal. The headline is the first
/// term and its meaning, with how many more follow; the detail lists every
/// term, one per line, until it reaches [`MAX_DETAIL_CHARS`] and then says
/// "and N more". With no usable term the card says [`NOTHING_HEADLINE`].
pub fn card_text(proposal: &Value) -> CardText {
    let (lines, extra) = entries(proposal);
    let Some(first) = lines.first() else {
        return CardText {
            headline: NOTHING_HEADLINE.to_string(),
            detail: String::new(),
        };
    };
    let more = lines.len() - 1 + extra;
    let suffix = match more {
        0 => String::new(),
        n => format!(" (+{n} more)"),
    };
    let headline = format!(
        "{}{suffix}",
        sanitize_model_text(first, MAX_HEADLINE_CHARS - suffix.chars().count())
    );
    if more == 0 {
        return CardText {
            headline,
            detail: String::new(),
        };
    }
    let mut shown = Vec::new();
    let mut used = 0;
    for line in &lines {
        let cost = line.chars().count() + 1;
        if !shown.is_empty() && used + cost > MAX_DETAIL_CHARS {
            break;
        }
        used += cost;
        shown.push(line.as_str());
    }
    let hidden = lines.len() - shown.len() + extra;
    let mut detail = shown.join("\n");
    if hidden > 0 {
        detail.push_str(&format!("\nand {hidden} more"));
    }
    CardText { headline, detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn terms(pairs: &[(&str, &str)]) -> Value {
        json!({"terms": pairs.iter().map(|(t, m)| json!({"term": t, "meaning": m})).collect::<Vec<_>>()})
    }

    #[test]
    fn the_entry_is_a_read_only_answer_over_selection_then_screen() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.group.as_deref(), Some("Study"));
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, "jargon_terms");
        assert_eq!(a.executor, "none");
        assert!(!a.confirm, "a read-only answer needs no preview");
        assert!(!a.prompt.contains('\u{2014}'), "no em dash in model text");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
    }

    #[test]
    fn it_is_registered_as_a_built_in() {
        let ids: Vec<String> = crate::actions::builtin_actions()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert!(ids.iter().any(|id| id == ACTION_ID), "{ids:?}");
    }

    #[test]
    fn the_prompt_states_the_limits_and_never_invent() {
        for needle in ["at most 12", "80 characters", "once", "never invent"] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
    }

    #[test]
    fn one_term_is_just_a_headline() {
        let c = card_text(&terms(&[("EBITDA", "profit before interest and tax")]));
        assert_eq!(c.headline, "EBITDA: profit before interest and tax");
        assert_eq!(c.detail, "");
    }

    #[test]
    fn several_terms_headline_the_first_and_list_all() {
        let c = card_text(&terms(&[
            ("SLA", "the promised uptime"),
            ("p < 0.05", "unlikely to be chance"),
            ("EBITDA", "profit before interest and tax"),
        ]));
        assert_eq!(c.headline, "SLA: the promised uptime (+2 more)");
        assert_eq!(
            c.detail,
            "SLA: the promised uptime\np < 0.05: unlikely to be chance\nEBITDA: profit before interest and tax"
        );
    }

    #[test]
    fn a_meaning_is_cut_to_eighty_characters_in_code() {
        let long = "word ".repeat(60);
        let c = card_text(&terms(&[("A", &long), ("B", "ok")]));
        let first = c.detail.lines().next().unwrap();
        let meaning = first.strip_prefix("A: ").unwrap();
        assert!(meaning.chars().count() <= MAX_MEANING_CHARS, "{meaning}");
        assert!(meaning.ends_with('\u{2026}'));
    }

    #[test]
    fn only_the_first_entry_of_a_repeated_term_is_kept() {
        let c = card_text(&terms(&[
            ("SLA", "first meaning"),
            ("sla", "second meaning"),
            ("ROI", "return on money spent"),
        ]));
        assert_eq!(c.detail, "SLA: first meaning\nROI: return on money spent");
    }

    #[test]
    fn at_most_twelve_terms_are_listed_and_the_rest_are_counted() {
        let many: Vec<(String, String)> = (0..15)
            .map(|i| (format!("T{i}"), "short".to_string()))
            .collect();
        let pairs: Vec<(&str, &str)> = many.iter().map(|(t, m)| (t.as_str(), m.as_str())).collect();
        let c = card_text(&terms(&pairs));
        assert_eq!(c.headline, "T0: short (+14 more)");
        assert_eq!(c.detail.lines().filter(|l| l.starts_with('T')).count(), 12);
        assert!(c.detail.ends_with("\nand 3 more"), "{}", c.detail);
    }

    #[test]
    fn the_detail_stops_at_its_budget_and_says_how_many_are_left() {
        let meaning = "m".repeat(MAX_MEANING_CHARS);
        let many: Vec<(String, String)> = (0..12)
            .map(|i| (format!("Term number {i} that is long"), meaning.clone()))
            .collect();
        let pairs: Vec<(&str, &str)> = many.iter().map(|(t, m)| (t.as_str(), m.as_str())).collect();
        let c = card_text(&terms(&pairs));
        assert!(
            c.detail.chars().count() <= MAX_DETAIL_CHARS + 20,
            "{}",
            c.detail
        );
        assert!(c.detail.contains("\nand "), "{}", c.detail);
        assert!(c.headline.chars().count() <= MAX_HEADLINE_CHARS);
    }

    #[test]
    fn blank_and_malformed_entries_are_skipped_not_panicked_on() {
        let p = json!({"terms": [
            {"term": "", "meaning": "no term"},
            {"term": "No meaning", "meaning": "  "},
            {"term": 5, "meaning": "not text"},
            "just a string",
            null,
            {"term": "ROI", "meaning": "return on money spent"}
        ]});
        let c = card_text(&p);
        assert_eq!(c.headline, "ROI: return on money spent");
    }

    #[test]
    fn nothing_found_says_so() {
        for p in [
            json!({"terms": []}),
            json!({}),
            json!({"terms": "x"}),
            json!(null),
        ] {
            let c = card_text(&p);
            assert_eq!(c.headline, NOTHING_HEADLINE);
            assert_eq!(c.detail, "");
        }
    }

    #[test]
    fn newlines_and_em_dashes_in_model_text_are_flattened() {
        let c = card_text(&terms(&[(
            "Net\nzero",
            "a goal\u{2014}no more\nthan it emits",
        )]));
        assert!(!c.headline.contains('\n'));
        assert!(!c.headline.contains('\u{2014}'));
    }

    #[test]
    fn the_result_card_uses_this_layout_for_the_none_executor() {
        let c = crate::actions::generic_answer::result_card_text(
            "Jargon to plain English",
            "none",
            PROPOSAL_KIND,
            "nothing to do",
            &terms(&[("SLA", "the promised uptime")]),
        );
        assert_eq!(c.headline, "SLA: the promised uptime");
    }
}
