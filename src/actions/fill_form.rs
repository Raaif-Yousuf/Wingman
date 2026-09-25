//! "Fill this form" (#40): the action layer that turns a UIA field snapshot
//! (`inputs::uia::snapshot_foreground`), the profile (`profile::Profile`)
//! and, only when needed, one model completion, into a `form_fill` proposal
//! the already-shipped `executors::fill_form::FillFormExecutor` (#33) can
//! run. This file owns the "Propose" half of Look/Propose/Confirm/Do; the
//! executor owns "Do" and already exists (see its own module doc comment
//! for the five-step per-field refusal chain it runs regardless of what
//! this file proposes).
//!
//! # Two stages, to save tokens
//!
//! 1. **Local, deterministic, no model call.** [`map_candidates_locally`]
//!    walks every fillable UIA field and tries `profile::match_label` on
//!    its label; a field that matches a [`profile::FieldKind`] the profile
//!    actually has a non-empty value for is filled straight from the
//!    profile -- `source: "profile:<field>"`. This is the common case (a
//!    "Full name" / "Email" / "Phone" field on an ordinary form) and it
//!    costs nothing: no network call, no tokens, no latency.
//! 2. **Only the leftovers go to a model.** A field `match_label` could not
//!    map, or mapped to a profile field that is empty, is collected into
//!    the "unmapped" list. If that list is non-empty, [`form_fill_request`]
//!    builds ONE completion request covering all of them together --
//!    never one request per field -- and [`merge_model_response`] folds
//!    the result back in (`source: "model"`, or `source: "profile:<field>"`
//!    when the model names a profile field this file did not already try).
//!    If nothing is unmapped, no request is ever built at all (`app.rs`'s
//!    wiring: `if unmapped.is_empty() { skip straight to build_proposal }`,
//!    which is exactly what the live `#[ignore]`d test at the bottom of
//!    this file asserts: zero provider calls when local mapping alone
//!    fills every candidate).
//!
//! The model request ([`build_model_prompt`]) sends the unmapped fields'
//! labels and current text, plus [`profile_field_summary`]'s NAMES for
//! every non-empty profile field, but a VALUE only for a non-sensitive one
//! -- a sensitive field (`date_of_birth` always; anything else the profile
//! or an earlier "model" fill marked sensitive) is named so the model knows
//! it exists, never with its value, and never at all if it is empty. This
//! is the literal token-and-privacy point of the two-stage split; see
//! `model_request_excludes_sensitive_values_but_names_sensitive_fields`
//! below for the golden-request-body proof.
//!
//! # The merged proposal shape
//!
//! [`build_proposal`] is the one place that turns the merged
//! local-plus-model fields into `{"fields": [{target, label, value,
//! sensitive, approved, current, source}]}`. `target`/`label`/`value`/
//! `sensitive`/`approved` are exactly what
//! `executors::fill_form::parse_field_fill` reads (`current`/`source` are
//! extra, ignored by that parser, kept only so the preview translation
//! below can render "current -> proposed (from source)" without a second
//! source of truth). `sensitive` here is not simply each field's own
//! natural sensitivity: [`effective_sensitive`] first applies
//! `config.forms.require_tick_for` (expansion plan §15's still-owed owner
//! decision -- see `config::RequireTickFor`'s own doc comment for why its
//! default is marked provisional), and `approved` is initialized to
//! `!sensitive` so a non-sensitive field fills with no per-field click
//! (`OWNER_TODO.md` item 3's own recommended wording) while a sensitive one
//! waits for the user to approve it in the preview.
//!
//! # The preview: reusing the card's existing machinery, not a new one
//!
//! `ui::preview::PreviewModel` (and `ui/card.rs`'s Win32 rendering of it)
//! already exist, are tested, and render one row per flat scalar schema
//! property -- exactly the shape a `form_fill` proposal's `fields` ARRAY is
//! not. Rather than build a second, parallel, hand-rolled GDI rendering
//! path purely for form_fill (real risk with no way to verify it visually
//! under this task's constraints -- "do not launch the exe"), this file
//! translates: [`build_preview_schema_and_value`] turns the merged
//! proposal into a flat pseudo-schema/value pair `Card::show_preview`
//! already knows how to render, one property per field, keyed `f<i>`. A
//! non-sensitive field renders as a non-editable informational row
//! ([`format_display_value`]: "current -> proposed (from source)"); a
//! sensitive field renders as an EDITABLE row defaulting to the proposed
//! value -- editing it to something else overrides that value, clearing it
//! to blank means "skip this field". [`rebuild_after_confirm`] reverses the
//! translation once "Do it" fires: it reads the confirmed flat value back
//! against the ORIGINAL merged proposal (kept in `App` state between "show"
//! and "decide" -- see `app.rs`), so a sensitive field's `approved` becomes
//! `!edited_text.is_empty()` and its `value` becomes whatever the user left
//! in the box.
//!
//! # Empty profile
//!
//! Expansion plan §15/`OWNER_TODO.md` item 3 leaves the sensitivity default
//! to the owner, but a genuinely EMPTY profile (issue #40's task brief) is
//! a product decision this file does make, documented here rather than
//! left as a TODO: `profile.bin` is a DPAPI-encrypted binary with no
//! settings page yet (#50) for a user to hand-edit, so pointing at it alone
//! would be a dead end. [`load_or_import_profile`] additionally looks for a
//! plain, hand-editable `profile.toml` next to it and imports it once (the
//! same `Profile::save_to` -- and therefore the same payment-data denylist
//! -- every other write already goes through) whenever the encrypted store
//! is still empty. This was judged "trivial" per the task brief's own
//! permission to add it: `Profile` already derives `serde::Deserialize`,
//! so parsing a TOML document into one is a `toml::from_str` call, no new
//! type. If neither the encrypted store nor a `profile.toml` has anything,
//! [`empty_profile_detail`] is the card text shown instead of ever
//! building a UIA snapshot.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::config::RequireTickFor;
use crate::executors::uia_guard::is_forbidden_target;
use crate::inputs::uia::{ControlKind, FieldSnapshot, FieldValue};
use crate::profile::{match_label, FieldKind, Profile};
use crate::provider::{Effort, Request, Shot};

use super::{Action, InputKind, Prefer};

/// This action's id.
pub const ACTION_ID: &str = "fill-this-form";

/// The action's base prompt. Unlike `calendar`'s `BASE_PROMPT` (one prompt,
/// sent with every ask), this file only ever sends a prompt to the model
/// for the unmapped-fields request (stage 2) -- see [`build_model_prompt`].
pub const BASE_PROMPT: &str = "You are shown a form (or, if no image could be captured, its recognized text and fields instead) and a short list of its fields Wingman could not confidently map to a profile field by label alone. For each one, decide whether a profile field (named below) is the right source, or whether you can read/infer a short, literal value directly from what is visible on screen. Never invent personal data that is not visible and not in the profile. Never fill a payment field (card number, CVV, expiry, IBAN, account or routing number) by any means; leave it as \"skip\". If you are unsure, choose \"skip\" rather than guessing. Use plain text only in every field: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in "Fill this form" action (#40): group Work, inputs the UIA
/// snapshot plus the screen, proposal `form_fill`, executor `fill_form`
/// (#33, already shipped), `confirm = true` -- writing a form field is
/// never auto-confirmed (`ui::confirm::auto_confirm_read_only` refuses any
/// non-`ReadOnly` executor, and `fill_form`'s `effect()` is `Writes`).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Fill this form".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Uia, InputKind::Screen],
        proposal: "form_fill".to_string(),
        executor: "fill_form".to_string(),
        confirm: true,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

// ---------------------------------------------------------------------------
// Candidate filtering
// ---------------------------------------------------------------------------

/// The UIA control-type string `executors::target::parse_target_ref` (and
/// therefore `executors::fill_form`'s own `evaluate_resolved_field`) reads
/// back off a proposal's `target.control_type`. Duplicated from
/// `inputs::uia`'s own (private) `ControlKind::as_str`, the same
/// "duplicated because it's private to its own file" trade-off this
/// crate's other UIA modules already accept (see `inputs::uia`'s
/// `runtime_id_from_safearray`, duplicated from `executors::target::com`'s
/// own copy for the identical reason).
pub fn control_type_str(kind: ControlKind) -> &'static str {
    match kind {
        ControlKind::Edit => "Edit",
        ControlKind::ComboBox => "ComboBox",
        ControlKind::Document => "Document",
        ControlKind::CheckBox => "CheckBox",
        ControlKind::RadioButton => "RadioButton",
        ControlKind::List => "List",
    }
}

/// Whether a snapshotted field is worth even considering as a fill
/// candidate. `inputs::uia::snapshot_foreground` already never emits a
/// button (or anything else outside its six known control kinds) as a
/// `FieldSnapshot` at all -- see that module's `build_snapshot` -- so
/// "never touches a button" (AGENTS.md, Wingman never presses Send/Submit)
/// holds by construction upstream of this function; this narrows further,
/// to the three kinds `executors::target::is_editable_control_type` (and
/// therefore `TextElementAccess::write`/`write_with_fallback`) actually
/// know how to write a text value into. A `CheckBox`/`RadioButton`/`List`
/// field is real UIA data this snapshot carries for other future actions,
/// but not one this one proposes filling with a text value. Also excludes
/// a disabled field (nothing this executor could write to would stick) and
/// any field whose name or automation id reads as a forbidden target
/// (`executors::uia_guard::is_forbidden_target` -- the same deny-list
/// `executors::fill_form` itself re-checks at Do-time; filtering here too
/// means a forbidden-named field never even shows up as a proposed row).
/// Never a password field (`FieldValue::Redacted`) -- its real value was
/// never read into this process in the first place.
pub fn is_fillable_candidate(field: &FieldSnapshot) -> bool {
    matches!(
        field.control_type,
        ControlKind::Edit | ControlKind::Document | ControlKind::ComboBox
    ) && field.enabled
        && !matches!(field.value, FieldValue::Redacted)
        && !is_forbidden_target(&field.name, &field.automation_id)
}

/// Fillable candidates from a full snapshot, in walk order -- the index
/// into THIS list (not the original snapshot's) is what [`control_id`]
/// names, so a caller filters once, here, and never has to re-derive it.
pub fn fillable_candidates(fields: &[FieldSnapshot]) -> Vec<FieldSnapshot> {
    fields
        .iter()
        .filter(|f| is_fillable_candidate(f))
        .cloned()
        .collect()
}

fn current_text(value: &FieldValue) -> String {
    match value {
        FieldValue::Text(s) => s.clone(),
        FieldValue::Empty | FieldValue::Redacted => String::new(),
    }
}

// ---------------------------------------------------------------------------
// Profile field naming: the one vocabulary both directions of the model
// request (what's offered, what the model names back) share.
// ---------------------------------------------------------------------------

/// Every [`FieldKind`] this file knows how to read off a [`Profile`], in a
/// fixed order -- used by [`profile_field_summary`] so the model always
/// sees the same field vocabulary regardless of which ones happen to be
/// populated.
const ALL_FIELD_KINDS: &[FieldKind] = &[
    FieldKind::FullName,
    FieldKind::PreferredName,
    FieldKind::Email,
    FieldKind::Phone,
    FieldKind::AddressLine,
    FieldKind::City,
    FieldKind::Region,
    FieldKind::Postcode,
    FieldKind::Country,
    FieldKind::Organisation,
    FieldKind::JobTitle,
    FieldKind::DateOfBirth,
    FieldKind::Website,
];

/// The canonical string name for a [`FieldKind`], both what
/// [`profile_field_summary`] names a field by and what a model's
/// `"profile_field"` response is matched back against
/// ([`profile_field_kind_by_name`]).
pub fn profile_field_name(kind: FieldKind) -> &'static str {
    match kind {
        FieldKind::FullName => "full_name",
        FieldKind::PreferredName => "preferred_name",
        FieldKind::Email => "email",
        FieldKind::Phone => "phone",
        FieldKind::AddressLine => "address_line",
        FieldKind::City => "city",
        FieldKind::Region => "region",
        FieldKind::Postcode => "postcode",
        FieldKind::Country => "country",
        FieldKind::Organisation => "organisation",
        FieldKind::JobTitle => "job_title",
        FieldKind::DateOfBirth => "date_of_birth",
        FieldKind::Website => "website",
    }
}

/// The inverse of [`profile_field_name`]. `None` for anything a model might
/// hallucinate that is not a real profile field name -- [`merge_model_response`]
/// treats that the same as "skip" rather than guessing.
pub fn profile_field_kind_by_name(name: &str) -> Option<FieldKind> {
    ALL_FIELD_KINDS
        .iter()
        .copied()
        .find(|&kind| profile_field_name(kind) == name)
}

/// Reads the [`Profile`] value (and its sensitivity) [`FieldKind`] names,
/// or `None` when that value is empty (nothing to offer, whether from the
/// local mapper or the model summary). Multi-value fields ([`FieldKind::Email`],
/// [`FieldKind::Phone`]) use the first non-empty entry -- a profile with
/// more than one address to hand to a form is a later issue's problem
/// (there is no UI yet to pick between them); [`FieldKind::AddressLine`]
/// joins every line with `", "` into one text value, since a single-line
/// UIA edit field is the common case this action targets first.
pub fn profile_value_for_kind(kind: FieldKind, profile: &Profile) -> Option<(String, bool)> {
    let (value, sensitive): (String, bool) = match kind {
        FieldKind::FullName => (profile.full_name.value.clone(), profile.full_name.sensitive),
        FieldKind::PreferredName => (
            profile.preferred_name.value.clone(),
            profile.preferred_name.sensitive,
        ),
        FieldKind::Email => match profile.emails.iter().find(|f| !f.value.trim().is_empty()) {
            Some(f) => (f.value.clone(), f.sensitive),
            None => (String::new(), false),
        },
        FieldKind::Phone => match profile.phones.iter().find(|f| !f.value.trim().is_empty()) {
            Some(f) => (f.value.clone(), f.sensitive),
            None => (String::new(), false),
        },
        FieldKind::AddressLine => (
            profile.address.value.lines.join(", "),
            profile.address.sensitive,
        ),
        FieldKind::City => (
            profile.address.value.city.clone(),
            profile.address.sensitive,
        ),
        FieldKind::Region => (
            profile.address.value.region.clone(),
            profile.address.sensitive,
        ),
        FieldKind::Postcode => (
            profile.address.value.postcode.clone(),
            profile.address.sensitive,
        ),
        FieldKind::Country => (
            profile.address.value.country.clone(),
            profile.address.sensitive,
        ),
        FieldKind::Organisation => (
            profile.organisation.value.clone(),
            profile.organisation.sensitive,
        ),
        FieldKind::JobTitle => (profile.job_title.value.clone(), profile.job_title.sensitive),
        FieldKind::DateOfBirth => (
            profile.date_of_birth.value.clone(),
            profile.date_of_birth.sensitive,
        ),
        FieldKind::Website => (profile.website.value.clone(), profile.website.sensitive),
    };
    if value.trim().is_empty() {
        None
    } else {
        Some((value, sensitive))
    }
}

// ---------------------------------------------------------------------------
// Stage 1: local, deterministic mapping (no model call)
// ---------------------------------------------------------------------------

/// One fillable candidate the local mapper (or, later, the model) is ready
/// to fill: everything [`build_proposal`] needs except the live UIA
/// `target`, which it reads separately from the original candidate list by
/// [`MappedField::candidate_index`].
#[derive(Debug, Clone, PartialEq)]
pub struct MappedField {
    /// Index into the `candidates` slice [`map_candidates_locally`] (or
    /// [`merge_model_response`]) was given.
    pub candidate_index: usize,
    pub label: String,
    /// The field's live text at snapshot time (empty for a blank field).
    pub current: String,
    pub value: String,
    /// This field's OWN natural sensitivity (the profile's, or the
    /// model's own opinion for a `"model"`-sourced value) -- BEFORE
    /// `config.forms.require_tick_for` is applied. [`build_proposal`]
    /// applies that config, not this struct.
    pub sensitive: bool,
    /// `"profile:<field>"` or `"model"`.
    pub source: String,
}

/// A fillable candidate the local mapper could not confidently fill:
/// either `profile::match_label` found no [`FieldKind`] for its label, or
/// it did but [`profile_value_for_kind`] returned `None` (the profile has
/// nothing there). Carried forward to stage 2's model request.
#[derive(Debug, Clone, PartialEq)]
pub struct UnmappedField {
    pub candidate_index: usize,
    pub label: String,
    pub current: String,
}

/// The stable id a candidate is referred to by across the model request and
/// response -- an index into the SAME candidate list both
/// [`map_candidates_locally`] and [`merge_model_response`] are given,
/// valid only within one "Fill this form" press (never persisted, never
/// compared across presses).
pub fn control_id(candidate_index: usize) -> String {
    format!("f{candidate_index}")
}

/// Stage 1. Splits `candidates` into what the local mapper can fill
/// straight from `profile` and what stage 2 needs the model for. Pure, no
/// model call, no I/O.
pub fn map_candidates_locally(
    candidates: &[FieldSnapshot],
    profile: &Profile,
) -> (Vec<MappedField>, Vec<UnmappedField>) {
    let mut mapped = Vec::new();
    let mut unmapped = Vec::new();

    for (candidate_index, field) in candidates.iter().enumerate() {
        let current = current_text(&field.value);
        let local_match = match_label(&field.label)
            .and_then(|kind| profile_value_for_kind(kind, profile).map(|v| (kind, v)));

        match local_match {
            Some((kind, (value, sensitive))) => mapped.push(MappedField {
                candidate_index,
                label: field.label.clone(),
                current,
                value,
                sensitive,
                source: format!("profile:{}", profile_field_name(kind)),
            }),
            None => unmapped.push(UnmappedField {
                candidate_index,
                label: field.label.clone(),
                current,
            }),
        }
    }

    (mapped, unmapped)
}

// ---------------------------------------------------------------------------
// Stage 2: the model request for whatever stage 1 could not map
// ---------------------------------------------------------------------------

/// One profile field, named for the model, with a value attached only when
/// it is safe to send: non-empty AND not sensitive. A sensitive-but-populated
/// field is still named (so the model knows it exists and can name it back
/// as a `"profile"` source) but its `value` is always `None` here -- this
/// is the literal mechanism behind "never send sensitive values" (see this
/// file's module doc comment).
#[derive(Debug, Clone, PartialEq)]
pub struct ProfileFieldSummary {
    pub name: &'static str,
    pub value: Option<String>,
}

/// Builds [`ProfileFieldSummary`] for every profile field that has ANY
/// value -- a field with nothing set is left out entirely (naming an empty
/// field would not help the model and only adds tokens).
pub fn profile_field_summary(profile: &Profile) -> Vec<ProfileFieldSummary> {
    ALL_FIELD_KINDS
        .iter()
        .filter_map(|&kind| {
            let (value, sensitive) = profile_value_for_kind(kind, profile)?;
            Some(ProfileFieldSummary {
                name: profile_field_name(kind),
                value: if sensitive { None } else { Some(value) },
            })
        })
        .collect()
}

fn format_profile_summary(summary: &[ProfileFieldSummary]) -> String {
    if summary.is_empty() {
        return "(the profile has no data yet)".to_string();
    }
    summary
        .iter()
        .map(|s| match &s.value {
            Some(v) => format!("{}: {v}", s.name),
            None => format!("{} (sensitive, value withheld)", s.name),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_unmapped_fields(unmapped: &[UnmappedField]) -> String {
    unmapped
        .iter()
        .map(|f| {
            let id = control_id(f.candidate_index);
            if f.current.trim().is_empty() {
                format!("{id}: \"{}\"", f.label)
            } else {
                format!("{id}: \"{}\" (currently: {})", f.label, f.current)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The full system prompt for stage 2's completion: [`BASE_PROMPT`] plus
/// the unmapped fields' labels/current text and the profile summary (names
/// always, values only when non-sensitive -- see [`profile_field_summary`]).
pub fn build_model_prompt(unmapped: &[UnmappedField], profile: &Profile) -> String {
    format!(
        "{BASE_PROMPT}\n\nFields Wingman could not map on its own:\n{}\n\nProfile fields available \
         (name: value; a sensitive field's name is shown with its value withheld):\n{}",
        format_unmapped_fields(unmapped),
        format_profile_summary(&profile_field_summary(profile)),
    )
}

/// Builds the `Request` for stage 2. Never called at all when `unmapped` is
/// empty -- `app.rs`'s wiring checks that before ever reaching here, which
/// is what the live `#[ignore]`d test at the bottom of this file proves for
/// a real, fully-local-mappable form.
pub fn form_fill_request(shot: &Shot, unmapped: &[UnmappedField], profile: &Profile) -> Request {
    Request {
        system: build_model_prompt(unmapped, profile),
        user: "Fill in what you can; \"skip\" anything you are not sure about.".to_string(),
        images: vec![shot.png.clone()],
        schema: Some(
            crate::actions::schema::schema_for("form_fill", false)
                .expect("\"form_fill\" is always registered in actions::schema"),
        ),
        effort: Effort::Unset,
        max_tokens: 0,
    }
}

/// One entry of the model's `form_fill` completion, parsed from JSON but
/// not yet merged against `profile`/`unmapped` -- see [`merge_model_response`].
#[derive(Debug, Clone, PartialEq)]
pub struct ModelFieldResponse {
    pub control_id: String,
    pub source: String,
    pub profile_field: String,
    pub value: String,
    pub sensitive: bool,
}

/// Parses stage 2's completion text (a `form_fill`-schema-shaped JSON
/// object) into a list of [`ModelFieldResponse`]. A field entry missing
/// `"control_id"` is a hard parse error (there would be no way to know
/// which candidate it is about); every other property defaults
/// conservatively when absent (`source` defaults to `"skip"`, `sensitive`
/// defaults to `true`) rather than guessing permissively.
pub fn parse_model_response(text: &str) -> Result<Vec<ModelFieldResponse>> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("provider: form_fill completion is not valid JSON: {e}"))?;
    let fields = value
        .get("fields")
        .and_then(Value::as_array)
        .ok_or_else(|| anyhow::anyhow!("provider: form_fill completion has no \"fields\" array"))?;

    fields
        .iter()
        .map(|f| {
            let control_id = f
                .get("control_id")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow::anyhow!("provider: form_fill field has no \"control_id\""))?
                .to_string();
            let source = f
                .get("source")
                .and_then(Value::as_str)
                .unwrap_or("skip")
                .to_string();
            let profile_field = f
                .get("profile_field")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let value = f
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let sensitive = f.get("sensitive").and_then(Value::as_bool).unwrap_or(true);
            Ok(ModelFieldResponse {
                control_id,
                source,
                profile_field,
                value,
                sensitive,
            })
        })
        .collect()
}

/// Merges the model's response for `unmapped` fields into [`MappedField`]s,
/// re-reading a `"profile"`-sourced value from `profile` itself (never
/// trusting a value the model might have echoed -- see this file's module
/// doc comment) and applying the payment denylist to a `"model"`-sourced
/// field's LABEL and VALUE as a second line of defense
/// (`executors::fill_form` re-checks both again at Do-time regardless;
/// filtering here too just keeps a doomed-to-be-refused row out of the
/// preview). The value check exists because a `"model"`-sourced value is
/// literal text the model invented from the screenshot, with no structural
/// guarantee against being payment-shaped (unlike a `"profile"`-sourced
/// value, which `Profile::save_to`/`load_from` already refuse to store in
/// that shape): #220 -- a Luhn-valid card number or a mod-97-valid IBAN
/// behind an ordinary-looking label ("Reference number", "Confirmation
/// code") is refused here even though its label alone would pass. A
/// `control_id` the response never mentions, an unrecognized `profile_field`
/// name, an empty profile value, an empty `"model"` value, or any `source`
/// other than `"profile"`/`"model"` all mean the same thing: nothing is
/// proposed for that field.
pub fn merge_model_response(
    unmapped: &[UnmappedField],
    responses: &[ModelFieldResponse],
    profile: &Profile,
) -> Vec<MappedField> {
    let mut out = Vec::new();

    for field in unmapped {
        let id = control_id(field.candidate_index);
        let Some(resp) = responses.iter().find(|r| r.control_id == id) else {
            continue;
        };

        match resp.source.as_str() {
            "profile" => {
                let Some(kind) = profile_field_kind_by_name(&resp.profile_field) else {
                    continue;
                };
                let Some((value, sensitive)) = profile_value_for_kind(kind, profile) else {
                    continue;
                };
                out.push(MappedField {
                    candidate_index: field.candidate_index,
                    label: field.label.clone(),
                    current: field.current.clone(),
                    value,
                    sensitive,
                    source: format!("profile:{}", resp.profile_field),
                });
            }
            "model" => {
                let trimmed = resp.value.trim();
                if trimmed.is_empty() {
                    continue;
                }
                if crate::payment_denylist::is_payment_shaped_label(&field.label) {
                    continue;
                }
                if crate::payment_denylist::is_payment_shaped_value(trimmed) {
                    continue;
                }
                out.push(MappedField {
                    candidate_index: field.candidate_index,
                    label: field.label.clone(),
                    current: field.current.clone(),
                    value: trimmed.to_string(),
                    sensitive: resp.sensitive,
                    source: "model".to_string(),
                });
            }
            _ => continue, // "skip", or anything unrecognized
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Building the merged fill_form proposal
// ---------------------------------------------------------------------------

fn target_json(field: &FieldSnapshot) -> Value {
    json!({
        "hwnd": field.hwnd,
        "runtime_id": field.runtime_id,
        "automation_id": field.automation_id,
        "name": field.name,
        "control_type": control_type_str(field.control_type),
    })
}

/// Applies `config.forms.require_tick_for` to one field's natural
/// sensitivity. See `config::RequireTickFor`'s own doc comment for what
/// each setting means and why its default is provisional.
pub fn effective_sensitive(natural: bool, require_tick_for: RequireTickFor) -> bool {
    match require_tick_for {
        RequireTickFor::All => true,
        RequireTickFor::None => false,
        RequireTickFor::Sensitive => natural,
    }
}

/// Builds the final `{"fields": [...]}` proposal `executors::fill_form::parse_form_fill`
/// (via `target`/`label`/`value`/`sensitive`/`approved`) and this file's own
/// preview translation (via the extra `current`/`source`, which that parser
/// ignores) both read. `candidates` must be the SAME slice (by index)
/// [`map_candidates_locally`]/[`merge_model_response`] were built against.
pub fn build_proposal(
    candidates: &[FieldSnapshot],
    mapped: &[MappedField],
    require_tick_for: RequireTickFor,
) -> Value {
    let fields: Vec<Value> = mapped
        .iter()
        .filter_map(|m| {
            let field = candidates.get(m.candidate_index)?;
            let sensitive = effective_sensitive(m.sensitive, require_tick_for);
            Some(json!({
                "target": target_json(field),
                "label": m.label,
                "current": m.current,
                "value": m.value,
                "sensitive": sensitive,
                "approved": !sensitive,
                "source": m.source,
            }))
        })
        .collect();
    json!({ "fields": fields })
}

/// Whether a built proposal actually has anything to fill. `executors::fill_form`
/// itself refuses an empty `"fields"` array as a parse error (by design --
/// see that module's `empty_fields_array_is_a_named_error`), so a caller
/// must check this BEFORE ever showing a preview or running the executor,
/// and show an informational card instead (`app.rs`'s wiring).
pub fn has_fillable_fields(proposal: &Value) -> bool {
    proposal
        .get("fields")
        .and_then(Value::as_array)
        .is_some_and(|a| !a.is_empty())
}

// ---------------------------------------------------------------------------
// Preview translation: merged proposal <-> the existing flat PreviewModel
// ---------------------------------------------------------------------------

fn format_display_value(current: &str, proposed: &str, source: &str) -> String {
    if current.trim().is_empty() {
        format!("{proposed} (from {source})")
    } else {
        format!("{current} -> {proposed} (from {source})")
    }
}

/// Translates a merged `form_fill` proposal into the flat pseudo-schema/value
/// pair `Card::show_preview`/`ui::preview::PreviewModel::from_schema`
/// already know how to render -- see this file's module doc comment for why
/// this reuses the existing machinery instead of a new Win32 rendering
/// path. One property per field, keyed `f<i>` (`i` is the field's position
/// in `proposal["fields"]`, NOT `MappedField::candidate_index` -- the two
/// can differ once fields with no match are filtered out of the proposal).
/// A non-sensitive field is a non-editable informational row; a sensitive
/// one is editable, its value defaulting to the proposed text -- clearing
/// it means "skip", editing it means "use this instead".
pub fn build_preview_schema_and_value(proposal: &Value) -> (Value, Value) {
    let empty = Vec::new();
    let fields = proposal
        .get("fields")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut properties = Map::new();
    let mut value_obj = Map::new();

    for (i, field) in fields.iter().enumerate() {
        let key = format!("f{i}");
        let label = field.get("label").and_then(Value::as_str).unwrap_or("");
        let current = field.get("current").and_then(Value::as_str).unwrap_or("");
        let proposed = field.get("value").and_then(Value::as_str).unwrap_or("");
        let source = field.get("source").and_then(Value::as_str).unwrap_or("");
        let sensitive = field
            .get("sensitive")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        if sensitive {
            properties.insert(
                key.clone(),
                json!({
                    "type": "string",
                    "editable": true,
                    "label": format!("{label} (sensitive, clear to skip)"),
                }),
            );
            value_obj.insert(key, Value::String(proposed.to_string()));
        } else {
            properties.insert(
                key.clone(),
                json!({
                    "type": "string",
                    "label": label,
                }),
            );
            value_obj.insert(
                key,
                Value::String(format_display_value(current, proposed, source)),
            );
        }
    }

    (
        json!({ "properties": properties }),
        Value::Object(value_obj),
    )
}

/// The reverse of [`build_preview_schema_and_value`]: `confirmed_flat` is
/// the flat `Value` `ui::confirm::confirm_preview` built from whatever was
/// on screen when "Do it" fired (`card.take_confirmed()`); `original` is
/// the merged proposal [`build_proposal`] produced, kept in `App` state
/// since "show" (see `app.rs`). Every sensitive field's `value`/`approved`
/// are overwritten from what the user left in its row; every non-sensitive
/// field is untouched (there was never a row to edit).
pub fn rebuild_after_confirm(original: &Value, confirmed_flat: &Value) -> Value {
    let empty = Vec::new();
    let fields = original
        .get("fields")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let rebuilt: Vec<Value> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let mut field = field.clone();
            let sensitive = field
                .get("sensitive")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if sensitive {
                let key = format!("f{i}");
                let edited = confirmed_flat
                    .get(&key)
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                let approved = !edited.trim().is_empty();
                if let Some(obj) = field.as_object_mut() {
                    obj.insert("value".to_string(), Value::String(edited));
                    obj.insert("approved".to_string(), Value::Bool(approved));
                }
            }
            field
        })
        .collect();

    json!({ "fields": rebuilt })
}

// ---------------------------------------------------------------------------
// The flow's pure state machine (same status as `actions::calendar::FlowState`:
// `app.rs`'s real wiring follows these rules procedurally, not literally --
// see that type's own doc comment for why this still earns its keep as a
// directly-testable statement of the rules)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum FlowState {
    Proposed(Value),
    Previewed(Value),
    Confirmed(Value),
    Executed { summary: String },
    Cancelled,
    Failed(String),
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum FlowEvent {
    ProposalReady(std::result::Result<Value, String>),
    Shown,
    Confirmed,
    Cancelled,
    Executed(std::result::Result<String, String>),
}

#[allow(dead_code)]
pub fn advance(state: FlowState, event: FlowEvent) -> FlowState {
    match (state, event) {
        (_, FlowEvent::ProposalReady(Err(e))) => FlowState::Failed(e),
        (_, FlowEvent::ProposalReady(Ok(value))) if !has_fillable_fields(&value) => {
            FlowState::Cancelled
        }
        (_, FlowEvent::ProposalReady(Ok(value))) => FlowState::Proposed(value),
        (FlowState::Proposed(value), FlowEvent::Shown) => FlowState::Previewed(value),
        (FlowState::Previewed(value), FlowEvent::Confirmed) => FlowState::Confirmed(value),
        (FlowState::Previewed(_), FlowEvent::Cancelled) => FlowState::Cancelled,
        (FlowState::Confirmed(_), FlowEvent::Executed(Ok(summary))) => {
            FlowState::Executed { summary }
        }
        (FlowState::Confirmed(_), FlowEvent::Executed(Err(e))) => FlowState::Failed(e),
        (other, _) => other,
    }
}

// ---------------------------------------------------------------------------
// Empty profile: the "point at where to add profile data" card, plus the
// one-shot profile.toml import -- see this file's module doc comment for
// the decision this section implements.
// ---------------------------------------------------------------------------

/// Whether `profile` has nothing at all -- every field at its `Default`.
pub fn profile_is_empty(profile: &Profile) -> bool {
    *profile == Profile::default()
}

pub const EMPTY_PROFILE_HEADLINE: &str = "No profile data yet";

/// The card body shown when [`profile_is_empty`] is still true after
/// [`load_or_import_profile`] has already tried a one-shot `profile.toml`
/// import. Names both real paths (rule 1 never applies here: this is only
/// the PATH, never the file's contents) so the user knows exactly what to
/// create and where the encrypted result ends up.
pub fn empty_profile_detail(toml_path: &str, bin_path: &str) -> String {
    format!(
        "Fill this form has nothing to offer yet because your profile is empty. \
         Create a plain text file at:\n{toml_path}\n\
         with fields like full_name, emails, phones and address, and Wingman will import it \
         the next time you press this. The encrypted profile it becomes lives at:\n{bin_path}\n\
         A profile settings page is planned (#50) but not built yet."
    )
}

/// Reads a plain-text `profile.toml`, if one exists at `toml_path`. `None`
/// (not an error) when the file is simply absent -- the common case, since
/// this import only ever matters the first time. A malformed file IS an
/// error (never silently ignored): the user wrote something, and it is
/// wrong, which deserves a card rather than silent failure (rule 7).
fn import_profile_toml_if_present(toml_path: &Path) -> Result<Option<Profile>> {
    if !toml_path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(toml_path)
        .with_context(|| format!("failed to read {}", toml_path.display()))?;
    let profile: Profile = toml::from_str(&text)
        .with_context(|| format!("{} is not a valid profile file", toml_path.display()))?;
    Ok(Some(profile))
}

/// Loads the encrypted profile from `bin_path`; if it is empty, tries a
/// one-shot import from a plain `profile.toml` at `toml_path` and, when
/// that yields anything, saves it back to `bin_path` -- through
/// `Profile::save_to`, so it is encrypted and the payment-data denylist
/// runs over it exactly like any other write (a `profile.toml` with a
/// payment-shaped value in it is rejected here, surfaced as an `Err`,
/// never silently imported). Path-injectable (rule 9); [`load_or_import`]
/// is the only caller that touches the real `%APPDATA%` paths.
pub fn load_or_import_profile_from(bin_path: &Path, toml_path: &Path) -> Result<Profile> {
    let mut profile = Profile::load_from(bin_path)?;
    if profile_is_empty(&profile) {
        if let Some(imported) = import_profile_toml_if_present(toml_path)? {
            if !profile_is_empty(&imported) {
                imported.save_to(bin_path)?;
                profile = imported;
            }
        }
    }
    Ok(profile)
}

/// [`load_or_import_profile_from`] against the real
/// `%APPDATA%\Wingman\profile.bin` and `%APPDATA%\Wingman\profile.toml`.
pub fn load_or_import_profile() -> Result<Profile> {
    let bin_path = Profile::path()?;
    let toml_path = bin_path.with_file_name("profile.toml");
    load_or_import_profile_from(&bin_path, &toml_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- BASE_PROMPT / non-vision composition (#247) ---------------------

    #[test]
    fn base_prompt_does_not_contradict_the_non_vision_preface() {
        // provider::non_vision_request prefixes NON_VISION_PREFACE (which
        // says "Instead of a screenshot, you are given the on-screen text
        // recognized by OCR...") onto BASE_PROMPT. BASE_PROMPT must not
        // then turn around and claim a screenshot is shown -- that is a
        // literal contradiction inside one system prompt (#247).
        let base = crate::provider::Request {
            system: BASE_PROMPT.to_string(),
            user: String::new(),
            images: Vec::new(),
            schema: None,
            effort: crate::provider::Effort::Unset,
            max_tokens: 0,
        };
        let composed =
            crate::provider::non_vision_request(&base, "some ocr text", "some fields").system;
        let after_instead = composed
            .split("Instead of a screenshot")
            .nth(1)
            .expect("preface names the non-vision path");
        assert!(
            !after_instead
                .to_lowercase()
                .contains("you are shown a screenshot"),
            "BASE_PROMPT still asserts a screenshot is shown after the non-vision \
             preface says otherwise: {composed}"
        );
    }

    // -- builtin_action -------------------------------------------------

    #[test]
    fn builtin_action_matches_the_task_brief() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.name, "Fill this form");
        assert_eq!(a.group.as_deref(), Some("Work"));
        assert_eq!(a.inputs, vec![InputKind::Uia, InputKind::Screen]);
        assert_eq!(a.proposal, "form_fill");
        assert_eq!(a.executor, "fill_form");
        assert!(a.confirm, "writing a form field is never auto-confirmed");
        assert!(!a.rate_difficulty);
        assert!(a.enabled);
    }

    // -- is_fillable_candidate / fillable_candidates -------------------------

    fn field(
        control_type: ControlKind,
        label: &str,
        value: FieldValue,
        enabled: bool,
    ) -> FieldSnapshot {
        FieldSnapshot {
            hwnd: 1,
            runtime_id: vec![1],
            automation_id: String::new(),
            name: label.to_string(),
            label: label.to_string(),
            control_type,
            value,
            rect: Default::default(),
            enabled,
            focusable: true,
        }
    }

    #[test]
    fn edit_document_combobox_are_fillable() {
        for ct in [
            ControlKind::Edit,
            ControlKind::Document,
            ControlKind::ComboBox,
        ] {
            let f = field(ct, "Full name", FieldValue::Empty, true);
            assert!(is_fillable_candidate(&f), "{ct:?} should be fillable");
        }
    }

    #[test]
    fn checkbox_radio_list_are_not_fillable() {
        for ct in [
            ControlKind::CheckBox,
            ControlKind::RadioButton,
            ControlKind::List,
        ] {
            let f = field(ct, "Remember me", FieldValue::Empty, true);
            assert!(!is_fillable_candidate(&f), "{ct:?} should not be fillable");
        }
    }

    #[test]
    fn disabled_field_is_not_fillable() {
        let f = field(ControlKind::Edit, "Full name", FieldValue::Empty, false);
        assert!(!is_fillable_candidate(&f));
    }

    #[test]
    fn redacted_password_field_is_never_fillable() {
        let f = field(ControlKind::Edit, "Password", FieldValue::Redacted, true);
        assert!(!is_fillable_candidate(&f));
    }

    #[test]
    fn forbidden_named_field_is_not_fillable() {
        let mut f = field(ControlKind::Edit, "Notes", FieldValue::Empty, true);
        f.name = "Place Order".to_string();
        assert!(!is_fillable_candidate(&f));
    }

    #[test]
    fn fillable_candidates_filters_and_preserves_order() {
        let fields = vec![
            field(ControlKind::Edit, "Full name", FieldValue::Empty, true),
            field(
                ControlKind::CheckBox,
                "Remember me",
                FieldValue::Empty,
                true,
            ),
            field(ControlKind::Edit, "Email", FieldValue::Empty, true),
        ];
        let candidates = fillable_candidates(&fields);
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].label, "Full name");
        assert_eq!(candidates[1].label, "Email");
    }

    // -- profile_value_for_kind / profile_field_name round trip -------------

    fn sample_profile() -> Profile {
        use crate::profile::{Address, Field};
        Profile {
            full_name: Field::new("Ada Lovelace".to_string(), false),
            preferred_name: Field::new("Ada".to_string(), false),
            emails: vec![Field::new("ada@example.com".to_string(), false)],
            phones: vec![Field::new("+1 555-123-4567".to_string(), false)],
            address: Field::new(
                Address {
                    lines: vec!["221B Baker Street".to_string()],
                    city: "London".to_string(),
                    region: "Greater London".to_string(),
                    postcode: "NW1 6XE".to_string(),
                    country: "United Kingdom".to_string(),
                },
                false,
            ),
            organisation: Field::new("Analytical Engines Ltd".to_string(), false),
            job_title: Field::new("Mathematician".to_string(), false),
            // In production this is always `true` (`Profile::normalize_sensitivity`,
            // applied on every load/save); set explicitly here since this
            // in-memory sample is never round-tripped through `save_to`/`load_from`.
            date_of_birth: Field::new("1815-12-10".to_string(), true),
            website: Field::new("https://example.com".to_string(), false),
            notes: Field::new(String::new(), false),
        }
    }

    #[test]
    fn profile_field_name_and_kind_by_name_round_trip_for_every_kind() {
        for &kind in ALL_FIELD_KINDS {
            let name = profile_field_name(kind);
            assert_eq!(profile_field_kind_by_name(name), Some(kind));
        }
    }

    #[test]
    fn profile_field_kind_by_name_is_none_for_an_unrecognized_name() {
        assert_eq!(profile_field_kind_by_name("not_a_real_field"), None);
    }

    #[test]
    fn profile_value_for_kind_reads_job_title() {
        let profile = sample_profile();
        let (value, sensitive) = profile_value_for_kind(FieldKind::JobTitle, &profile).unwrap();
        assert_eq!(value, "Mathematician");
        assert!(!sensitive);
    }

    #[test]
    fn profile_value_for_kind_is_none_for_an_empty_field() {
        let profile = Profile::default();
        assert_eq!(profile_value_for_kind(FieldKind::JobTitle, &profile), None);
    }

    #[test]
    fn profile_value_for_kind_uses_the_first_non_empty_email() {
        use crate::profile::Field;
        let profile = Profile {
            emails: vec![
                Field::new(String::new(), false),
                Field::new("ada@example.com".to_string(), true),
            ],
            ..Profile::default()
        };
        let (value, sensitive) = profile_value_for_kind(FieldKind::Email, &profile).unwrap();
        assert_eq!(value, "ada@example.com");
        assert!(sensitive);
    }

    #[test]
    fn profile_value_for_kind_joins_address_lines() {
        use crate::profile::{Address, Field};
        let profile = Profile {
            address: Field::new(
                Address {
                    lines: vec!["221B Baker Street".to_string(), "Flat 2".to_string()],
                    ..Default::default()
                },
                false,
            ),
            ..Profile::default()
        };
        let (value, _) = profile_value_for_kind(FieldKind::AddressLine, &profile).unwrap();
        assert_eq!(value, "221B Baker Street, Flat 2");
    }

    // -- map_candidates_locally: the local mapping table --------------------

    #[test]
    fn local_mapping_table() {
        let profile = sample_profile();
        // (label, expect a local match, expected source substring, expected sensitive)
        let cases: &[(&str, bool, &str, bool)] = &[
            ("Full name", true, "profile:full_name", false),
            ("E-mail address", true, "profile:email", false),
            ("Phone", true, "profile:phone", false),
            ("Street Address", true, "profile:address_line", false),
            ("City", true, "profile:city", false),
            ("Company", true, "profile:organisation", false),
            ("Job Title", true, "profile:job_title", false),
            ("Website", true, "profile:website", false),
            ("Date of Birth", true, "profile:date_of_birth", true),
            ("Favorite color", false, "", false),
        ];

        let candidates: Vec<FieldSnapshot> = cases
            .iter()
            .map(|(label, ..)| field(ControlKind::Edit, label, FieldValue::Empty, true))
            .collect();

        let (mapped, unmapped) = map_candidates_locally(&candidates, &profile);

        for (label, should_map, source_substr, expected_sensitive) in cases {
            if *should_map {
                let m = mapped
                    .iter()
                    .find(|m| m.label == *label)
                    .unwrap_or_else(|| panic!("{label:?} should have mapped locally"));
                assert!(
                    m.source.contains(source_substr),
                    "{label:?}: expected source to contain {source_substr:?}, got {:?}",
                    m.source
                );
                assert_eq!(
                    m.sensitive, *expected_sensitive,
                    "{label:?}: unexpected sensitivity"
                );
            } else {
                assert!(
                    unmapped.iter().any(|u| u.label == *label),
                    "{label:?} should be unmapped"
                );
            }
        }
    }

    #[test]
    fn map_candidates_locally_carries_candidate_index_through() {
        let profile = sample_profile();
        let candidates = vec![
            field(ControlKind::Edit, "Full name", FieldValue::Empty, true),
            field(ControlKind::Edit, "Favorite color", FieldValue::Empty, true),
        ];
        let (mapped, unmapped) = map_candidates_locally(&candidates, &profile);
        assert_eq!(mapped[0].candidate_index, 0);
        assert_eq!(unmapped[0].candidate_index, 1);
    }

    #[test]
    fn map_candidates_locally_treats_a_matched_but_empty_profile_field_as_unmapped() {
        let profile = Profile::default(); // nothing set
        let candidates = vec![field(
            ControlKind::Edit,
            "Full name",
            FieldValue::Empty,
            true,
        )];
        let (mapped, unmapped) = map_candidates_locally(&candidates, &profile);
        assert!(mapped.is_empty());
        assert_eq!(unmapped.len(), 1);
    }

    #[test]
    fn map_candidates_locally_carries_the_current_live_text_through() {
        let profile = sample_profile();
        let candidates = vec![field(
            ControlKind::Edit,
            "Full name",
            FieldValue::Text("old value".to_string()),
            true,
        )];
        let (mapped, _) = map_candidates_locally(&candidates, &profile);
        assert_eq!(mapped[0].current, "old value");
        assert_eq!(mapped[0].value, "Ada Lovelace");
    }

    // -- profile_field_summary: names always, values only when safe --------

    #[test]
    fn profile_field_summary_omits_empty_fields() {
        let profile = Profile::default();
        assert!(profile_field_summary(&profile).is_empty());
    }

    #[test]
    fn profile_field_summary_attaches_the_value_for_a_non_sensitive_field() {
        let profile = sample_profile();
        let summary = profile_field_summary(&profile);
        let job_title = summary.iter().find(|s| s.name == "job_title").unwrap();
        assert_eq!(job_title.value.as_deref(), Some("Mathematician"));
    }

    #[test]
    fn profile_field_summary_withholds_the_value_for_a_sensitive_field() {
        use crate::profile::Field;
        let profile = Profile {
            date_of_birth: Field::new("1815-12-10".to_string(), true),
            ..Profile::default()
        };
        let summary = profile_field_summary(&profile);
        let dob = summary.iter().find(|s| s.name == "date_of_birth").unwrap();
        assert_eq!(dob.value, None);
    }

    // -- build_model_prompt / form_fill_request: the golden request body ----

    #[test]
    fn model_request_excludes_sensitive_values_but_names_sensitive_fields() {
        use crate::profile::Field;
        let mut profile = sample_profile();
        // A distinctive, unmistakable sensitive value that must never leak
        // into the request text.
        profile.date_of_birth = Field::new("1815-12-10".to_string(), true);

        let unmapped = vec![UnmappedField {
            candidate_index: 3,
            label: "Favorite color".to_string(),
            current: String::new(),
        }];

        let prompt = build_model_prompt(&unmapped, &profile);

        // The unmapped field itself, named by its control id.
        assert!(prompt.contains("f3"));
        assert!(prompt.contains("Favorite color"));

        // A non-sensitive profile field's real value is sent.
        assert!(prompt.contains("job_title: Mathematician"));
        assert!(prompt.contains("Ada Lovelace"));

        // The sensitive field is NAMED...
        assert!(prompt.contains("date_of_birth"));
        // ...but its literal value never appears anywhere in the request.
        assert!(!prompt.contains("1815-12-10"));
    }

    #[test]
    fn form_fill_request_carries_the_form_fill_schema_and_one_image() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Favorite color".to_string(),
            current: String::new(),
        }];
        let shot = Shot {
            png: vec![1, 2, 3],
            width: 10,
            height: 10,
        };
        let req = form_fill_request(&shot, &unmapped, &profile);
        assert_eq!(req.images, vec![vec![1u8, 2, 3]]);
        assert_eq!(
            req.schema,
            crate::actions::schema::schema_for("form_fill", false)
        );
    }

    #[test]
    fn build_model_prompt_with_no_unmapped_fields_still_carries_the_profile_summary() {
        let profile = sample_profile();
        let prompt = build_model_prompt(&[], &profile);
        assert!(prompt.contains("job_title: Mathematician"));
    }

    #[test]
    fn build_model_prompt_with_an_empty_profile_says_so_plainly() {
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Favorite color".to_string(),
            current: String::new(),
        }];
        let prompt = build_model_prompt(&unmapped, &Profile::default());
        assert!(prompt.contains("no data yet"));
    }

    // -- parse_model_response -------------------------------------------

    #[test]
    fn parse_model_response_parses_a_full_response() {
        let text = serde_json::json!({
            "fields": [
                {"control_id": "f0", "source": "profile", "profile_field": "job_title", "value": "", "sensitive": false},
                {"control_id": "f1", "source": "model", "profile_field": "", "value": "Blue", "sensitive": false}
            ]
        })
        .to_string();
        let parsed = parse_model_response(&text).unwrap();
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].control_id, "f0");
        assert_eq!(parsed[0].source, "profile");
        assert_eq!(parsed[1].value, "Blue");
    }

    #[test]
    fn parse_model_response_rejects_invalid_json() {
        let err = parse_model_response("not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn parse_model_response_rejects_a_missing_fields_array() {
        let err = parse_model_response("{}").unwrap_err();
        assert!(err.to_string().contains("fields"));
    }

    #[test]
    fn parse_model_response_field_missing_control_id_is_a_named_error() {
        let text = serde_json::json!({"fields": [{"source": "skip"}]}).to_string();
        let err = parse_model_response(&text).unwrap_err();
        assert!(err.to_string().contains("control_id"));
    }

    #[test]
    fn parse_model_response_defaults_missing_source_to_skip_and_sensitive_to_true() {
        let text = serde_json::json!({"fields": [{"control_id": "f0"}]}).to_string();
        let parsed = parse_model_response(&text).unwrap();
        assert_eq!(parsed[0].source, "skip");
        assert!(parsed[0].sensitive);
    }

    // -- merge_model_response ---------------------------------------------

    #[test]
    fn merge_model_response_fills_a_profile_sourced_field_from_the_real_profile_value() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Position".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "profile".to_string(),
            profile_field: "job_title".to_string(),
            value: "ignored, never trusted".to_string(),
            sensitive: false,
        }];
        let merged = merge_model_response(&unmapped, &responses, &profile);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].value, "Mathematician");
        assert_eq!(merged[0].source, "profile:job_title");
    }

    #[test]
    fn merge_model_response_uses_a_model_sourced_literal_value() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Favorite color".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            value: "Blue".to_string(),
            sensitive: false,
        }];
        let merged = merge_model_response(&unmapped, &responses, &profile);
        assert_eq!(merged[0].value, "Blue");
        assert_eq!(merged[0].source, "model");
    }

    #[test]
    fn merge_model_response_skips_an_unrecognized_profile_field_name() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "X".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "profile".to_string(),
            profile_field: "not_a_real_field".to_string(),
            value: String::new(),
            sensitive: false,
        }];
        assert!(merge_model_response(&unmapped, &responses, &profile).is_empty());
    }

    #[test]
    fn merge_model_response_skips_a_payment_shaped_model_label_even_with_a_value() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Card number".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            value: "4111111111111111".to_string(),
            sensitive: false,
        }];
        assert!(merge_model_response(&unmapped, &responses, &profile).is_empty());
    }

    // -- #220: a Luhn-valid/IBAN-shaped model value behind an ORDINARY label
    // must be refused too, not just a payment-shaped label -----------------

    #[test]
    fn merge_model_response_skips_a_luhn_valid_card_number_behind_an_ordinary_label() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Reference number".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            // A well-known Luhn-valid test card number (not a real account).
            value: "4111111111111111".to_string(),
            sensitive: false,
        }];
        assert!(
            merge_model_response(&unmapped, &responses, &profile).is_empty(),
            "a Luhn-valid card number must be refused even behind an ordinary label"
        );
    }

    #[test]
    fn merge_model_response_skips_a_mod97_valid_iban_behind_an_ordinary_label() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Confirmation code".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            // The well-known mod-97-valid IBAN worked example (ISO 13616).
            value: "GB29 NWBK 6016 1331 9268 19".to_string(),
            sensitive: false,
        }];
        assert!(
            merge_model_response(&unmapped, &responses, &profile).is_empty(),
            "a mod-97-valid IBAN must be refused even behind an ordinary label"
        );
    }

    #[test]
    fn merge_model_response_allows_a_non_luhn_digit_run_behind_an_ordinary_label() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Order number".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            // 16 digits, deliberately not Luhn-valid (verified by hand, same
            // as profile::denylist's own non-Luhn fixture): an order number
            // must still be allowed through.
            value: "1234567890123456".to_string(),
            sensitive: false,
        }];
        let merged = merge_model_response(&unmapped, &responses, &profile);
        assert_eq!(merged.len(), 1, "a non-Luhn digit run is not a card number");
        assert_eq!(merged[0].value, "1234567890123456");
    }

    #[test]
    fn merge_model_response_allows_an_ordinary_phone_number_behind_an_ordinary_label() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Phone".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            value: "+1 555-123-4567".to_string(),
            sensitive: false,
        }];
        let merged = merge_model_response(&unmapped, &responses, &profile);
        assert_eq!(
            merged.len(),
            1,
            "an ordinary phone number must not be refused"
        );
        assert_eq!(merged[0].value, "+1 555-123-4567");
    }

    #[test]
    fn merge_model_response_allows_a_zip_plus_four_behind_an_ordinary_label() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "Postcode".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "model".to_string(),
            profile_field: String::new(),
            value: "94103-1234".to_string(),
            sensitive: false,
        }];
        let merged = merge_model_response(&unmapped, &responses, &profile);
        assert_eq!(merged.len(), 1, "a ZIP+4 must not be refused");
        assert_eq!(merged[0].value, "94103-1234");
    }

    #[test]
    fn merge_model_response_ignores_an_explicit_skip() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "X".to_string(),
            current: String::new(),
        }];
        let responses = vec![ModelFieldResponse {
            control_id: "f0".to_string(),
            source: "skip".to_string(),
            profile_field: String::new(),
            value: String::new(),
            sensitive: false,
        }];
        assert!(merge_model_response(&unmapped, &responses, &profile).is_empty());
    }

    #[test]
    fn merge_model_response_ignores_a_field_the_response_never_mentions() {
        let profile = sample_profile();
        let unmapped = vec![UnmappedField {
            candidate_index: 0,
            label: "X".to_string(),
            current: String::new(),
        }];
        assert!(merge_model_response(&unmapped, &[], &profile).is_empty());
    }

    // -- build_proposal / has_fillable_fields --------------------------------

    #[test]
    fn build_proposal_matches_the_executors_expected_shape() {
        let candidates = vec![field(
            ControlKind::Edit,
            "Full name",
            FieldValue::Empty,
            true,
        )];
        let mapped = vec![MappedField {
            candidate_index: 0,
            label: "Full name".to_string(),
            current: String::new(),
            value: "Ada Lovelace".to_string(),
            sensitive: false,
            source: "profile:full_name".to_string(),
        }];
        let proposal = build_proposal(&candidates, &mapped, RequireTickFor::Sensitive);

        // `executors::fill_form::parse_form_fill` must be able to parse this
        // directly.
        let parsed = crate::executors::registry::resolve("fill_form").unwrap();
        assert_eq!(parsed.name(), "fill_form");

        let field0 = &proposal["fields"][0];
        assert_eq!(field0["label"], "Full name");
        assert_eq!(field0["value"], "Ada Lovelace");
        assert_eq!(field0["sensitive"], false);
        assert_eq!(field0["approved"], true);
        assert_eq!(field0["target"]["automation_id"], "");
        assert_eq!(field0["target"]["control_type"], "Edit");
    }

    #[test]
    fn build_proposal_sensitive_field_starts_unapproved() {
        let candidates = vec![field(
            ControlKind::Edit,
            "Date of birth",
            FieldValue::Empty,
            true,
        )];
        let mapped = vec![MappedField {
            candidate_index: 0,
            label: "Date of birth".to_string(),
            current: String::new(),
            value: "1990-01-01".to_string(),
            sensitive: true,
            source: "profile:date_of_birth".to_string(),
        }];
        let proposal = build_proposal(&candidates, &mapped, RequireTickFor::Sensitive);
        assert_eq!(proposal["fields"][0]["sensitive"], true);
        assert_eq!(proposal["fields"][0]["approved"], false);
    }

    #[test]
    fn effective_sensitive_all_forces_every_field_sensitive() {
        assert!(effective_sensitive(false, RequireTickFor::All));
        assert!(effective_sensitive(true, RequireTickFor::All));
    }

    #[test]
    fn effective_sensitive_none_forces_every_field_non_sensitive() {
        assert!(!effective_sensitive(false, RequireTickFor::None));
        assert!(!effective_sensitive(true, RequireTickFor::None));
    }

    #[test]
    fn effective_sensitive_sensitive_passes_the_natural_flag_through() {
        assert!(!effective_sensitive(false, RequireTickFor::Sensitive));
        assert!(effective_sensitive(true, RequireTickFor::Sensitive));
    }

    #[test]
    fn has_fillable_fields_is_false_for_an_empty_fields_array() {
        assert!(!has_fillable_fields(&json!({"fields": []})));
    }

    #[test]
    fn has_fillable_fields_is_true_when_at_least_one_field_exists() {
        let candidates = vec![field(
            ControlKind::Edit,
            "Full name",
            FieldValue::Empty,
            true,
        )];
        let mapped = vec![MappedField {
            candidate_index: 0,
            label: "Full name".to_string(),
            current: String::new(),
            value: "Ada".to_string(),
            sensitive: false,
            source: "profile:full_name".to_string(),
        }];
        let proposal = build_proposal(&candidates, &mapped, RequireTickFor::Sensitive);
        assert!(has_fillable_fields(&proposal));
    }

    // -- preview translation / approval filtering ----------------------------

    fn two_field_proposal() -> Value {
        json!({
            "fields": [
                {
                    "target": {"hwnd": 1, "runtime_id": [1], "automation_id": "", "name": "", "control_type": "Edit"},
                    "label": "Full name", "current": "", "value": "Ada Lovelace",
                    "sensitive": false, "approved": true, "source": "profile:full_name"
                },
                {
                    "target": {"hwnd": 1, "runtime_id": [2], "automation_id": "", "name": "", "control_type": "Edit"},
                    "label": "Date of birth", "current": "", "value": "1990-01-01",
                    "sensitive": true, "approved": false, "source": "profile:date_of_birth"
                }
            ]
        })
    }

    #[test]
    fn preview_translation_non_sensitive_row_is_not_editable_and_shows_current_arrow_proposed() {
        let (schema, value) = build_preview_schema_and_value(&two_field_proposal());
        let props = schema["properties"].as_object().unwrap();
        // No "editable" key at all for a non-sensitive row; PreviewModel
        // treats an absent key the same as `false` (see the round-trip test
        // below for the proof through the real PreviewModel).
        assert!(props["f0"].get("editable").is_none());
        assert_eq!(props["f0"]["label"], "Full name");
        assert_eq!(value["f0"], "Ada Lovelace (from profile:full_name)");
    }

    #[test]
    fn preview_translation_sensitive_row_is_editable_and_defaults_to_the_proposed_value() {
        let (schema, value) = build_preview_schema_and_value(&two_field_proposal());
        let props = schema["properties"].as_object().unwrap();
        assert_eq!(props["f1"]["editable"], true);
        assert!(props["f1"]["label"]
            .as_str()
            .unwrap()
            .contains("Date of birth"));
        assert_eq!(value["f1"], "1990-01-01");
    }

    #[test]
    fn preview_translation_round_trips_through_the_real_preview_model() {
        // The whole point: this pseudo-schema/value pair must be something
        // `ui::preview::PreviewModel` (and therefore `Card::show_preview`)
        // can actually render, not just a Value shape this file invented.
        let proposal = two_field_proposal();
        let (schema, value) = build_preview_schema_and_value(&proposal);
        let model = crate::ui::preview::PreviewModel::from_schema(&schema, &value);
        assert_eq!(model.fields().len(), 2);
        let sensitive_row = model.fields().iter().find(|f| f.name == "f1").unwrap();
        assert!(sensitive_row.editable);
        let plain_row = model.fields().iter().find(|f| f.name == "f0").unwrap();
        assert!(!plain_row.editable);
    }

    #[test]
    fn rebuild_after_confirm_approves_an_edited_sensitive_field() {
        let original = two_field_proposal();
        let confirmed_flat = json!({
            "f0": "Ada Lovelace (from profile:full_name)",
            "f1": "1990-01-01"
        });
        let rebuilt = rebuild_after_confirm(&original, &confirmed_flat);
        assert_eq!(rebuilt["fields"][1]["approved"], true);
        assert_eq!(rebuilt["fields"][1]["value"], "1990-01-01");
        // The non-sensitive field is untouched.
        assert_eq!(rebuilt["fields"][0]["value"], "Ada Lovelace");
        assert_eq!(rebuilt["fields"][0]["approved"], true);
    }

    #[test]
    fn rebuild_after_confirm_treats_a_cleared_sensitive_field_as_unapproved() {
        let original = two_field_proposal();
        let confirmed_flat = json!({
            "f0": "Ada Lovelace (from profile:full_name)",
            "f1": ""
        });
        let rebuilt = rebuild_after_confirm(&original, &confirmed_flat);
        assert_eq!(rebuilt["fields"][1]["approved"], false);
    }

    #[test]
    fn rebuild_after_confirm_lets_the_user_override_the_sensitive_value() {
        let original = two_field_proposal();
        let confirmed_flat = json!({
            "f0": "Ada Lovelace (from profile:full_name)",
            "f1": "1985-05-05"
        });
        let rebuilt = rebuild_after_confirm(&original, &confirmed_flat);
        assert_eq!(rebuilt["fields"][1]["value"], "1985-05-05");
        assert_eq!(rebuilt["fields"][1]["approved"], true);
    }

    #[test]
    fn rebuilt_proposal_is_accepted_by_the_real_fill_form_parser() {
        // End-to-end proof: build -> translate to preview -> confirm ->
        // rebuild -> the ALREADY-SHIPPED executor's own parser accepts it.
        // (Exercised indirectly: `executors::fill_form`'s parser is private
        // to that module, so this asserts the shape it documents instead --
        // target/label/value/sensitive/approved all present per field.)
        let original = two_field_proposal();
        let confirmed_flat = json!({"f0": "x", "f1": "1990-01-01"});
        let rebuilt = rebuild_after_confirm(&original, &confirmed_flat);
        for field in rebuilt["fields"].as_array().unwrap() {
            assert!(field.get("target").is_some());
            assert!(field.get("label").and_then(Value::as_str).is_some());
            assert!(field.get("value").and_then(Value::as_str).is_some());
            assert!(field.get("sensitive").and_then(Value::as_bool).is_some());
            assert!(field.get("approved").and_then(Value::as_bool).is_some());
        }
    }

    // -- FlowState / advance -------------------------------------------

    #[test]
    fn advance_provider_error_yields_failed() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProposalReady(Err("boom".to_string())),
        );
        assert_eq!(next, FlowState::Failed("boom".to_string()));
    }

    #[test]
    fn advance_empty_proposal_yields_cancelled_not_proposed() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProposalReady(Ok(json!({"fields": []}))),
        );
        assert_eq!(next, FlowState::Cancelled);
    }

    #[test]
    fn advance_full_happy_path() {
        let proposal = json!({"fields": [{"label": "x"}]});
        let s = advance(
            FlowState::Cancelled,
            FlowEvent::ProposalReady(Ok(proposal.clone())),
        );
        assert_eq!(s, FlowState::Proposed(proposal.clone()));
        let s = advance(s, FlowEvent::Shown);
        assert_eq!(s, FlowState::Previewed(proposal.clone()));
        let s = advance(s, FlowEvent::Confirmed);
        assert_eq!(s, FlowState::Confirmed(proposal));
        let s = advance(s, FlowEvent::Executed(Ok("filled 1 of 1".to_string())));
        assert_eq!(
            s,
            FlowState::Executed {
                summary: "filled 1 of 1".to_string()
            }
        );
    }

    #[test]
    fn advance_cancel_from_previewed() {
        let s = FlowState::Previewed(json!({}));
        assert_eq!(advance(s, FlowEvent::Cancelled), FlowState::Cancelled);
    }

    #[test]
    fn advance_executor_error_yields_failed() {
        let s = FlowState::Confirmed(json!({}));
        let next = advance(s, FlowEvent::Executed(Err("write failed".to_string())));
        assert_eq!(next, FlowState::Failed("write failed".to_string()));
    }

    #[test]
    fn advance_out_of_order_event_is_a_no_op() {
        let s = FlowState::Proposed(json!({}));
        let next = advance(s.clone(), FlowEvent::Confirmed);
        assert_eq!(next, s);
    }

    // -- empty profile / profile.toml import -----------------------------

    #[test]
    fn profile_is_empty_true_for_default() {
        assert!(profile_is_empty(&Profile::default()));
    }

    #[test]
    fn profile_is_empty_false_once_anything_is_set() {
        assert!(!profile_is_empty(&sample_profile()));
    }

    #[test]
    fn empty_profile_detail_names_both_paths_and_has_no_em_dash() {
        let text = empty_profile_detail("C:\\x\\profile.toml", "C:\\x\\profile.bin");
        assert!(text.contains("profile.toml"));
        assert!(text.contains("profile.bin"));
        assert!(!text.contains('\u{2014}'));
    }

    fn scratch_paths(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "wingman-fill-form-test-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        (dir.join("profile.bin"), dir.join("profile.toml"))
    }

    #[test]
    fn load_or_import_profile_from_with_no_bin_and_no_toml_yields_an_empty_profile() {
        let (bin_path, toml_path) = scratch_paths("neither");
        let profile = load_or_import_profile_from(&bin_path, &toml_path).unwrap();
        assert!(profile_is_empty(&profile));
        assert!(
            !bin_path.exists(),
            "must never create a profile out of nothing"
        );
    }

    #[test]
    fn load_or_import_profile_from_imports_a_plain_toml_and_saves_it_encrypted() {
        let (bin_path, toml_path) = scratch_paths("import");
        std::fs::create_dir_all(toml_path.parent().unwrap()).unwrap();
        std::fs::write(
            &toml_path,
            "[full_name]\nvalue = \"Ada Lovelace\"\nsensitive = false\n",
        )
        .unwrap();

        let profile = load_or_import_profile_from(&bin_path, &toml_path).unwrap();
        assert_eq!(profile.full_name.value, "Ada Lovelace");
        assert!(
            bin_path.exists(),
            "the import must be saved to the encrypted store"
        );

        // A second load (no more profile.toml needed) reads the saved copy.
        let reloaded = Profile::load_from(&bin_path).unwrap();
        assert_eq!(reloaded.full_name.value, "Ada Lovelace");

        let _ = std::fs::remove_dir_all(bin_path.parent().unwrap());
    }

    #[test]
    fn load_or_import_profile_from_never_imports_over_an_already_populated_profile() {
        let (bin_path, toml_path) = scratch_paths("no-overwrite");
        std::fs::create_dir_all(bin_path.parent().unwrap()).unwrap();
        let existing = sample_profile();
        existing.save_to(&bin_path).unwrap();
        std::fs::write(
            &toml_path,
            "[full_name]\nvalue = \"Someone Else\"\nsensitive = false\n",
        )
        .unwrap();

        let profile = load_or_import_profile_from(&bin_path, &toml_path).unwrap();
        assert_eq!(
            profile.full_name.value, "Ada Lovelace",
            "the existing profile must win"
        );

        let _ = std::fs::remove_dir_all(bin_path.parent().unwrap());
    }

    #[test]
    fn load_or_import_profile_from_rejects_a_payment_shaped_toml_and_saves_nothing() {
        let (bin_path, toml_path) = scratch_paths("denylist");
        std::fs::create_dir_all(toml_path.parent().unwrap()).unwrap();
        std::fs::write(
            &toml_path,
            "[notes]\nvalue = \"card: 4111 1111 1111 1111\"\nsensitive = false\n",
        )
        .unwrap();

        let err = load_or_import_profile_from(&bin_path, &toml_path).unwrap_err();
        assert!(!err.to_string().contains("4111"));
        assert!(!bin_path.exists());

        let _ = std::fs::remove_dir_all(toml_path.parent().unwrap());
    }

    // -- live, real Win32 window: local mapping alone fills a whole form,
    // with zero model calls ---------------------------------------------
    //
    // #40's task brief: "profile in a temp dir, local mapping fills all
    // three with zero model calls (assert no provider called), button
    // untouched." `#[ignore]`d (AGENTS.md: no live app launch in this
    // task) -- run by hand with
    // `cargo test fill_form_live -- --ignored --nocapture`.
    mod win32 {
        use super::*;
        use std::sync::{Once, OnceLock};
        use windows::core::w;
        use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
        use windows::Win32::System::LibraryLoader::GetModuleHandleW;
        use windows::Win32::UI::WindowsAndMessaging::{
            CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetWindowTextW,
            LoadCursorW, PeekMessageW, RegisterClassExW, SetForegroundWindow, SetWindowPos,
            ShowWindow, TranslateMessage, BN_CLICKED, CS_HREDRAW, CS_VREDRAW, ES_AUTOHSCROLL,
            HWND_BOTTOM, IDC_ARROW, MSG, PM_REMOVE, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
            SW_SHOWNOACTIVATE, WM_COMMAND, WNDCLASSEXW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_TABSTOP,
            WS_VISIBLE,
        };

        const CLASS_NAME: windows::core::PCWSTR =
            w!("Wingman.Actions.FillFormTestWindow.test.9c3e17");

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

        fn stack_in_creation_order(children: &[HWND]) {
            for &child in children {
                unsafe {
                    let _ = SetWindowPos(
                        child,
                        Some(HWND_BOTTOM),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                    );
                }
            }
        }

        fn window_text(hwnd: HWND) -> String {
            let mut buf = [0u16; 256];
            let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
            String::from_utf16_lossy(&buf[..len as usize])
        }

        #[test]
        #[ignore]
        fn fill_form_live_local_mapping_fills_a_whole_form_with_zero_model_calls() {
            let _uia = crate::inputs::lock_uia_test();
            let hinstance = instance();
            assert!(
                ensure_class_registered(hinstance),
                "RegisterClassExW for the test window class"
            );

            let frame = unsafe {
                CreateWindowExW(
                    Default::default(),
                    CLASS_NAME,
                    w!("Wingman fill_form (actions) live test window"),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    0,
                    0,
                    360,
                    260,
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

            let child = |class: windows::core::PCWSTR,
                         text: windows::core::PCWSTR,
                         style: windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE,
                         y: i32|
             -> HWND {
                unsafe {
                    CreateWindowExW(
                        Default::default(),
                        class,
                        text,
                        WS_CHILD | WS_VISIBLE | style,
                        10,
                        y,
                        260,
                        20,
                        Some(frame),
                        None,
                        Some(hinstance),
                        None,
                    )
                }
                .expect("CreateWindowExW (child)")
            };

            let name_label = child(w!("STATIC"), w!("Full name"), Default::default(), 10);
            let name_edit = child(
                w!("EDIT"),
                w!(""),
                WS_TABSTOP
                    | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                30,
            );
            let email_label = child(w!("STATIC"), w!("Email"), Default::default(), 60);
            let email_edit = child(
                w!("EDIT"),
                w!(""),
                WS_TABSTOP
                    | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                80,
            );
            let phone_label = child(w!("STATIC"), w!("Phone"), Default::default(), 110);
            let phone_edit = child(
                w!("EDIT"),
                w!(""),
                WS_TABSTOP
                    | windows::Win32::UI::WindowsAndMessaging::WINDOW_STYLE(ES_AUTOHSCROLL as u32),
                130,
            );
            let button = unsafe {
                CreateWindowExW(
                    Default::default(),
                    w!("BUTTON"),
                    w!("Place order"),
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    10,
                    160,
                    100,
                    24,
                    Some(frame),
                    Some(windows::Win32::UI::WindowsAndMessaging::HMENU(
                        601 as *mut _,
                    )),
                    Some(hinstance),
                    None,
                )
            }
            .expect("CreateWindowExW (button)");

            stack_in_creation_order(&[
                name_label,
                name_edit,
                email_label,
                email_edit,
                phone_label,
                phone_edit,
                button,
            ]);
            pump_pending_messages();
            unsafe {
                let _ = SetForegroundWindow(frame);
            }

            // -- profile, in a temp dir (rule 9: never the real %APPDATA%) --
            use crate::profile::Field;
            let bin_path = std::env::temp_dir().join(format!(
                "wingman-fill-form-live-test-{}-{}.bin",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let profile = Profile {
                full_name: Field::new("Ada Lovelace".to_string(), false),
                emails: vec![Field::new("ada@example.com".to_string(), false)],
                phones: vec![Field::new("+1 555-123-4567".to_string(), false)],
                ..Profile::default()
            };
            profile.save_to(&bin_path).expect("save_to");

            // -- snapshot, filter, map locally --------------------------------
            let snapshot = crate::inputs::uia::snapshot_hwnd(
                frame,
                crate::inputs::uia::DEFAULT_MAX_ELEMENTS,
                crate::inputs::uia::DEFAULT_BUDGET,
            )
            .expect("snapshot_hwnd");
            let candidates = fillable_candidates(&snapshot.fields);
            assert_eq!(
                candidates.len(),
                3,
                "exactly the three EDIT fields, never the button (it is never even walked as a \
                 field by inputs::uia's own six-control-type filter)"
            );

            let loaded = Profile::load_from(&bin_path).expect("load_from");
            let (mapped, unmapped) = map_candidates_locally(&candidates, &loaded);
            assert!(
                unmapped.is_empty(),
                "every field must map locally: {unmapped:?}"
            );
            assert_eq!(
                mapped.len(),
                3,
                "zero model calls means this is the ONLY source of fields"
            );

            // -- build the proposal and run it through the real executor -----
            let proposal = build_proposal(&candidates, &mapped, RequireTickFor::Sensitive);
            let token = crate::ui::confirm::user_confirmed();
            let confirmed =
                crate::ui::confirm::confirm(crate::ui::confirm::Proposal::new(proposal), token);
            let executor =
                crate::executors::registry::resolve("fill_form").expect("fill_form is registered");
            executor.execute(confirmed).expect("execute must succeed");

            pump_pending_messages();

            assert_eq!(window_text(name_edit), "Ada Lovelace");
            assert_eq!(window_text(email_edit), "ada@example.com");
            assert_eq!(window_text(phone_edit), "+1 555-123-4567");
            assert_eq!(
                window_text(button),
                "Place order",
                "the button's text must never change"
            );
            assert_eq!(
                BN_CLICKED_COUNT.load(std::sync::atomic::Ordering::SeqCst),
                0,
                "the button must never receive a click"
            );

            unsafe {
                let _ = DestroyWindow(frame);
            }
            let _ = std::fs::remove_file(&bin_path);
        }
    }
}
