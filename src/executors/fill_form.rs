//! The `"fill_form"` executor (#33): consumes a confirmed `form_fill`
//! proposal -- a list of `{target, label, value, sensitive, approved}`
//! fields, each identifying a UIA element captured at "Look" time the same
//! way `replace_text`'s (#32) `TargetRef` does -- re-resolves every field
//! independently at "Do" time, and sets each through `ValuePattern.SetValue`
//! (falling back to focus plus typed Unicode input for a control with no
//! `ValuePattern`). See `executors::target`'s module doc comment for the
//! shared re-resolution, staleness and UIA plumbing this file builds on
//! instead of duplicating.
//!
//! # Refuse the field, not the whole fill
//!
//! Unlike `replace_text` (one target, one refusal aborts the whole
//! executor), `fill_form` has many fields: a problem with one field
//! ([`FieldOutcome::Refused`]) or a field this executor deliberately will
//! not touch ([`FieldOutcome::Skipped`]) never stops the rest from being
//! attempted. [`do_fill`] always returns `Ok` (barring a JSON parse
//! failure before any field is touched); the result card reads
//! [`FieldResult`]'s outcome list, built by [`format_outcomes`].
//!
//! # The per-field refusal chain, in order
//!
//! 1. **Re-resolve the target.** Not found or ambiguous ->
//!    [`RefuseReason::NotResolved`], same two failure shapes
//!    `executors::target::resolve_index` gives `replace_text`.
//! 2. **Password control** -> [`SkipReason::PasswordControl`]. The real
//!    value of a password field is never read into this process (see
//!    `executors::target::ResolvedElement`'s doc comment); this executor
//!    never even considers writing one.
//! 3. **`sensitive && !approved`** -> [`SkipReason::SensitiveUnapproved`].
//!    Expansion plan §15's decision #2 (the sensitivity *default*) is still
//!    owed to the owner; this executor makes no default of its own -- every
//!    proposal must say `sensitive` and `approved` explicitly (see
//!    [`parse_field_fill`]), and this is the one place either flag is read.
//! 4. **Invokable or button control type**
//!    ([`executors::target::is_invokable_control_type`]) or a forbidden
//!    target by name/automation id
//!    ([`super::uia_guard::is_forbidden_target`]) ->
//!    [`RefuseReason::InvokableOrForbiddenTarget`]. Never touches a button,
//!    regardless of its label (CLAUDE.md: "Wingman never presses Send,
//!    Submit, Buy or Pay").
//! 5. **Payment-looking label** ([`is_payment_label`]) ->
//!    [`RefuseReason::PaymentLabel`]. Card number, CVV/CVC, expiry, IBAN,
//!    account/routing number -- the profile has no payment fields to source
//!    these from in the first place (expansion plan §6 rule 4), but a
//!    provider could still propose one by label, so this is checked
//!    independently of where the value came from. **Coordination note:**
//!    the profile module (`src/profile/`, added tonight in parallel on the
//!    still-unmerged `worktree-agent-a16f5b1a39aed9da0` branch that closed
//!    #36) is not present on this branch as of this commit, but its own
//!    closing comment names `profile::denylist::check_field`: a second,
//!    independently written payment-shaped-*value* denylist (this file's
//!    checks a proposal field's *label*, before any write). The two should
//!    be reconciled once both branches land on master -- filed as #215.
//!
//! Whatever survives all five checks is [`FieldOutcome::Filled`]: the prior
//! value is recorded and the new value is written.
//!
//! # Restore
//!
//! [`Undo::undo`] re-resolves every field this executor actually filled and
//! writes its recorded prior value back -- except a field whose text has
//! changed again since this executor wrote it ("the user edited it after
//! the fill"), which is skipped and reported rather than overwritten (same
//! staleness check `replace_text`'s undo uses,
//! `executors::target::is_stale`). See [`decide_restore_action`] for the
//! pure per-field restore decision and [`restore_fields`] for the loop that
//! applies it to every filled field independently.

use anyhow::{anyhow, Result};
use serde_json::Value;
use std::sync::Arc;

use crate::ui::confirm::Confirmed;

use super::target::{
    self, is_invokable_control_type, is_stale, parse_target_ref, TargetRef, TextElementAccess,
};
use super::uia_guard::is_forbidden_target;
use super::{Effect, Executor, Undo};

// ---------------------------------------------------------------------------
// Pure types
// ---------------------------------------------------------------------------

/// One field of a confirmed `form_fill` proposal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldFill {
    pub target: TargetRef,
    pub label: String,
    pub value: String,
    /// Whether the model marked this field as carrying sensitive
    /// information (date of birth and similar -- expansion plan §15). No
    /// default is applied anywhere in this file: a proposal missing this
    /// field is a parse error, never an assumed `false`.
    pub sensitive: bool,
    /// Whether the user ticked this field in the preview card. Only
    /// consulted when `sensitive` is true; ignored otherwise. Also never
    /// defaulted -- see [`parse_field_fill`].
    pub approved: bool,
}

/// A confirmed `form_fill` proposal, parsed once from JSON by
/// [`parse_form_fill`] and never re-read from the raw `Value` again (same
/// split `replace_text::parse_replace_text`'s module doc comment names).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FormFill {
    pub fields: Vec<FieldFill>,
}

/// Why a field was skipped: this executor deliberately never even attempted
/// it, independent of whether the target could be reached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    PasswordControl,
    SensitiveUnapproved,
}

impl std::fmt::Display for SkipReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkipReason::PasswordControl => write!(f, "it is a password field"),
            SkipReason::SensitiveUnapproved => {
                write!(f, "it is marked sensitive and was not approved")
            }
        }
    }
}

/// Why a field was refused: this executor tried, or would have tried, and
/// declined.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefuseReason {
    /// The target could not be re-resolved (not found, or ambiguous). Carries
    /// the underlying resolve error's message.
    NotResolved(String),
    /// A button (or other invokable control type) or a name/automation id
    /// that reads as a final-action control -- never touched, regardless of
    /// label.
    InvokableOrForbiddenTarget,
    /// The label reads as a payment field (card number, CVV/CVC, expiry,
    /// IBAN, account/routing number).
    PaymentLabel,
    /// Re-resolution and every check above passed, but the write itself
    /// failed (no `ValuePattern`, no editable fallback, or a live Win32
    /// error). Carries the underlying write error's message.
    WriteFailed(String),
}

impl std::fmt::Display for RefuseReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            RefuseReason::NotResolved(msg) => write!(f, "its target could not be resolved: {msg}"),
            RefuseReason::InvokableOrForbiddenTarget => {
                write!(f, "it is a button or a final-action control")
            }
            RefuseReason::PaymentLabel => write!(f, "its label reads as a payment field"),
            RefuseReason::WriteFailed(msg) => write!(f, "the write failed: {msg}"),
        }
    }
}

/// What happened to one field, for the result card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldOutcome {
    Filled,
    Skipped(SkipReason),
    Refused(RefuseReason),
}

/// One field's label and what happened to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldResult {
    pub label: String,
    pub outcome: FieldOutcome,
}

/// Deny terms checked as a case-insensitive substring of a field's label.
/// Not tokenized like `uia_guard`'s deny-list (these are prose labels, not
/// invokable-element names: "Card number", "CVV/CVC", not camelCase
/// identifiers), so a phrase-substring match is the right shape here, the
/// same reasoning `uia_guard`'s `PHRASE_DENY` documents for "Place Order".
/// See this file's module doc comment ("Coordination note") for why this is
/// its own table rather than reusing `uia_guard`'s: they gate different
/// things (an invokable element's name vs. a form field's label) and the
/// profile module's own denylist, once it exists, is a third thing again.
const PAYMENT_LABEL_TERMS: &[&str] = &[
    "card number",
    "cvv",
    "cvc",
    "expiry",
    "expiration date",
    "iban",
    "account number",
    "routing number",
];

/// Whether a form field's label reads as a payment field this executor must
/// never fill, regardless of what the model proposed as its value (the
/// profile has no payment fields to source one from -- expansion plan §6
/// rule 4 -- but a provider could still propose one by label). Deliberately
/// conservative: matches only compound terms ("card number", "routing
/// number"), never bare "account" or "card" alone, so an ordinary "Card
/// issuer" or "Account holder name" style field is not the target here --
/// "Account holder name" in particular is accepted as a false negative
/// (documented, not fixed): the profile's own about-me fields cover names,
/// and #215 is where this table gets reconciled with
/// `profile::denylist::check_field` once both branches are on master.
pub fn is_payment_label(label: &str) -> bool {
    let normalized = label.to_lowercase();
    PAYMENT_LABEL_TERMS.iter().any(|t| normalized.contains(t))
}

/// The pure per-field decision, given the field's proposal data and its
/// freshly re-resolved live state. `None` means "proceed to write"; `Some`
/// is the exact outcome to record instead. Checked in the order the module
/// doc comment names: password, then sensitive/unapproved, then
/// invokable-or-forbidden, then payment label.
pub fn evaluate_resolved_field(
    field: &FieldFill,
    resolved: &target::ResolvedElement,
) -> Option<FieldOutcome> {
    if resolved.is_password {
        return Some(FieldOutcome::Skipped(SkipReason::PasswordControl));
    }
    if field.sensitive && !field.approved {
        return Some(FieldOutcome::Skipped(SkipReason::SensitiveUnapproved));
    }
    if is_invokable_control_type(&resolved.control_type)
        || is_forbidden_target(&resolved.name, &resolved.automation_id)
    {
        return Some(FieldOutcome::Refused(
            RefuseReason::InvokableOrForbiddenTarget,
        ));
    }
    if is_payment_label(&field.label) {
        return Some(FieldOutcome::Refused(RefuseReason::PaymentLabel));
    }
    None
}

/// What to do with one already-filled field during Restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestoreAction {
    Restore,
    SkipStale,
    SkipNotFound,
}

/// The pure per-field restore decision: `resolved_current_text` is `None`
/// when the field could not be re-resolved at all (treated the same as a
/// stale field -- there is nothing safe to overwrite), `Some` otherwise.
/// Staleness is checked against `expected_after_write` -- what THIS executor
/// wrote, not the field's original prior value, the same "did *I* still
/// leave this in place" check `replace_text`'s undo closure uses via
/// `executors::target::is_stale`.
pub fn decide_restore_action(
    expected_after_write: &str,
    resolved_current_text: Option<&str>,
) -> RestoreAction {
    match resolved_current_text {
        None => RestoreAction::SkipNotFound,
        Some(actual) => {
            if is_stale(expected_after_write, actual) {
                RestoreAction::SkipStale
            } else {
                RestoreAction::Restore
            }
        }
    }
}

/// Renders a completed fill's per-field outcomes into the `Undo::summary`
/// the result card shows (CLAUDE.md rule 5: "say what happened, not what
/// was intended"). No em dash (rule 11).
pub fn format_outcomes(results: &[FieldResult]) -> String {
    let filled = results
        .iter()
        .filter(|r| r.outcome == FieldOutcome::Filled)
        .count();
    let mut parts = vec![format!(
        "Filled {filled} of {total} field{plural}.",
        total = results.len(),
        plural = if results.len() == 1 { "" } else { "s" }
    )];
    for result in results {
        match &result.outcome {
            FieldOutcome::Filled => {}
            FieldOutcome::Skipped(reason) => {
                parts.push(format!("Skipped \"{}\": {reason}.", result.label));
            }
            FieldOutcome::Refused(reason) => {
                parts.push(format!("Refused \"{}\": {reason}.", result.label));
            }
        }
    }
    parts.join(" ")
}

// ---------------------------------------------------------------------------
// Parsing: JSON Value -> FormFill
// ---------------------------------------------------------------------------

fn parse_field_fill(value: &Value) -> Result<FieldFill> {
    let target_value = value
        .get("target")
        .ok_or_else(|| anyhow!("fill_form: a field has no \"target\" field"))?;
    let target = parse_target_ref(target_value)?;

    let label = value
        .get("label")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("fill_form: a field has no \"label\" field"))?
        .to_string();
    let field_value = value
        .get("value")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("fill_form: field \"{label}\" has no \"value\" field"))?
        .to_string();
    // Neither flag is defaulted (module doc comment, "The per-field refusal
    // chain" step 3, and expansion plan §15's still-owed decision #2): a
    // proposal that omits either is a hard parse error, never a silent
    // false/true.
    let sensitive = value
        .get("sensitive")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("fill_form: field \"{label}\" has no \"sensitive\" field"))?;
    let approved = value
        .get("approved")
        .and_then(Value::as_bool)
        .ok_or_else(|| anyhow!("fill_form: field \"{label}\" has no \"approved\" field"))?;

    Ok(FieldFill {
        target,
        label,
        value: field_value,
        sensitive,
        approved,
    })
}

/// Parses a confirmed `form_fill` proposal's `Value`. Everything past this
/// function in the executor's call chain acts on the typed [`FormFill`],
/// never the raw JSON again.
fn parse_form_fill(value: &Value) -> Result<FormFill> {
    let fields_value = value
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow!("fill_form: proposal has no \"fields\" array"))?;
    anyhow::ensure!(
        !fields_value.is_empty(),
        "fill_form: proposal has an empty \"fields\" array"
    );
    let fields = fields_value
        .iter()
        .map(parse_field_fill)
        .collect::<Result<Vec<_>>>()?;
    Ok(FormFill { fields })
}

// ---------------------------------------------------------------------------
// The executor
// ---------------------------------------------------------------------------

/// `Effect::Writes`: `fill_form` always requires the preview card's "Do it"
/// confirmation (executor design doc; `auto_confirm_read_only` refuses
/// anything that is not `Effect::ReadOnly`).
pub struct FillFormExecutor {
    access: Arc<dyn TextElementAccess>,
}

impl FillFormExecutor {
    /// Production constructor: the real UIA-backed access, shared with
    /// `replace_text` (#33: extracted, not duplicated).
    pub fn new() -> Self {
        Self {
            access: Arc::new(target::com::UiaTextElementAccess),
        }
    }

    /// Test constructor: an injected fake, so [`do_fill`]'s per-field
    /// refusal/restore logic is exercised with no live UIA element.
    #[allow(dead_code)]
    pub fn with_access(access: Arc<dyn TextElementAccess>) -> Self {
        Self { access }
    }
}

impl Default for FillFormExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for FillFormExecutor {
    fn name(&self) -> &'static str {
        "fill_form"
    }

    fn effect(&self) -> Effect {
        Effect::Writes
    }

    fn execute(&self, confirmed: Confirmed<Value>) -> Result<Undo> {
        let value = confirmed.into_value();
        let form = parse_form_fill(&value)?;
        do_fill(&self.access, &form)
    }
}

/// One field this executor actually wrote, kept for Restore.
struct FilledField {
    target: TargetRef,
    label: String,
    prior_value: String,
    expected_after_write: String,
}

/// The forward pass: for each field in order, re-resolve, run the pure
/// refusal chain ([`evaluate_resolved_field`]), and either record it as
/// filled or move on to the next field -- a refused or skipped field never
/// aborts the rest (module doc comment, "Refuse the field, not the whole
/// fill").
fn do_fill(access: &Arc<dyn TextElementAccess>, form: &FormFill) -> Result<Undo> {
    let mut results: Vec<FieldResult> = Vec::with_capacity(form.fields.len());
    let mut filled: Vec<FilledField> = Vec::new();

    for field in &form.fields {
        let resolved = match access.resolve(&field.target) {
            Ok(r) => r,
            Err(e) => {
                results.push(FieldResult {
                    label: field.label.clone(),
                    outcome: FieldOutcome::Refused(RefuseReason::NotResolved(e.to_string())),
                });
                continue;
            }
        };

        if let Some(outcome) = evaluate_resolved_field(field, &resolved) {
            results.push(FieldResult {
                label: field.label.clone(),
                outcome,
            });
            continue;
        }

        match access.write_with_fallback(&field.target, &field.value) {
            Ok(()) => {
                filled.push(FilledField {
                    target: field.target.clone(),
                    label: field.label.clone(),
                    prior_value: resolved.current_text.clone(),
                    expected_after_write: field.value.clone(),
                });
                results.push(FieldResult {
                    label: field.label.clone(),
                    outcome: FieldOutcome::Filled,
                });
            }
            Err(e) => {
                results.push(FieldResult {
                    label: field.label.clone(),
                    outcome: FieldOutcome::Refused(RefuseReason::WriteFailed(e.to_string())),
                });
            }
        }
    }

    let summary = format_outcomes(&results);
    let undo_access = Arc::clone(access);
    Ok(Undo::recording(summary, move || {
        restore_fields(&undo_access, &filled)
    }))
}

/// Restore: re-resolves every filled field independently and writes its
/// prior value back, skipping (and reporting, via the returned `Err`'s
/// message -- the only channel `Undo::undo`'s `FnOnce() -> Result<()>`
/// signature gives this closure back to the caller) any field that was
/// edited again since this executor wrote it, or that can no longer be
/// found. A field that restores cleanly is never rolled back by a sibling
/// field's failure -- each is independent, same as the forward pass.
fn restore_fields(access: &Arc<dyn TextElementAccess>, filled: &[FilledField]) -> Result<()> {
    let mut restored = 0usize;
    let mut skipped: Vec<String> = Vec::new();

    for field in filled {
        let resolved = access.resolve(&field.target).ok();
        let action = decide_restore_action(
            &field.expected_after_write,
            resolved.as_ref().map(|r| r.current_text.as_str()),
        );
        match action {
            RestoreAction::Restore => {
                access.write(&field.target, &field.prior_value)?;
                restored += 1;
            }
            RestoreAction::SkipStale => {
                skipped.push(format!("\"{}\" (edited since the fill)", field.label));
            }
            RestoreAction::SkipNotFound => {
                skipped.push(format!("\"{}\" (could not be found)", field.label));
            }
        }
    }

    if skipped.is_empty() {
        Ok(())
    } else {
        anyhow::bail!(
            "Restored {restored} of {total} fields; skipped: {list}",
            total = filled.len(),
            list = skipped.join(", ")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::HashMap;

    // -----------------------------------------------------------------
    // Fake injectable element model (one entry per field, keyed by target)
    // -----------------------------------------------------------------

    struct FakeField {
        name: String,
        automation_id: String,
        control_type: String,
        is_password: bool,
        text: RefCell<String>,
    }

    struct FakeAccess {
        fields: HashMap<TargetRef, FakeField>,
        write_calls: RefCell<Vec<TargetRef>>,
        unresolvable: Vec<TargetRef>,
    }

    // `RefCell` isn't `Sync`; this fake is never touched from more than one
    // thread in a test (same trade-off `replace_text.rs`'s `FakeAccess`
    // makes).
    unsafe impl Sync for FakeAccess {}

    impl TextElementAccess for FakeAccess {
        fn resolve(&self, target: &TargetRef) -> Result<target::ResolvedElement> {
            anyhow::ensure!(
                !self.unresolvable.contains(target),
                "fake: could not resolve the target"
            );
            let field = self
                .fields
                .get(target)
                .ok_or_else(|| anyhow!("fake: unknown target"))?;
            Ok(target::ResolvedElement {
                name: field.name.clone(),
                automation_id: field.automation_id.clone(),
                control_type: field.control_type.clone(),
                is_password: field.is_password,
                current_text: field.text.borrow().clone(),
            })
        }

        fn write(&self, target: &TargetRef, new_text: &str) -> Result<()> {
            self.write_calls.borrow_mut().push(target.clone());
            let field = self
                .fields
                .get(target)
                .ok_or_else(|| anyhow!("fake: unknown target"))?;
            *field.text.borrow_mut() = new_text.to_string();
            Ok(())
        }
    }

    fn target_for(id: &str) -> TargetRef {
        TargetRef {
            hwnd: 999,
            runtime_id: vec![id.len() as i32],
            automation_id: id.to_string(),
            name: String::new(),
            control_type: "Edit".to_string(),
        }
    }

    fn field(id: &str, control_type: &str, is_password: bool, name: &str, text: &str) -> FakeField {
        FakeField {
            name: name.to_string(),
            automation_id: id.to_string(),
            control_type: control_type.to_string(),
            is_password,
            text: RefCell::new(text.to_string()),
        }
    }

    fn access(fields: Vec<(&str, FakeField)>) -> Arc<FakeAccess> {
        Arc::new(FakeAccess {
            fields: fields
                .into_iter()
                .map(|(id, f)| (target_for(id), f))
                .collect(),
            write_calls: RefCell::new(Vec::new()),
            unresolvable: Vec::new(),
        })
    }

    fn confirmed(value: Value) -> Confirmed<Value> {
        crate::ui::confirm::confirm(
            crate::ui::confirm::Proposal::new(value),
            crate::ui::confirm::user_confirmed(),
        )
    }

    fn target_json(target: &TargetRef) -> Value {
        serde_json::json!({
            "hwnd": target.hwnd,
            "runtime_id": target.runtime_id,
            "automation_id": target.automation_id,
            "name": target.name,
            "control_type": target.control_type,
        })
    }

    fn field_json(id: &str, label: &str, value: &str, sensitive: bool, approved: bool) -> Value {
        serde_json::json!({
            "target": target_json(&target_for(id)),
            "label": label,
            "value": value,
            "sensitive": sensitive,
            "approved": approved,
        })
    }

    fn form_json(fields: Vec<Value>) -> Value {
        serde_json::json!({ "fields": fields })
    }

    // -- parse_form_fill / parse_field_fill --------------------------------

    #[test]
    fn parses_a_full_form() {
        let value = form_json(vec![field_json("name", "Full name", "Ada", false, false)]);
        let form = parse_form_fill(&value).unwrap();
        assert_eq!(form.fields.len(), 1);
        assert_eq!(form.fields[0].label, "Full name");
        assert_eq!(form.fields[0].value, "Ada");
        assert!(!form.fields[0].sensitive);
        assert!(!form.fields[0].approved);
    }

    #[test]
    fn missing_fields_array_is_a_named_error_not_a_panic() {
        let value = serde_json::json!({});
        let err = parse_form_fill(&value).expect_err("no fields array must error");
        assert!(err.to_string().contains("fields"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn empty_fields_array_is_a_named_error() {
        let value = form_json(vec![]);
        let err = parse_form_fill(&value).expect_err("empty fields array must error");
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn field_missing_target_is_a_named_error() {
        let mut value = field_json("name", "Full name", "Ada", false, false);
        value.as_object_mut().unwrap().remove("target");
        let err = parse_form_fill(&form_json(vec![value])).expect_err("no target must error");
        assert!(err.to_string().contains("target"));
    }

    #[test]
    fn field_missing_label_is_a_named_error() {
        let mut value = field_json("name", "Full name", "Ada", false, false);
        value.as_object_mut().unwrap().remove("label");
        let err = parse_form_fill(&form_json(vec![value])).expect_err("no label must error");
        assert!(err.to_string().contains("label"));
    }

    #[test]
    fn field_missing_value_is_a_named_error() {
        let mut value = field_json("name", "Full name", "Ada", false, false);
        value.as_object_mut().unwrap().remove("value");
        let err = parse_form_fill(&form_json(vec![value])).expect_err("no value must error");
        assert!(err.to_string().contains("value"));
    }

    #[test]
    fn field_missing_sensitive_is_a_named_error_no_default_is_assumed() {
        let mut value = field_json("name", "Full name", "Ada", false, false);
        value.as_object_mut().unwrap().remove("sensitive");
        let err = parse_form_fill(&form_json(vec![value])).expect_err("no sensitive must error");
        assert!(err.to_string().contains("sensitive"));
    }

    #[test]
    fn field_missing_approved_is_a_named_error_no_default_is_assumed() {
        let mut value = field_json("name", "Full name", "Ada", false, false);
        value.as_object_mut().unwrap().remove("approved");
        let err = parse_form_fill(&form_json(vec![value])).expect_err("no approved must error");
        assert!(err.to_string().contains("approved"));
    }

    // -- is_payment_label ---------------------------------------------------

    #[test]
    fn denies_payment_looking_labels() {
        let cases = [
            "Card number",
            "CVV",
            "cvc",
            "Expiry date",
            "IBAN",
            "Account number",
            "Routing number",
        ];
        for label in cases {
            assert!(is_payment_label(label), "expected {label:?} to be denied");
        }
    }

    #[test]
    fn allows_ordinary_labels_that_are_not_payment_fields() {
        let cases = [
            "Email",
            "Display name",
            "Discount code",
            "Account settings",
            "Phone number",
            "Shipping address",
        ];
        for label in cases {
            assert!(!is_payment_label(label), "expected {label:?} to be allowed");
        }
    }

    // -- evaluate_resolved_field: the pure per-field decision table --------

    fn resolved(control_type: &str, is_password: bool, name: &str) -> target::ResolvedElement {
        target::ResolvedElement {
            name: name.to_string(),
            automation_id: String::new(),
            control_type: control_type.to_string(),
            is_password,
            current_text: String::new(),
        }
    }

    fn ordinary_field(label: &str, sensitive: bool, approved: bool) -> FieldFill {
        FieldFill {
            target: target_for("f"),
            label: label.to_string(),
            value: "value".to_string(),
            sensitive,
            approved,
        }
    }

    #[test]
    fn password_control_is_skipped() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Password", false, false),
            &resolved("Edit", true, ""),
        );
        assert_eq!(
            outcome,
            Some(FieldOutcome::Skipped(SkipReason::PasswordControl))
        );
    }

    #[test]
    fn sensitive_and_unapproved_is_skipped() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Date of birth", true, false),
            &resolved("Edit", false, ""),
        );
        assert_eq!(
            outcome,
            Some(FieldOutcome::Skipped(SkipReason::SensitiveUnapproved))
        );
    }

    #[test]
    fn sensitive_and_approved_proceeds() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Date of birth", true, true),
            &resolved("Edit", false, ""),
        );
        assert_eq!(outcome, None);
    }

    #[test]
    fn button_control_type_is_refused() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Continue", false, false),
            &resolved("Button", false, "Continue"),
        );
        assert_eq!(
            outcome,
            Some(FieldOutcome::Refused(
                RefuseReason::InvokableOrForbiddenTarget
            ))
        );
    }

    #[test]
    fn forbidden_target_by_name_is_refused() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Notes", false, false),
            &resolved("Edit", false, "Place Order"),
        );
        assert_eq!(
            outcome,
            Some(FieldOutcome::Refused(
                RefuseReason::InvokableOrForbiddenTarget
            ))
        );
    }

    #[test]
    fn payment_looking_label_is_refused() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Card number", false, false),
            &resolved("Edit", false, "Card number"),
        );
        assert_eq!(
            outcome,
            Some(FieldOutcome::Refused(RefuseReason::PaymentLabel))
        );
    }

    #[test]
    fn an_ordinary_field_proceeds() {
        let outcome = evaluate_resolved_field(
            &ordinary_field("Full name", false, false),
            &resolved("Edit", false, "Full name"),
        );
        assert_eq!(outcome, None);
    }

    // -- decide_restore_action ----------------------------------------------

    #[test]
    fn matching_current_text_is_restored() {
        assert_eq!(
            decide_restore_action("Ada", Some("Ada")),
            RestoreAction::Restore
        );
    }

    #[test]
    fn changed_current_text_is_skipped_as_stale() {
        assert_eq!(
            decide_restore_action("Ada", Some("someone typed this")),
            RestoreAction::SkipStale
        );
    }

    #[test]
    fn unresolvable_target_is_skipped_as_not_found() {
        assert_eq!(
            decide_restore_action("Ada", None),
            RestoreAction::SkipNotFound
        );
    }

    // -- format_outcomes ------------------------------------------------

    #[test]
    fn format_outcomes_counts_filled_and_lists_the_rest() {
        let results = vec![
            FieldResult {
                label: "Full name".to_string(),
                outcome: FieldOutcome::Filled,
            },
            FieldResult {
                label: "Password".to_string(),
                outcome: FieldOutcome::Skipped(SkipReason::PasswordControl),
            },
            FieldResult {
                label: "Place order".to_string(),
                outcome: FieldOutcome::Refused(RefuseReason::InvokableOrForbiddenTarget),
            },
        ];
        let summary = format_outcomes(&results);
        assert!(summary.contains("Filled 1 of 3 fields"));
        assert!(summary.contains("Password"));
        assert!(summary.contains("Place order"));
        assert!(!summary.contains('\u{2014}'), "no em dashes: {summary}");
    }

    // -- do_fill / execute on the fake element model -------------------

    #[test]
    fn fills_ordinary_fields_and_records_prior_values() {
        let access = access(vec![
            ("name", field("name", "Edit", false, "Full name", "")),
            ("email", field("email", "Edit", false, "Email", "")),
        ]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("name", "Full name", "Ada Lovelace", false, false),
            field_json("email", "Email", "ada@example.com", false, false),
        ]);

        let undo = executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(
            *access.fields[&target_for("name")].text.borrow(),
            "Ada Lovelace"
        );
        assert_eq!(
            *access.fields[&target_for("email")].text.borrow(),
            "ada@example.com"
        );
        assert!(undo.summary.contains("Filled 2 of 2 fields"));
    }

    #[test]
    fn password_field_is_never_written() {
        let access = access(vec![(
            "pw",
            field("pw", "Edit", true, "Password", "secret"),
        )]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![field_json(
            "pw",
            "Password",
            "new-secret",
            false,
            false,
        )]);

        executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(*access.fields[&target_for("pw")].text.borrow(), "secret");
        assert!(access.write_calls.borrow().is_empty());
    }

    #[test]
    fn sensitive_unapproved_field_is_skipped_others_still_fill() {
        let access = access(vec![
            ("dob", field("dob", "Edit", false, "Date of birth", "")),
            ("name", field("name", "Edit", false, "Full name", "")),
        ]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("dob", "Date of birth", "2000-01-01", true, false),
            field_json("name", "Full name", "Ada", false, false),
        ]);

        let undo = executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(*access.fields[&target_for("dob")].text.borrow(), "");
        assert_eq!(*access.fields[&target_for("name")].text.borrow(), "Ada");
        assert!(undo.summary.contains("Filled 1 of 2 fields"));
    }

    #[test]
    fn button_is_never_touched_other_fields_still_fill() {
        let access = access(vec![
            (
                "btn",
                field("btn", "Button", false, "Place order", "Place order"),
            ),
            ("name", field("name", "Edit", false, "Full name", "")),
        ]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("btn", "Place order", "clicked", false, false),
            field_json("name", "Full name", "Ada", false, false),
        ]);

        executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(
            *access.fields[&target_for("btn")].text.borrow(),
            "Place order",
            "the button's text must never change"
        );
        assert!(!access.write_calls.borrow().contains(&target_for("btn")));
        assert_eq!(*access.fields[&target_for("name")].text.borrow(), "Ada");
    }

    #[test]
    fn payment_label_is_refused_even_though_the_target_resolves_fine() {
        let access = access(vec![(
            "card",
            field("card", "Edit", false, "Card number", ""),
        )]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![field_json(
            "card",
            "Card number",
            "4111 1111 1111 1111",
            false,
            false,
        )]);

        let undo = executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(*access.fields[&target_for("card")].text.borrow(), "");
        assert!(undo.summary.contains("Card number"));
    }

    #[test]
    fn not_found_field_is_refused_and_reported_others_still_fill() {
        let access = Arc::new(FakeAccess {
            fields: vec![(
                target_for("name"),
                field("name", "Edit", false, "Full name", ""),
            )]
            .into_iter()
            .collect(),
            write_calls: RefCell::new(Vec::new()),
            unresolvable: vec![target_for("missing")],
        });
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("missing", "Ghost field", "value", false, false),
            field_json("name", "Full name", "Ada", false, false),
        ]);

        let undo = executor
            .execute(confirmed(form))
            .expect("execute must succeed");
        assert_eq!(*access.fields[&target_for("name")].text.borrow(), "Ada");
        assert!(undo.summary.contains("Ghost field"));
        assert!(undo.summary.contains("Filled 1 of 2 fields"));
    }

    // -- Undo / Restore -----------------------------------------------------

    #[test]
    fn undo_restores_every_filled_field() {
        let access = access(vec![
            (
                "name",
                field("name", "Edit", false, "Full name", "old name"),
            ),
            (
                "email",
                field("email", "Edit", false, "Email", "old@example.com"),
            ),
        ]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("name", "Full name", "Ada", false, false),
            field_json("email", "Email", "ada@example.com", false, false),
        ]);

        let undo = executor.execute(confirmed(form)).unwrap();
        undo.undo().expect("undo must succeed");

        assert_eq!(
            *access.fields[&target_for("name")].text.borrow(),
            "old name"
        );
        assert_eq!(
            *access.fields[&target_for("email")].text.borrow(),
            "old@example.com"
        );
    }

    #[test]
    fn undo_skips_and_reports_a_field_edited_after_the_fill() {
        let access = access(vec![
            (
                "name",
                field("name", "Edit", false, "Full name", "old name"),
            ),
            (
                "email",
                field("email", "Edit", false, "Email", "old@example.com"),
            ),
        ]);
        let executor = FillFormExecutor::with_access(access.clone());
        let form = form_json(vec![
            field_json("name", "Full name", "Ada", false, false),
            field_json("email", "Email", "ada@example.com", false, false),
        ]);

        let undo = executor.execute(confirmed(form)).unwrap();

        // The user edits the "name" field after the fill, before Undo.
        *access.fields[&target_for("name")].text.borrow_mut() = "typed after fill".to_string();

        let err = undo.undo().expect_err("a stale field must be reported");
        assert!(err.to_string().contains("Full name"));
        assert!(err.to_string().contains("edited"));

        // The stale field is left alone; the other field still restores.
        assert_eq!(
            *access.fields[&target_for("name")].text.borrow(),
            "typed after fill",
            "a stale field must not be overwritten"
        );
        assert_eq!(
            *access.fields[&target_for("email")].text.borrow(),
            "old@example.com"
        );
    }

    // -- real Win32 window: three EDIT controls plus one BUTTON ------------
    mod win32 {
        use super::*;
        use std::sync::{Once, OnceLock};
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, LoadCursorW,
            PeekMessageW, RegisterClassExW, ShowWindow, TranslateMessage, BN_CLICKED, CS_HREDRAW,
            CS_VREDRAW, ES_AUTOHSCROLL, ES_PASSWORD, IDC_ARROW, MSG, PM_REMOVE, SW_SHOWNOACTIVATE,
            WM_COMMAND, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
        };

        const CLASS_NAME: windows::core::PCWSTR =
            w!("Wingman.Executors.FillFormTestWindow.test.4e21bc");

        static CLASS_INIT: Once = Once::new();
        static CLASS_OK: OnceLock<bool> = OnceLock::new();
        static BN_CLICKED_COUNT: std::sync::atomic::AtomicU32 =
            std::sync::atomic::AtomicU32::new(0);

        fn instance() -> HINSTANCE {
            let h = unsafe { GetModuleHandleW(None) }.expect("GetModuleHandleW");
            HINSTANCE(h.0)
        }

        unsafe extern "system" fn test_wndproc(
            hwnd: HWND,
            msg: u32,
            wparam: WPARAM,
            lparam: LPARAM,
        ) -> LRESULT {
            if msg == WM_COMMAND && ((wparam.0 >> 16) & 0xFFFF) as u32 == BN_CLICKED {
                BN_CLICKED_COUNT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }

        fn ensure_class_registered(hinstance: HINSTANCE) -> bool {
            CLASS_INIT.call_once(|| {
                let ok = unsafe {
                    let wc = WNDCLASSEXW {
                        cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                        style: CS_HREDRAW | CS_VREDRAW,
                        lpfnWndProc: Some(test_wndproc),
                        hInstance: hinstance,
                        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                        lpszClassName: CLASS_NAME,
                        ..Default::default()
                    };
                    RegisterClassExW(&wc) != 0
                };
                let _ = CLASS_OK.set(ok);
            });
            CLASS_OK.get().copied().unwrap_or(false)
        }

        fn pump_pending_messages() {
            let mut msg = MSG::default();
            unsafe {
                while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                }
            }
        }

        struct TestWindow {
            frame: HWND,
            name: HWND,
            email: HWND,
            password: HWND,
            button: HWND,
        }

        fn build_test_window() -> TestWindow {
            let hinstance = instance();
            assert!(
                ensure_class_registered(hinstance),
                "RegisterClassExW for the test window class"
            );

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman fill_form test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    320,
                    240,
                    None,
                    None,
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (frame)");
            unsafe {
                let _ = ShowWindow(frame, SW_SHOWNOACTIVATE);
            }
            pump_pending_messages();

            let make_edit = |y: i32, style_extra: u32, text: windows::core::PCWSTR| {
                unsafe {
                    CreateWindowExW(
                        Default::default(),
                        w!("EDIT"),
                        text,
                        WS_CHILD
                            | WS_VISIBLE
                            | WS_TABSTOP
                            | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(
                                ES_AUTOHSCROLL as u32 | style_extra,
                            ),
                        10,
                        y,
                        200,
                        20,
                        Some(frame),
                        None,
                        Some(hinstance),
                        None,
                    )
                }
                .expect("CreateWindowExW (edit)")
            };

            let name = make_edit(10, 0, w!(""));
            let email = make_edit(40, 0, w!(""));
            let password = make_edit(70, ES_PASSWORD as u32, w!("secret"));

            let button = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("BUTTON"),
                    w!("Place order"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    10,
                    100,
                    100,
                    24,
                    Some(frame),
                    Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                        501 as *mut _,
                    )),
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (button)");

            pump_pending_messages();

            TestWindow {
                frame,
                name,
                email,
                password,
                button,
            }
        }

        fn find_target(frame: HWND, control_type: &str, skip_automation_ids: &[&str]) -> TargetRef {
            let candidates = target::com::list_all_for_test(frame).expect("list_all_for_test");
            candidates
                .into_iter()
                .filter(|c| c.control_type == control_type)
                .find(|c| !skip_automation_ids.contains(&c.automation_id.as_str()))
                .unwrap_or_else(|| panic!("no unclaimed {control_type} control found in the walk"))
        }

        fn window_text(hwnd: HWND) -> String {
            let mut buf = [0u16; 256];
            let len =
                unsafe { windows::Win32::UI::WindowsAndMessaging::GetWindowTextW(hwnd, &mut buf) };
            String::from_utf16_lossy(&buf[..len as usize])
        }

        #[test]
        fn fills_edit_controls_skips_password_never_touches_the_button_then_restores() {
            let _uia = crate::inputs::lock_uia_test();
            let win = build_test_window();

            // A bare EDIT control gets no automation id or name from Win32's
            // default UIA proxy in this test window (no associated static
            // label), so the three EDIT candidates are only distinguishable
            // from each other by which one is a password field -- resolve
            // each and split on `is_password`, the same live property this
            // executor itself checks before ever writing.
            let real_access = target::com::UiaTextElementAccess;
            let edits: Vec<TargetRef> = target::com::list_all_for_test(win.frame)
                .expect("list_all_for_test")
                .into_iter()
                .filter(|c| c.control_type == "Edit")
                .collect();
            assert_eq!(edits.len(), 3, "expected exactly three EDIT controls");

            let mut password_target = None;
            let mut plain_targets = Vec::new();
            for edit in edits {
                let resolved = real_access
                    .resolve(&edit)
                    .expect("resolve a real EDIT control");
                if resolved.is_password {
                    password_target = Some(edit);
                } else {
                    plain_targets.push(edit);
                }
            }
            let password_target =
                password_target.expect("exactly one EDIT must be a password field");
            assert_eq!(
                plain_targets.len(),
                2,
                "expected two non-password EDIT controls"
            );
            let (edit_a, edit_b) = (plain_targets[0].clone(), plain_targets[1].clone());
            let button_target = find_target(win.frame, "Button", &[]);

            let access: Arc<dyn TextElementAccess> = Arc::new(target::com::UiaTextElementAccess);
            let executor = FillFormExecutor::with_access(access);

            let form = form_json(vec![
                serde_json::json!({
                    "target": target_json(&edit_a), "label": "Full name",
                    "value": "Ada Lovelace", "sensitive": false, "approved": false
                }),
                serde_json::json!({
                    "target": target_json(&edit_b), "label": "Email",
                    "value": "ada@example.com", "sensitive": false, "approved": false
                }),
                serde_json::json!({
                    "target": target_json(&password_target), "label": "Password",
                    "value": "new-secret", "sensitive": false, "approved": false
                }),
                serde_json::json!({
                    "target": target_json(&button_target), "label": "Place order",
                    "value": "clicked", "sensitive": false, "approved": false
                }),
            ]);

            let start = std::time::Instant::now();
            let undo = executor
                .execute(confirmed(form))
                .expect("execute must succeed against real controls");
            let elapsed = start.elapsed();
            // MEASURED (printed verbatim on a `-- --nocapture` run): the
            // real cost of resolving and filling a four-field form against
            // a real Win32 window.
            eprintln!(
                "fill_form execute() took {elapsed:?} against 3 real EDIT controls and 1 BUTTON"
            );

            pump_pending_messages();

            // The two non-password fields were filled with the two intended
            // values -- checked as a set, since nothing in this bare test
            // window (no labels) lets the test know in advance which real
            // HWND ("name" or "email") UIA's walk happened to list first.
            let mut actual_values = vec![window_text(win.name), window_text(win.email)];
            actual_values.sort();
            let mut expected_values =
                vec!["Ada Lovelace".to_string(), "ada@example.com".to_string()];
            expected_values.sort();
            assert_eq!(actual_values, expected_values);

            assert_eq!(
                window_text(win.password),
                "secret",
                "password must be untouched"
            );
            assert_eq!(
                window_text(win.button),
                "Place order",
                "button text must be untouched"
            );
            assert_eq!(
                BN_CLICKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "the button must never receive a click"
            );
            assert!(undo.summary.contains("Password"));
            assert!(undo.summary.contains("Place order"));

            undo.undo().expect("restore must succeed");
            pump_pending_messages();
            assert_eq!(window_text(win.name), "");
            assert_eq!(window_text(win.email), "");

            unsafe {
                let _ = DestroyWindow(win.frame);
            }
        }
    }
}
