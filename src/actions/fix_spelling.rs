//! "Fix spelling and grammar" (#1274): fixes spelling, grammar and punctuation
//! in whatever text box the user is typing in, not only email.
//!
//! It reuses the `text_review` proposal, the capture order of "Review this
//! email" (`review_email::capture_input`: the focused text box, else the
//! selection) and the `replace_text` executor. What differs from the email
//! action: the prompt never touches tone and never mentions email, the card
//! lists only the edits that will really be written, and a text box that
//! cannot be written back through gets the fixed text on the clipboard instead
//! (the same fallback `actions::generic_replace` gives "Translate selection").
//! Pure logic here, unit-tested with fake executors; `app.rs` does the wiring.

use serde_json::{json, Value};

use super::generic_replace::{copy_report, preview_title, ReplaceReport, COPIED_INSTEAD_DETAIL};
use super::review_email::{
    apply_edits, do_review_confirmed, edits_from_value, is_good_to_go, ReviewContext,
    ReviewOutcome, CUT_NOTICE,
};
use super::{Action, InputKind, Prefer};
use crate::executors::Executor;

/// This action's id.
pub const ACTION_ID: &str = "fix-spelling-grammar";

/// The palette row and the router's one-line description of this action. It
/// says what the action does for any text, so the router can offer it for a
/// document, a form field or a chat box.
pub const NAME: &str = "Fix spelling and grammar";

/// The system prompt. The `text_review` schema still asks for a verdict, a
/// tone note and an attachment flag; this prompt has the model leave the last
/// two empty, and the card never shows them.
pub const BASE_PROMPT: &str = "You are shown a piece of text the user is typing. Fix spelling, grammar and punctuation mistakes only. Never change the tone, the style, the word choice or the meaning, and never rewrite a sentence that is already correct. Keep names, numbers, dates, URLs, code and the language of the text exactly as they are, and keep the author's own spelling variant (for example British or American) when it is used consistently. For each individual mistake, propose one edit: the EXACT original text to find (\"before\"), what it should become (\"after\"), and a short reason such as \"spelling\" or \"missing comma\". Keep each edit as small and specific as possible: one edit per individual mistake (for example one misspelled word), never one edit spanning a whole sentence or paragraph just because it contains more than one mistake. Never propose an edit whose \"after\" is identical to its \"before\". If nothing needs fixing, set \"verdict\" to \"good_to_go\" and leave \"edits\" empty. Otherwise set \"verdict\" to \"needs_edits\". List the edits in the order they appear in the text. Leave every other field empty: an empty string for text, false for a flag. Use plain text only in every field: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// Shown when there is no text to fix: nothing was sent to a model.
pub const NO_TEXT_DETAIL: &str = "Click into a text box, or select some text, then try again.";

/// What the card says when nothing needs fixing.
pub const NO_CHANGES_DETAIL: &str = "No spelling or grammar mistakes found.";

/// The built-in action: group Writing, the focused text box then the
/// selection, proposal `text_review`, executor `replace_text`, `confirm =
/// true` because it writes.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: NAME.to_string(),
        group: Some("Writing".to_string()),
        inputs: vec![InputKind::Uia, InputKind::Selection],
        proposal: "text_review".to_string(),
        executor: "replace_text".to_string(),
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

/// What the card should do with a finished review.
#[derive(Debug, Clone, PartialEq)]
pub enum FixPlan {
    /// No text box or selection could be read, so nothing was asked.
    NoText,
    /// Nothing to fix.
    NoChanges,
    /// The text was cut to the length limit: list the edits, offer no write.
    TooLong(String),
    /// Show the edits, then write (or copy) on "Do it".
    Preview {
        /// The write-back target; `None` means "Do it" copies instead.
        ctx: Option<Box<ReviewContext>>,
        /// The whole fixed text, for the clipboard.
        new_text: String,
        /// The preview title.
        title: String,
        /// The `text_review` proposal trimmed to the edits that will apply.
        shown: Value,
    },
}

/// The schema the preview card draws: `text_review` without the verdict, tone
/// note and attachment fields, which mean nothing here.
pub fn display_schema() -> Value {
    let edits = crate::actions::schema::schema_for("text_review", false)
        .and_then(|s| s.get("properties")?.get("edits").cloned())
        .unwrap_or(Value::Null);
    json!({
        "type": "object",
        "properties": {"edits": edits},
        "required": ["edits"],
        "additionalProperties": false
    })
}

/// Decides what to show for a finished review.
pub fn plan(outcome: &ReviewOutcome) -> FixPlan {
    if outcome.original_text.trim().is_empty() {
        return FixPlan::NoText;
    }
    if is_good_to_go(&outcome.proposal) {
        return FixPlan::NoChanges;
    }
    let edits = edits_from_value(&outcome.proposal);
    let applied = apply_edits(&outcome.original_text, &edits);
    // An edit that changes nothing is not a fix.
    let real: Vec<_> = applied
        .applied
        .iter()
        .filter(|e| e.before != e.after)
        .collect();
    if real.is_empty() || applied.new_text == outcome.original_text {
        return FixPlan::NoChanges;
    }
    if outcome.cut {
        let lines = real
            .iter()
            .map(|e| format!("{} -> {}", e.before, e.after))
            .collect::<Vec<_>>()
            .join("\n");
        return FixPlan::TooLong(format!("{lines}\n\n{CUT_NOTICE}"));
    }
    let shown_edits: Vec<Value> = real
        .iter()
        .map(|e| json!({"before": e.before, "after": e.after, "reason": e.reason}))
        .collect();
    let ctx = outcome.target.clone().map(|target| {
        Box::new(ReviewContext {
            target,
            original_text: outcome.original_text.clone(),
            new_text: applied.new_text.clone(),
        })
    });
    FixPlan::Preview {
        title: preview_title(NAME, ctx.is_some()),
        ctx,
        new_text: applied.new_text,
        shown: json!({"edits": shown_edits}),
    }
}

/// Applies a confirmed fix: writes `ctx` through `replace` when there is one
/// and it works, otherwise copies `new_text` to the clipboard so the fix is
/// never lost.
pub fn write_or_copy(
    replace: &dyn Executor,
    copy: &dyn Executor,
    ctx: Option<&ReviewContext>,
    new_text: &str,
) -> ReplaceReport {
    if new_text.trim().is_empty() {
        return ReplaceReport::NothingToReplace;
    }
    let copy_value = || {
        crate::ui::confirm::confirm(
            crate::ui::confirm::Proposal::new(json!({"text": new_text})),
            crate::ui::confirm::user_confirmed(),
        )
    };
    let Some(ctx) = ctx else {
        return copy_report(copy, copy_value(), COPIED_INSTEAD_DETAIL.to_string(), None);
    };
    match do_review_confirmed(replace, ctx) {
        Ok(undo) => ReplaceReport::Replaced(undo.summary),
        Err(e) => {
            let reason = format!("{e:#}");
            let detail = format!(
                "Could not replace in place: {}. The text is on the clipboard.",
                reason.trim_end_matches('.')
            );
            copy_report(copy, copy_value(), detail, Some(reason))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::review_email::{CapturedTarget, InputSource, ReplaceTarget};
    use crate::executors::{Effect, Undo};
    use crate::ui::confirm::Confirmed;
    use std::sync::Mutex;

    fn target() -> ReplaceTarget {
        ReplaceTarget::Whole(CapturedTarget {
            hwnd: 9,
            runtime_id: vec![1, 2],
            automation_id: "box".into(),
            name: "Body".into(),
            control_type: "Edit".into(),
        })
    }

    fn proposal(edits: Value) -> Value {
        json!({
            "edits": edits,
            "verdict": "needs_edits",
            "tone_note": "",
            "missing_attachment": false
        })
    }

    fn outcome(text: &str, edits: Value, target: Option<ReplaceTarget>) -> ReviewOutcome {
        ReviewOutcome {
            proposal: proposal(edits),
            original_text: text.to_string(),
            target,
            source: InputSource::ComposeBody,
            cut: false,
            action_id: ACTION_ID.to_string(),
        }
    }

    fn three_typos() -> Value {
        json!([
            {"before": "recieve", "after": "receive", "reason": "spelling"},
            {"before": "teh", "after": "the", "reason": "spelling"},
            {"before": "its", "after": "it's", "reason": "apostrophe"}
        ])
    }

    const TEXT: &str = "I will recieve teh parcel when its sent";

    // -- the action itself ---------------------------------------------------

    #[test]
    fn builtin_action_reuses_text_review_and_replace_text() {
        let a = builtin_action();
        assert_eq!(a.id, "fix-spelling-grammar");
        assert_eq!(a.name, "Fix spelling and grammar");
        assert_eq!(a.group.as_deref(), Some("Writing"));
        assert_eq!(a.proposal, "text_review");
        assert_eq!(a.executor, "replace_text");
        assert!(a.confirm);
        assert!(a.enabled);
        assert_eq!(a.inputs, vec![InputKind::Uia, InputKind::Selection]);
    }

    #[test]
    fn prompt_and_name_have_no_email_wording() {
        let a = builtin_action();
        for text in [a.prompt.as_str(), a.name.as_str(), NO_TEXT_DETAIL] {
            let lower = text.to_lowercase();
            assert!(!lower.contains("email"), "{text}");
            assert!(!lower.contains("e-mail"), "{text}");
            assert!(!lower.contains("attach"), "{text}");
        }
        assert!(!a.prompt.is_empty());
    }

    #[test]
    fn prompt_fixes_spelling_grammar_and_punctuation_but_never_tone() {
        let p = BASE_PROMPT.to_lowercase();
        for word in ["spelling", "grammar", "punctuation"] {
            assert!(p.contains(word), "{word}");
        }
        assert!(p.contains("never") && p.contains("tone"), "{p}");
        assert!(!BASE_PROMPT.contains('\u{2014}'), "no em dashes");
    }

    #[test]
    fn the_router_description_says_any_text_not_email() {
        let resolved = crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]);
        let catalogue = crate::ui::palette_model::catalogue(&resolved);
        let candidates = crate::router::candidates_from_catalogue(&catalogue);
        let c = candidates
            .iter()
            .find(|c| c.id == ACTION_ID)
            .expect("offered to the router");
        assert_eq!(c.description, "Fix spelling and grammar");
        assert!(!c.description.to_lowercase().contains("email"));
    }

    #[test]
    fn it_is_in_the_built_in_list_right_after_review_this_email() {
        let ids: Vec<String> = crate::actions::builtin_actions()
            .into_iter()
            .map(|a| a.id)
            .collect();
        let review = ids.iter().position(|i| i == "review-this-email").unwrap();
        assert_eq!(ids[review + 1], ACTION_ID);
    }

    // -- plan ----------------------------------------------------------------

    #[test]
    fn three_typos_become_a_preview_that_writes_the_fixed_text() {
        let FixPlan::Preview {
            ctx,
            new_text,
            title,
            shown,
        } = plan(&outcome(TEXT, three_typos(), Some(target())))
        else {
            panic!("expected a preview");
        };
        assert_eq!(new_text, "I will receive the parcel when it's sent");
        let ctx = ctx.expect("has a target");
        assert_eq!(ctx.new_text, new_text);
        assert_eq!(ctx.original_text, TEXT);
        assert_eq!(title, "Fix spelling and grammar");
        assert_eq!(shown["edits"].as_array().unwrap().len(), 3);
        assert_eq!(shown["edits"][0]["before"], "recieve");
    }

    #[test]
    fn a_text_box_that_cannot_be_written_previews_a_copy() {
        let FixPlan::Preview { ctx, title, .. } = plan(&outcome(TEXT, three_typos(), None)) else {
            panic!("expected a preview");
        };
        assert!(ctx.is_none());
        assert_eq!(title, "Fix spelling and grammar (copies to the clipboard)");
    }

    #[test]
    fn an_edit_that_will_not_apply_is_not_shown() {
        let edits = json!([
            {"before": "teh", "after": "the", "reason": "spelling"},
            {"before": "zzz", "after": "yyy", "reason": "made up"}
        ]);
        let FixPlan::Preview { shown, .. } = plan(&outcome(TEXT, edits, Some(target()))) else {
            panic!("expected a preview");
        };
        assert_eq!(shown["edits"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn no_edits_a_good_verdict_or_unmatched_edits_mean_no_changes() {
        assert_eq!(
            plan(&outcome(TEXT, json!([]), Some(target()))),
            FixPlan::NoChanges
        );
        let mut good = outcome(TEXT, three_typos(), Some(target()));
        good.proposal["verdict"] = json!("good_to_go");
        assert_eq!(plan(&good), FixPlan::NoChanges);
        let unmatched = json!([{"before": "zzz", "after": "yyy", "reason": "x"}]);
        assert_eq!(
            plan(&outcome(TEXT, unmatched, Some(target()))),
            FixPlan::NoChanges
        );
        let same = json!([{"before": "teh", "after": "teh", "reason": "x"}]);
        assert_eq!(
            plan(&outcome(TEXT, same, Some(target()))),
            FixPlan::NoChanges
        );
    }

    #[test]
    fn no_readable_text_asks_nothing_and_says_so() {
        let mut o = outcome("", json!([]), None);
        o.source = InputSource::Screen;
        o.proposal = Value::Null;
        assert_eq!(plan(&o), FixPlan::NoText);
    }

    #[test]
    fn a_cut_selection_lists_the_edits_and_offers_no_write() {
        let mut o = outcome(TEXT, three_typos(), None);
        o.source = InputSource::Selection;
        o.cut = true;
        let FixPlan::TooLong(detail) = plan(&o) else {
            panic!("expected too long");
        };
        assert!(detail.contains("recieve"), "{detail}");
        assert!(detail.contains(CUT_NOTICE), "{detail}");
    }

    #[test]
    fn the_preview_schema_lists_only_the_edits() {
        let schema = display_schema();
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props.keys().collect::<Vec<_>>(), ["edits"]);
        assert_eq!(schema["required"], json!(["edits"]));
        assert_eq!(
            schema["properties"]["edits"],
            crate::actions::schema::schema_for("text_review", false).unwrap()["properties"]
                ["edits"]
        );
    }

    #[test]
    fn a_crlf_text_keeps_its_line_endings() {
        let text = "teh first\r\nand teh second";
        let edits = json!([
            {"before": "teh", "after": "the", "reason": "s"},
            {"before": "teh", "after": "the", "reason": "s"}
        ]);
        let FixPlan::Preview { new_text, .. } = plan(&outcome(text, edits, None)) else {
            panic!("expected a preview");
        };
        assert_eq!(new_text, "the first\r\nand the second");
    }

    // -- write_or_copy -------------------------------------------------------

    struct Fake {
        name: &'static str,
        result: Mutex<Option<anyhow::Result<Undo>>>,
        seen: Mutex<Option<Value>>,
    }

    impl Fake {
        fn new(name: &'static str, result: anyhow::Result<Undo>) -> Self {
            Fake {
                name,
                result: Mutex::new(Some(result)),
                seen: Mutex::new(None),
            }
        }
        fn seen(&self) -> Option<Value> {
            self.seen.lock().unwrap().clone()
        }
    }

    impl Executor for Fake {
        fn name(&self) -> &'static str {
            self.name
        }
        fn effect(&self) -> Effect {
            Effect::Writes
        }
        fn execute(&self, confirmed: Confirmed<Value>) -> anyhow::Result<Undo> {
            *self.seen.lock().unwrap() = Some(confirmed.into_value());
            self.result.lock().unwrap().take().expect("run once")
        }
    }

    fn ctx() -> ReviewContext {
        ReviewContext {
            target: target(),
            original_text: TEXT.into(),
            new_text: "fixed".into(),
        }
    }

    #[test]
    fn a_working_write_replaces_the_whole_text_and_copies_nothing() {
        let replace = Fake::new("replace_text", Ok(Undo::none("wrote it")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let report = write_or_copy(&replace, &copy, Some(&ctx()), "fixed");
        assert_eq!(report, ReplaceReport::Replaced("wrote it".into()));
        let sent = replace.seen().expect("the write ran");
        assert_eq!(sent["new_text"], "fixed");
        assert_eq!(sent["mode"], "replace_all");
        assert_eq!(sent["expected_current_text"], TEXT);
        assert_eq!(copy.seen(), None);
    }

    #[test]
    fn no_target_copies_the_fixed_text_and_never_writes() {
        let replace = Fake::new("replace_text", Ok(Undo::none("wrote it")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let report = write_or_copy(&replace, &copy, None, "fixed");
        assert_eq!(
            report,
            ReplaceReport::Copied(crate::actions::generic_replace::COPIED_INSTEAD_DETAIL.into())
        );
        assert_eq!(replace.seen(), None);
        assert_eq!(copy.seen(), Some(json!({"text": "fixed"})));
    }

    #[test]
    fn a_failed_write_falls_back_to_the_clipboard_and_says_why() {
        let replace = Fake::new("replace_text", Err(anyhow::anyhow!("the text changed")));
        let copy = Fake::new("clipboard", Ok(Undo::none("copied")));
        let report = write_or_copy(&replace, &copy, Some(&ctx()), "fixed");
        let ReplaceReport::Copied(detail) = report else {
            panic!("expected a copy, got {report:?}");
        };
        assert!(detail.contains("the text changed"), "{detail}");
        assert!(detail.contains("on the clipboard"), "{detail}");
        assert_eq!(copy.seen(), Some(json!({"text": "fixed"})));
    }

    #[test]
    fn when_both_fail_the_report_names_both() {
        let replace = Fake::new("replace_text", Err(anyhow::anyhow!("no value pattern")));
        let copy = Fake::new("clipboard", Err(anyhow::anyhow!("clipboard busy")));
        let report = write_or_copy(&replace, &copy, Some(&ctx()), "fixed");
        let ReplaceReport::Failed(why) = report else {
            panic!("expected a failure, got {report:?}");
        };
        assert!(why.contains("no value pattern") && why.contains("clipboard busy"));
    }

    #[test]
    fn blank_text_never_blanks_the_box_or_the_clipboard() {
        let replace = Fake::new("replace_text", Ok(Undo::none("x")));
        let copy = Fake::new("clipboard", Ok(Undo::none("x")));
        let report = write_or_copy(&replace, &copy, Some(&ctx()), "  \n");
        assert_eq!(report, ReplaceReport::NothingToReplace);
        assert_eq!(replace.seen(), None);
        assert_eq!(copy.seen(), None);
    }
}
