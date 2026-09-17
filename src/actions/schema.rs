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
//! Only `"verdict"` is registered today, because it is the only proposal
//! kind any built-in action uses. `text_answer`, `calendar_event`,
//! `form_fill` and `text_review` (named in CONTRIBUTING.md's "Add an action
//! in 20 minutes" and the expansion plan's §6) get their own `match` arm
//! here the same day their first action lands, not before -- an
//! unimplemented arm would be untestable dead code (see the
//! `wired-to-nothing` skill).

use serde_json::Value;

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
        _ => None,
    }
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
        assert_eq!(schema_for("calendar_event", false), None);
        assert_eq!(schema_for("totally_made_up", true), None);
    }
}
