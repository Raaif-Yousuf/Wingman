//! The proposal schema registry (#23): maps a proposal *kind* name (the
//! `Action::proposal` string, e.g. `"verdict"`) to the JSON Schema the
//! model's completion must satisfy.
//!
//! `serde_json` keeps `preserve_order` (CLAUDE.md rule 3) crate-wide, so
//! every schema built here carries its property order into the wire
//! request unchanged -- a verdict-shaped proposal always puts the field the
//! model should commit to (`headline`, `difficulty`) after the field that
//! justifies it (`detail`), never before.
//!
//! `"verdict"`, `"calendar_event"` (#26) and `"text_review"` (#38) are
//! registered today. `text_answer` and `form_fill` (named in
//! CONTRIBUTING.md's "Add an action in 20 minutes" and the expansion plan's
//! §6) get their own `match` arm here the same day their first action
//! lands, not before -- an unimplemented arm would be untestable dead code
//! (see the `wired-to-nothing` skill).
//!
//! A property can carry `"editable": true` -- a non-standard JSON Schema
//! keyword a provider's completion never sees echoed back (it only reads
//! `type`/`enum`/etc. from `properties`), read solely by
//! `ui::preview::PreviewModel::from_schema` to decide which fields the
//! confirm card renders as an EDIT control versus plain text. Keeping the
//! flag on the same schema value the provider is sent, rather than a
//! parallel per-proposal-kind table, is what keeps "which fields are
//! editable" from drifting out of sync with "which fields exist": the two
//! questions share one answer, in one place, in schema property order.

use serde_json::{json, Value};

/// Looks up the JSON Schema for `proposal`. `rate_difficulty` only affects
/// `"verdict"` (whether its `difficulty` property and rubric-driven enum are
/// present); it is accepted unconditionally rather than only for the kinds
/// that use it, so a caller never has to know which proposal kinds care
/// about it.
///
/// Returns `None` for a proposal name nothing has registered a schema for
/// yet -- the caller (`provider::physics_request` today) is expected to
/// treat that as a load error, not silently send no schema.
pub fn schema_for(proposal: &str, rate_difficulty: bool) -> Option<Value> {
    match proposal {
        // Delegates to the existing, already-tested schema builder rather
        // than re-describing the same JSON here: two independent schema
        // literals for one wire shape is exactly the "hard-coded list"
        // drift the `wired-to-nothing` skill warns about, and it would
        // defeat the point of the golden test in `provider::mod` that
        // proves this registry is a drop-in replacement for the direct
        // call it replaces.
        "verdict" => Some(crate::provider::common::answer_schema(rate_difficulty)),
        "calendar_event" => Some(calendar_event_schema()),
        "text_review" => Some(text_review_schema()),
        _ => None,
    }
}

/// The `calendar_event` proposal schema (#26): title, start, end, location,
/// notes.
///
/// Property order is load-bearing (rule 3), same as `answer_schema`'s
/// `detail`-before-`headline`: `title` comes first so the model commits to
/// *which* event this is before it has to work out *when* -- extracting a
/// start/end time only makes sense once the model has already anchored on
/// one specific event on screen, not some other one nearby. `start` before
/// `end` mirrors how a time range is naturally read and lets `end` be
/// filled in relative to an already-committed `start` (an end time earlier
/// than its own start is a self-contradiction the model can only avoid by
/// having already picked a start). `location` and `notes` trail last: both
/// are supplementary context that never changes whether the event or its
/// times are correct, so nothing upstream needs to be committed before they
/// are filled in.
///
/// Only `title` and `start` are `"editable"` (#26's Done-when: "a
/// `calendar_event` proposal renders with editable start and title"); `end`,
/// `location` and `notes` render as plain text in the preview card. All
/// five are `required`: a calendar event with a blank title or start is not
/// a usable proposal to show "Do it" for.
fn calendar_event_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "title": {"type": "string", "editable": true},
            "start": {"type": "string", "editable": true},
            "end": {"type": "string"},
            "location": {"type": "string"},
            "notes": {"type": "string"}
        },
        "required": ["title", "start", "end", "location", "notes"],
        "additionalProperties": false
    })
}

/// The `text_review` proposal schema (#38): edits, verdict, tone_note,
/// missing_attachment.
///
/// Property order is load-bearing (rule 3), for the same reason
/// `answer_schema`'s `detail`-before-`headline` and `calendar_event_schema`'s
/// `title`-before-`start` are: `edits` comes first so the model has to work
/// out and commit to the concrete list of problems (if any) before it ever
/// writes the summary verdict that follows from that list -- a model asked
/// for `verdict` first could commit to "good_to_go" or "needs_edits" before
/// having actually enumerated what, if anything, is wrong, and then pad or
/// contradict `edits` to match a verdict it already picked. `tone_note` and
/// `missing_attachment` trail last: both are supplementary observations that
/// never change whether the text itself is good to go, so nothing upstream
/// needs them committed first.
///
/// No property is `"editable"`: unlike `calendar_event`'s `title`/`start`,
/// nothing here is meant to be hand-edited in the preview card -- `edits` is
/// the model's own list of proposed changes (accepted or not as a whole via
/// "Do it"/"Cancel", never edited field-by-field), and `verdict`/`tone_note`/
/// `missing_attachment` are read-only observations.
fn text_review_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "edits": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "before": {"type": "string"},
                        "after": {"type": "string"},
                        "reason": {"type": "string"}
                    },
                    "required": ["before", "after", "reason"],
                    "additionalProperties": false
                }
            },
            "verdict": {"type": "string", "enum": ["good_to_go", "needs_edits"]},
            "tone_note": {"type": "string"},
            "missing_attachment": {"type": "boolean"}
        },
        "required": ["edits", "verdict", "tone_note", "missing_attachment"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_without_difficulty_has_no_difficulty_property() {
        let schema = schema_for("verdict", false).expect("verdict is registered");
        assert_eq!(
            schema["required"],
            serde_json::json!(["detail", "headline"])
        );
        assert!(schema["properties"].get("difficulty").is_none());
    }

    #[test]
    fn verdict_with_difficulty_requires_detail_headline_then_difficulty_in_order() {
        let schema = schema_for("verdict", true).expect("verdict is registered");
        // Order is load-bearing (rule 3): detail justifies headline, which
        // justifies difficulty. `required` is a JSON array, so this
        // assertion checks ORDER, not just membership.
        assert_eq!(
            schema["required"],
            serde_json::json!(["detail", "headline", "difficulty"])
        );
    }

    #[test]
    fn verdict_matches_provider_common_answer_schema_exactly() {
        // The registry must be a pure delegation, not a second copy of the
        // schema -- this is what makes the golden test in provider::mod
        // (captured before physics_request was switched to call this
        // registry) trivially still pass.
        for want_difficulty in [false, true] {
            assert_eq!(
                schema_for("verdict", want_difficulty),
                Some(crate::provider::common::answer_schema(want_difficulty))
            );
        }
    }

    #[test]
    fn unknown_proposal_kind_is_none_not_a_panic() {
        assert_eq!(schema_for("text_answer", false), None);
        assert_eq!(schema_for("form_fill", false), None);
        assert_eq!(schema_for("totally_made_up", true), None);
    }

    // -- calendar_event (#26) -------------------------------------------

    #[test]
    fn calendar_event_is_registered() {
        assert!(schema_for("calendar_event", false).is_some());
    }

    #[test]
    fn calendar_event_declares_fields_in_load_bearing_order() {
        let schema = schema_for("calendar_event", false).expect("calendar_event is registered");
        let names: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(names, vec!["title", "start", "end", "location", "notes"]);
        // `required` is a JSON array, so this checks order too, not just
        // membership (same guard `verdict`'s test above uses).
        assert_eq!(
            schema["required"],
            serde_json::json!(["title", "start", "end", "location", "notes"])
        );
    }

    #[test]
    fn calendar_event_marks_only_title_and_start_editable() {
        let schema = schema_for("calendar_event", false).expect("calendar_event is registered");
        let props = schema["properties"].as_object().unwrap();
        for (name, def) in props {
            let editable = def
                .get("editable")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let expected = matches!(name.as_str(), "title" | "start");
            assert_eq!(
                editable, expected,
                "property {name:?} editable={editable} expected={expected}"
            );
        }
    }

    #[test]
    fn calendar_event_rejects_additional_properties() {
        let schema = schema_for("calendar_event", false).expect("calendar_event is registered");
        assert_eq!(schema["additionalProperties"], false);
    }

    #[test]
    fn calendar_event_ignores_rate_difficulty() {
        // rate_difficulty only affects "verdict" -- calendar_event's shape
        // must be identical regardless of the flag, the same contract
        // schema_for's own doc comment states.
        assert_eq!(
            schema_for("calendar_event", false),
            schema_for("calendar_event", true)
        );
    }

    // -- text_review (#38) --------------------------------------------------

    #[test]
    fn text_review_is_registered() {
        assert!(schema_for("text_review", false).is_some());
    }

    #[test]
    fn text_review_declares_top_level_fields_in_load_bearing_order() {
        // "Golden" order test (task brief): edits before verdict, then
        // tone_note, then missing_attachment.
        let schema = schema_for("text_review", false).expect("text_review is registered");
        let names: Vec<&str> = schema["properties"]
            .as_object()
            .expect("properties is an object")
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            names,
            vec!["edits", "verdict", "tone_note", "missing_attachment"]
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["edits", "verdict", "tone_note", "missing_attachment"])
        );
    }

    #[test]
    fn text_review_edits_item_declares_before_after_reason_in_order() {
        let schema = schema_for("text_review", false).expect("text_review is registered");
        let item_props = schema["properties"]["edits"]["items"]["properties"]
            .as_object()
            .expect("edits.items.properties is an object");
        let names: Vec<&str> = item_props.keys().map(String::as_str).collect();
        assert_eq!(names, vec!["before", "after", "reason"]);
        assert_eq!(
            schema["properties"]["edits"]["items"]["required"],
            serde_json::json!(["before", "after", "reason"])
        );
    }

    #[test]
    fn text_review_verdict_enum_is_exactly_the_two_documented_values() {
        let schema = schema_for("text_review", false).expect("text_review is registered");
        assert_eq!(
            schema["properties"]["verdict"]["enum"],
            serde_json::json!(["good_to_go", "needs_edits"])
        );
    }

    #[test]
    fn text_review_has_no_editable_fields() {
        let schema = schema_for("text_review", false).expect("text_review is registered");
        let props = schema["properties"].as_object().unwrap();
        for (name, def) in props {
            assert!(
                def.get("editable").is_none(),
                "property {name:?} must not be marked editable"
            );
        }
    }

    #[test]
    fn text_review_rejects_additional_properties() {
        let schema = schema_for("text_review", false).expect("text_review is registered");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["edits"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn text_review_ignores_rate_difficulty() {
        assert_eq!(
            schema_for("text_review", false),
            schema_for("text_review", true)
        );
    }
}
