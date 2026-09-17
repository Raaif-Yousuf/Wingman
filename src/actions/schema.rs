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
//! `"verdict"` and `"calendar_event"` (#26) are registered today.
//! `text_answer`, `form_fill` and `text_review` (named in CONTRIBUTING.md's
//! "Add an action in 20 minutes" and the expansion plan's §6) get their own
//! `match` arm here the same day their first action lands, not before -- an
//! unimplemented arm would be untestable dead code (see the
//! `wired-to-nothing` skill).
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
        "form_fill" => Some(form_fill_schema()),
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

/// The `form_fill` proposal schema (#40 "Fill this form"). **This is the
/// model-facing request schema, not the final `fill_form` executor proposal
/// shape.** `actions::fill_form`'s two-stage design (see that module's doc
/// comment) maps most fields locally via `profile::match_label`, with no
/// model call at all; only the fields it cannot map (or maps ambiguously)
/// are ever sent to the model, and this schema is what THAT completion must
/// satisfy -- one entry per unmapped field, named by the `control_id` the
/// prompt gave it (an index into that request's own field list, not a
/// stable identifier across requests). `actions::fill_form` then merges the
/// model's response with the locally-mapped fields and builds the actual
/// `executors::fill_form`-consumable proposal (`{fields:
/// [{target, label, value, sensitive, approved}]}`, keyed by UIA `target`
/// identity, never `control_id`) -- that merge, not this schema, is what
/// `executors::fill_form::parse_field_fill` reads.
///
/// Deliberately lean for the two-stage design's whole point (saving
/// tokens): no `label` property, since the model never needs to echo back a
/// label `actions::fill_form` already knows from the UIA snapshot it sent.
///
/// Property order is load-bearing (rule 3): `control_id` first (which field
/// this entry is about), then `source` (a token that must be `"profile"`,
/// `"model"` or `"skip"` -- committing to a strategy before naming specifics
/// forces the model to decide *how* it knows a value before writing one
/// down), then `profile_field` (which profile field name, when
/// `source == "profile"`; empty otherwise -- `actions::fill_form` looks up
/// the real value itself, never trusting a value the model might have
/// echoed for a profile-sourced field), then `value` (the model's own
/// literal text, read only when `source == "model"`), then `sensitive`
/// last: the model's own opinion of whether ITS OWN invented value is
/// sensitive, which only makes sense to ask once that value has already
/// been produced.
fn form_fill_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "fields": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "control_id": {"type": "string"},
                        "source": {"type": "string"},
                        "profile_field": {"type": "string"},
                        "value": {"type": "string"},
                        "sensitive": {"type": "boolean"}
                    },
                    "required": ["control_id", "source", "profile_field", "value", "sensitive"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["fields"],
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
        assert_eq!(schema_for("text_review", false), None);
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

    // -- form_fill (#40) -----------------------------------------------

    #[test]
    fn form_fill_is_registered() {
        assert!(schema_for("form_fill", false).is_some());
    }

    #[test]
    fn form_fill_wraps_a_fields_array_matching_the_executor_proposal_shape() {
        // `executors::fill_form::parse_form_fill` reads a top-level
        // "fields" array too -- same wrapper key, even though the item
        // shape differs (this is the model-facing request schema, not the
        // final merged proposal; see this function's own doc comment).
        let schema = schema_for("form_fill", false).expect("form_fill is registered");
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["required"], serde_json::json!(["fields"]));
        assert_eq!(schema["properties"]["fields"]["type"], "array");
    }

    #[test]
    fn form_fill_item_declares_fields_in_load_bearing_order() {
        let schema = schema_for("form_fill", false).expect("form_fill is registered");
        let item_props = schema["properties"]["fields"]["items"]["properties"]
            .as_object()
            .expect("items.properties is an object");
        let names: Vec<&str> = item_props.keys().map(String::as_str).collect();
        assert_eq!(
            names,
            vec![
                "control_id",
                "source",
                "profile_field",
                "value",
                "sensitive"
            ]
        );
        let required = &schema["properties"]["fields"]["items"]["required"];
        assert_eq!(
            *required,
            serde_json::json!([
                "control_id",
                "source",
                "profile_field",
                "value",
                "sensitive"
            ])
        );
    }

    #[test]
    fn form_fill_never_asks_the_model_to_echo_a_label() {
        // The whole point of the two-stage design (`actions::fill_form`'s
        // module doc comment) is token savings: the model is never asked
        // to repeat a label `actions::fill_form` already knows from the
        // UIA snapshot.
        let schema = schema_for("form_fill", false).expect("form_fill is registered");
        let item_props = schema["properties"]["fields"]["items"]["properties"]
            .as_object()
            .unwrap();
        assert!(!item_props.contains_key("label"));
        assert!(!item_props.contains_key("target"));
    }

    #[test]
    fn form_fill_rejects_additional_properties_at_every_level() {
        let schema = schema_for("form_fill", false).expect("form_fill is registered");
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(
            schema["properties"]["fields"]["items"]["additionalProperties"],
            false
        );
    }

    #[test]
    fn form_fill_ignores_rate_difficulty() {
        assert_eq!(
            schema_for("form_fill", false),
            schema_for("form_fill", true)
        );
    }
}
