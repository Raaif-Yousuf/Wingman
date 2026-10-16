//! "Action items to a checklist" (#314): read the action items off meeting
//! notes, an email thread or a chat message and put them on the clipboard as
//! a Markdown checklist.
//!
//! The model fills `items`, one short line per action item ("Dana: send the
//! Q3 numbers (Fri)"). The confirm card shows those lines and lets the user
//! edit them; "Do it" runs the `"clipboard"` executor, which turns the
//! confirmed lines into `- [ ] ...` lines with [`checklist`]. The preview
//! card hands the executor `items` as an array of strings (what was shown,
//! after any edits). [`checklist_from_value`] also accepts a newline-joined
//! string, for a hand-written `actions.toml` action that produces one. The
//! marker is added in Rust rather than trusted to the model.

use serde_json::Value;

use super::{Action, InputKind, Prefer};

/// This action's id. Not special-cased by the palette: it runs through the
/// generic dispatch path (`App::run_generic_action`), the same as an
/// `actions.toml` entry, because it needs nothing a generic action lacks.
pub const ACTION_ID: &str = "action-items-checklist";

/// The action's prompt. The "never invent a date" rule is the load-bearing
/// part: a due date is only ever what the screen says.
pub const BASE_PROMPT: &str = "You are shown notes, an email thread, a chat message or a project page (as text, or as a screenshot if no text was selected). Find the action items: things a named person, or the reader, has to do. Return them in `items`, one short plain-text line per action item, in the order they appear, written as \"Owner: task (due)\". Use the owner's name exactly as shown; write \"Me\" when the text assigns it to the reader or to nobody in particular. Add the \"(due)\" part only when the screen states a date or deadline for that item, and copy it as written (for example \"Fri\" or \"Oct 3\"); never work out, guess or invent a date, and leave it off when none is shown. Do not include items that are already marked done, decisions, or general discussion. If there are no action items, return an empty list. Do not add bullets, numbers or checkbox characters: they are added for you. Use plain text only: no markdown, no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise (#484), proposal `action_items`, executor `clipboard`.
/// `confirm = true`: the card lists the items so the user can fix a line
/// before it lands on the clipboard.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Action items to checklist".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
        proposal: "action_items".to_string(),
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

/// Turns lines of text into a Markdown checklist, one `- [ ] ` line per
/// non-empty line. A bullet, checkbox or number the line already starts with
/// is dropped first, so a model (or a user edit) that added its own marker
/// does not end up with a doubled one.
pub fn checklist(lines: &str) -> String {
    lines
        .lines()
        .map(|line| strip_marker(line.trim()))
        .filter(|text| !text.is_empty())
        .map(|text| format!("- [ ] {text}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The checklist for a confirmed proposal's `items` field, whether it is a
/// array of strings (what the preview card produces) or a newline-joined
/// string (kept for a hand-written `actions.toml` action). `None` when there
/// is no `items` field or it holds no items.
pub fn checklist_from_value(proposal: &Value) -> Option<String> {
    let joined = match proposal.get("items")? {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("\n"),
        _ => return None,
    };
    let list = checklist(&joined);
    (!list.is_empty()).then_some(list)
}

/// What the card says when the screen holds no action items.
pub const NO_ITEMS_HEADLINE: &str = "No action items found";

/// Whether a model proposal for the `action_items` kind has nothing to list.
/// Always false for any other proposal kind.
pub fn is_empty_result(proposal_kind: &str, proposal: &Value) -> bool {
    proposal_kind == "action_items" && checklist_from_value(proposal).is_none()
}

/// Drops a leading list marker: `-`, `*`, a bullet character, `[ ]`/`[x]`
/// (alone or after a dash), or `1.`/`1)`.
fn strip_marker(line: &str) -> &str {
    let mut rest = line;
    // A bullet first, then an optional checkbox ("- [ ] task").
    if let Some(r) = rest
        .strip_prefix(['-', '*', '\u{2022}'])
        .filter(|r| r.starts_with(char::is_whitespace))
    {
        rest = r.trim_start();
    }
    for checkbox in ["[ ]", "[x]", "[X]"] {
        if let Some(r) = rest.strip_prefix(checkbox) {
            rest = r.trim_start();
            break;
        }
    }
    let digits = rest.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 {
        let after = &rest[digits..];
        if let Some(r) = after.strip_prefix(['.', ')']) {
            if r.starts_with(char::is_whitespace) {
                rest = r.trim_start();
            }
        }
    }
    rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn builds_the_issues_example_checklist() {
        let lines = "Dana: send the Q3 numbers (Fri)\nMe: book the design review room\nSam: update the onboarding doc";
        assert_eq!(
            checklist(lines),
            "- [ ] Dana: send the Q3 numbers (Fri)\n- [ ] Me: book the design review room\n- [ ] Sam: update the onboarding doc"
        );
    }

    #[test]
    fn blank_lines_and_surrounding_space_are_dropped() {
        assert_eq!(checklist("\n  a  \n\n\t b\r\n"), "- [ ] a\n- [ ] b");
        assert_eq!(checklist(""), "");
        assert_eq!(checklist("  \n "), "");
    }

    #[test]
    fn a_marker_the_line_already_has_is_not_doubled() {
        let lines = "- Ann: one\n* Bob: two\n\u{2022} Cy: three\n- [ ] Di: four\n[x] Ed: five\n1. Flo: six\n2) Gus: seven";
        assert_eq!(
            checklist(lines),
            "- [ ] Ann: one\n- [ ] Bob: two\n- [ ] Cy: three\n- [ ] Di: four\n- [ ] Ed: five\n- [ ] Flo: six\n- [ ] Gus: seven"
        );
    }

    #[test]
    fn text_that_only_looks_like_a_marker_is_kept() {
        // No space after the marker character, or no marker at all.
        assert_eq!(
            checklist("-5 degrees check\n2024 plan review\n3.5 hours: Kim"),
            "- [ ] -5 degrees check\n- [ ] 2024 plan review\n- [ ] 3.5 hours: Kim"
        );
    }

    #[test]
    fn value_accepts_a_joined_string_or_an_array() {
        let from_string = json!({"items": "Dana: a (Fri)\nMe: b"});
        let from_array = json!({"items": ["Dana: a (Fri)", "Me: b"]});
        let want = "- [ ] Dana: a (Fri)\n- [ ] Me: b";
        assert_eq!(checklist_from_value(&from_string).as_deref(), Some(want));
        assert_eq!(checklist_from_value(&from_array).as_deref(), Some(want));
    }

    #[test]
    fn value_with_nothing_to_list_is_none() {
        for v in [
            json!({}),
            json!({"items": []}),
            json!({"items": ""}),
            json!({"items": ["", "  "]}),
            json!({"items": 3}),
            json!({"text": "x"}),
        ] {
            assert_eq!(checklist_from_value(&v), None, "{v}");
        }
    }

    #[test]
    fn an_empty_action_items_proposal_ends_in_the_no_items_card() {
        for v in [json!({"items": []}), json!({"items": [" "]}), json!({})] {
            assert!(is_empty_result("action_items", &v), "{v}");
        }
        assert!(!is_empty_result("action_items", &json!({"items": ["a"]})));
        // Other proposal kinds are never treated as empty by this check.
        assert!(!is_empty_result("text_answer", &json!({"items": []})));
        assert_eq!(NO_ITEMS_HEADLINE, "No action items found");
    }

    #[test]
    fn the_action_entry_is_wired_the_way_the_issue_describes() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, "action_items");
        assert_eq!(a.executor, "clipboard");
        assert!(
            a.confirm,
            "the card lists the items before anything is copied"
        );
        assert!(a.prompt.contains("never work out, guess or invent a date"));
        assert!(
            !a.prompt.contains('\u{2014}'),
            "no em dash in model-facing text"
        );
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the clipboard executor resolves");
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
