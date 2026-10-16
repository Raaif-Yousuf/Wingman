//! What a generic (`actions.toml`) action's result card says (#441).
//!
//! Most executors report something worth showing in their `Undo::summary`
//! ("copied to the clipboard"). The `"none"` executor does not: it has
//! nothing to do, so its summary is a fixed string. For a read-only action
//! the model's proposal IS the result, so for `"none"` the card shows the
//! proposal's own fields instead. This module is that decision as pure
//! logic, so it is unit-tested without a window.

use serde_json::Value;

/// The collapsed card line is one line; longer text is cut here and the
/// full text goes in the expanded detail.
const MAX_HEADLINE_CHARS: usize = 88;

/// What to put on the result card: `headline` is the collapsed line,
/// `detail` the expanded text (empty when the headline already says it all).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardText {
    pub headline: String,
    pub detail: String,
}

/// Picks the result card text for a confirmed generic action.
///
/// For any executor other than `"none"` this is `(action_name, summary)`,
/// exactly what the card showed before. For `"none"` it is the proposal's
/// own content (see [`proposal_answer`]); a proposal with nothing to show
/// falls back to the executor's summary under the action name. A
/// `page_summary` proposal (#67) has its own layout and limits, see
/// [`super::page_summary::card_text`].
pub fn result_card_text(
    action_name: &str,
    executor_name: &str,
    proposal_kind: &str,
    summary: &str,
    proposal: &Value,
) -> CardText {
    if executor_name == "none" {
        if proposal_kind == super::page_summary::PROPOSAL_KIND {
            return super::page_summary::card_text(proposal);
        }
        if proposal_kind == super::explain_screen::PROPOSAL_KIND {
            return super::explain_screen::card_text(proposal);
        }
        if proposal_kind == super::scam_check::PROPOSAL_KIND {
            return super::scam_check::card_text(proposal);
        }
        if let Some(answer) = proposal_answer(proposal) {
            return answer;
        }
    }
    CardText {
        headline: action_name.to_string(),
        detail: summary.to_string(),
    }
}

/// Renders a proposal object as an answer: every field that has something
/// to show, in the object's own order (`serde_json` keeps `preserve_order`,
/// AGENTS.md rule 3, so that is the schema's order). `None` when the value
/// is not an object or has nothing to show.
///
/// The headline is the first field's text (first line, truncated). The
/// detail is empty when the proposal is a single short line; otherwise it is
/// the full text, with a `Label: value` line per field when there are
/// several.
pub fn proposal_answer(proposal: &Value) -> Option<CardText> {
    let fields: Vec<(&str, String)> = proposal
        .as_object()?
        .iter()
        .filter_map(|(k, v)| field_text(v).map(|t| (k.as_str(), t)))
        .collect();
    let (_, first) = fields.first()?;

    let first_line = first.lines().next().unwrap_or(first).trim();
    let headline = truncate(first_line, MAX_HEADLINE_CHARS);

    let detail = if let [(_, only)] = fields.as_slice() {
        if only.trim() == headline {
            String::new()
        } else {
            only.clone()
        }
    } else {
        fields
            .iter()
            .map(|(k, t)| format!("{}: {}", label(k), t))
            .collect::<Vec<_>>()
            .join("\n")
    };
    Some(CardText { headline, detail })
}

/// The displayable text of one field, or `None` for null and blank values.
fn field_text(v: &Value) -> Option<String> {
    let text = match v {
        Value::Null => return None,
        Value::String(s) => s.trim().to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::Array(items) => items
            .iter()
            .filter_map(field_text)
            .collect::<Vec<_>>()
            .join(", "),
        Value::Object(_) => v.to_string(),
    };
    (!text.is_empty()).then_some(text)
}

/// `"key_points"` -> `"Key points"`.
fn label(key: &str) -> String {
    let spaced = key.replace('_', " ");
    let mut chars = spaced.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

fn truncate(line: &str, max: usize) -> String {
    if line.chars().count() <= max {
        return line.to_string();
    }
    let mut out: String = line.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn none_executor_shows_the_models_text_not_nothing_to_do() {
        let card = result_card_text(
            "Define this word",
            "none",
            "text_answer",
            "nothing to do",
            &json!({"text": "ephemeral: lasting a very short time"}),
        );
        assert_eq!(card.headline, "ephemeral: lasting a very short time");
        assert!(!card.detail.contains("nothing to do"));
    }

    #[test]
    fn a_page_summary_uses_its_own_card_layout() {
        let card = result_card_text(
            "Summarize this page",
            "none",
            "page_summary",
            "nothing to do",
            &json!({"bullets": ["one"], "line1": "First", "line2": "Second"}),
        );
        assert_eq!(card.headline, "First\nSecond");
        assert_eq!(card.detail, "- one");
        // The same fields under another proposal kind keep the generic layout.
        let other = result_card_text(
            "Act",
            "none",
            "text_answer",
            "nothing to do",
            &json!({"bullets": ["one"], "line1": "First", "line2": "Second"}),
        );
        assert_ne!(other.headline, "First\nSecond");
    }

    #[test]
    fn a_screen_explanation_shows_the_headline_collapsed_and_the_detail_expanded() {
        let card = result_card_text(
            "Explain what's on screen",
            "none",
            "screen_explanation",
            "nothing to do",
            &json!({"detail": "A longer explanation.", "headline": "A short one."}),
        );
        assert_eq!(card.headline, "A short one.");
        assert_eq!(card.detail, "A longer explanation.");
        // Not the generic layout, which would lead with the first field.
        let other = result_card_text(
            "Act",
            "none",
            "text_answer",
            "nothing to do",
            &json!({"detail": "A longer explanation.", "headline": "A short one."}),
        );
        assert_ne!(other.headline, "A short one.");
    }

    #[test]
    fn other_executors_keep_the_action_name_and_summary() {
        let card = result_card_text(
            "Translate",
            "clipboard",
            "text_answer",
            "Copied to the clipboard",
            &json!({"text": "hola"}),
        );
        assert_eq!(card.headline, "Translate");
        assert_eq!(card.detail, "Copied to the clipboard");
    }

    #[test]
    fn none_executor_with_nothing_to_show_falls_back_to_the_summary() {
        for p in [
            json!({}),
            json!({"text": "  "}),
            json!({"a": null}),
            json!("x"),
            json!(null),
        ] {
            let card = result_card_text("Act", "none", "text_answer", "nothing to do", &p);
            assert_eq!(card.headline, "Act", "{p}");
            assert_eq!(card.detail, "nothing to do", "{p}");
        }
    }

    #[test]
    fn single_short_field_needs_no_detail() {
        let a = proposal_answer(&json!({"text": "  42 m/s  "})).unwrap();
        assert_eq!(a.headline, "42 m/s");
        assert_eq!(a.detail, "");
    }

    #[test]
    fn single_multiline_field_keeps_every_line_in_the_detail() {
        let a = proposal_answer(&json!({"text": "first\nsecond\nthird"})).unwrap();
        assert_eq!(a.headline, "first");
        assert_eq!(a.detail, "first\nsecond\nthird");
    }

    #[test]
    fn long_headline_is_truncated_and_the_full_text_goes_in_the_detail() {
        let long = "word ".repeat(40);
        let a = proposal_answer(&json!({"text": long.trim()})).unwrap();
        assert_eq!(a.headline.chars().count(), MAX_HEADLINE_CHARS);
        assert!(a.headline.ends_with('\u{2026}'));
        assert_eq!(a.detail, long.trim());
    }

    #[test]
    fn several_fields_are_labelled_in_the_proposals_own_order() {
        let a = proposal_answer(&json!({
            "summary": "A short note",
            "key_points": ["one", "two"],
            "confidence": 0.5,
            "ok": true,
            "skipped": null,
            "blank": ""
        }))
        .unwrap();
        assert_eq!(a.headline, "A short note");
        assert_eq!(
            a.detail,
            "Summary: A short note\nKey points: one, two\nConfidence: 0.5\nOk: true"
        );
    }

    #[test]
    fn nested_object_is_shown_compactly_not_dropped() {
        let a = proposal_answer(&json!({"meta": {"a": 1}})).unwrap();
        assert_eq!(a.headline, "{\"a\":1}");
    }

    #[test]
    fn multibyte_truncation_does_not_panic() {
        let a = proposal_answer(&json!({"text": "é".repeat(200)})).unwrap();
        assert_eq!(a.headline.chars().count(), MAX_HEADLINE_CHARS);
    }
}
