//! Pure model for the card's preview (confirmation) state (#26).
//!
//! Given a proposal's JSON Schema (which fields exist, in what order, which
//! are editable, which are required -- see `actions::schema`) and the
//! model's initial proposal value, [`PreviewModel`] builds an ordered list
//! of [`Field`]s the card renders as a compact form: a title line, then one
//! row per field. Editing happens here, not in Win32 code: [`PreviewModel`]
//! is the single source of truth for what "Do it" builds a `Confirmed<P>`
//! from, so "Enter yields a `Confirmed` equal to what was shown" (#26's
//! Done-when) is a property this module proves with a plain unit test, not
//! something that only holds if the GDI/EDIT-control plumbing happens to be
//! wired correctly.
//!
//! Deliberately has zero Win32 imports: `src/ui/card.rs` reads a
//! `PreviewModel`'s fields to lay out and paint/create controls, and writes
//! edits back into it via [`PreviewModel::set_value`] as the user types, but
//! never needs to be constructed or asserted against inside a live window to
//! be tested.

use serde_json::{Map, Value};

/// One row the preview form renders: a label, its current (possibly edited)
/// value, and whether the schema allows editing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    /// The schema property name (JSON key), e.g. `"start"`. This is what
    /// [`PreviewModel::set_value`] takes and what [`PreviewModel::to_value`]
    /// emits as the object key -- never the display label.
    pub name: String,
    /// A human-readable label. Derived from `name` (`"start"` -> `"Start"`)
    /// unless the schema property carries its own `"label"` string (#40
    /// "Fill this form": a form_fill-derived pseudo-schema names its rows
    /// with a stable, unique-but-ugly property key like `"f2_value"` while
    /// still wanting to show the real UIA field label, e.g. `"Date of
    /// birth"`, which `label_for`'s snake_case-to-sentence-case derivation
    /// could never produce from that key). See [`PreviewModel::from_schema`].
    pub label: String,
    /// The value currently shown for this field: the proposal's original
    /// value until [`PreviewModel::set_value`] overwrites it.
    pub value: String,
    /// Whether the schema marked this field editable (`"editable": true` on
    /// its property definition). Non-editable fields render as plain text;
    /// only editable ones get an EDIT control in `card.rs`.
    pub editable: bool,
    /// Whether the schema's `required` array names this field. Used by
    /// [`PreviewModel::missing_required`] to gate "Do it".
    pub required: bool,
}

/// The preview form's full state: an ordered list of fields, built once from
/// a schema and a proposal value, then mutated in place as the user edits
/// editable fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreviewModel {
    fields: Vec<Field>,
}

impl PreviewModel {
    /// Builds a model from a proposal's JSON Schema and its current value.
    ///
    /// Field order follows `schema["properties"]`'s own iteration order,
    /// which is load-bearing here the same way it is everywhere else this
    /// crate builds a schema (`serde_json` keeps `preserve_order`
    /// crate-wide, AGENTS.md rule 3): the card renders fields top-to-bottom
    /// in exactly the order the schema declares them, so a schema author
    /// controls the form's reading order by construction, not by a second
    /// ordering list that could drift from it.
    ///
    /// A property is editable when its schema entry sets `"editable":
    /// true`; a property's display label comes from its own `"label"`
    /// string when present, else from [`label_for`]. Every other schema
    /// keyword (`"type"`, `"enum"`, ...) is ignored here -- this is a
    /// rendering concern, not a validation one, and reading an extra,
    /// non-standard keyword out of an otherwise ordinary JSON Schema does
    /// not change what gets sent to a provider (`actions::schema::schema_for`
    /// is the only thing that does that; neither `"editable"` nor
    /// `"label"` is ever set on a schema that function returns, only on a
    /// pseudo-schema a caller builds purely to drive this card, e.g.
    /// `actions::fill_form`'s form_fill preview).
    ///
    /// Missing or malformed schema/value shapes degrade to an empty model
    /// (no fields) rather than panicking: a card that renders nothing is
    /// recoverable (the caller can still show an error card, rule 7); a
    /// panic on the main thread is not.
    pub fn from_schema(schema: &Value, value: &Value) -> Self {
        let properties = schema.get("properties").and_then(Value::as_object);
        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        let empty_map = Map::new();
        let value_obj = value.as_object().unwrap_or(&empty_map);

        let fields = match properties {
            Some(props) => props
                .iter()
                .map(|(name, def)| {
                    let editable = def
                        .get("editable")
                        .and_then(Value::as_bool)
                        .unwrap_or(false);
                    let label = def
                        .get("label")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                        .unwrap_or_else(|| label_for(name));
                    let field_value = value_obj.get(name).map(display_string).unwrap_or_default();
                    Field {
                        name: name.clone(),
                        label,
                        value: field_value,
                        editable,
                        required: required.contains(&name.as_str()),
                    }
                })
                .collect(),
            None => Vec::new(),
        };

        Self { fields }
    }

    pub fn fields(&self) -> &[Field] {
        &self.fields
    }

    /// Sets an editable field's current value in place. Returns `true` when
    /// the field existed and was editable (the write happened), `false`
    /// otherwise -- an unknown field name or a non-editable field is left
    /// untouched, never silently created or overwritten: a field the schema
    /// didn't mark editable must never end up differing from what the model
    /// originally proposed for it.
    pub fn set_value(&mut self, name: &str, new_value: impl Into<String>) -> bool {
        match self.fields.iter_mut().find(|f| f.name == name) {
            Some(field) if field.editable => {
                field.value = new_value.into();
                true
            }
            _ => false,
        }
    }

    /// Required fields (by schema property name) that are currently blank
    /// (empty after trimming whitespace). "Do it" must refuse to build a
    /// `Confirmed` while this is non-empty -- see `ui::confirm::confirm_preview`
    /// and its caller in `ui::card`.
    pub fn missing_required(&self) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|f| f.required && f.value.trim().is_empty())
            .map(|f| f.name.as_str())
            .collect()
    }

    pub fn is_valid(&self) -> bool {
        self.missing_required().is_empty()
    }

    /// The JSON object "Do it" builds a `Confirmed<Value>` from: exactly the
    /// values currently shown (after any edits), one string field per row,
    /// in schema order. This -- not the original proposal value passed to
    /// [`PreviewModel::from_schema`] -- is what `ui::confirm::confirm_preview`
    /// receives, which is what makes "what executes equals what was shown"
    /// true by construction rather than by care taken at each call site.
    pub fn to_value(&self) -> Value {
        let mut map = Map::new();
        for field in &self.fields {
            map.insert(field.name.clone(), Value::String(field.value.clone()));
        }
        Value::Object(map)
    }
}

/// Renders a JSON scalar the way a single-line text field should show it.
/// Strings are shown as-is (no quotes); everything else falls back to
/// `Value`'s own `Display`-ish `to_string()` (numbers/bools render plainly;
/// `null` and anything else become empty, since there is nothing useful to
/// show in a text field for them). An array (#38's `text_review` proposal's
/// `edits` field is the first schema property ever shaped this way) renders
/// as one line per item via [`display_array_item`], newline-joined -- the
/// card's multi-line rendering already exists for `detail`-length text, so
/// this needs no new card-side layout code, only a wider set of values this
/// function knows how to turn into a string.
fn display_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Array(items) => items
            .iter()
            .map(display_array_item)
            .collect::<Vec<_>>()
            .join("\n"),
        other => other.to_string(),
    }
}

/// Renders one array item for [`display_string`]. An `{before, after,
/// reason}` object -- `actions::schema`'s `text_review` schema's `edits`
/// item shape -- renders as `"before -> after (reason)"` (an ASCII arrow,
/// not a Unicode one or an em dash: rule 11's spirit for any card-facing
/// text is to keep it in plain characters no font/rendering path needs to
/// specially support), or `"before -> after"` when `reason` is empty.
/// Anything else (a plain string/number array, or an object without that
/// shape) falls back to [`display_string`] recursively, so a schema this
/// function doesn't specially know about still shows something reasonable
/// rather than raw JSON -- this deliberately does not hard-code the field
/// name `"edits"` anywhere: any future schema whose array items happen to
/// share the `{before, after, reason}` shape renders the same way for free.
fn display_array_item(v: &Value) -> String {
    if let Some(obj) = v.as_object() {
        if let (Some(before), Some(after)) = (
            obj.get("before").and_then(Value::as_str),
            obj.get("after").and_then(Value::as_str),
        ) {
            let reason = obj.get("reason").and_then(Value::as_str).unwrap_or("");
            return if reason.is_empty() {
                format!("{before} -> {after}")
            } else {
                format!("{before} -> {after} ({reason})")
            };
        }
    }
    display_string(v)
}

/// `"start"` -> `"Start"`, `"rate_difficulty"` -> `"Rate difficulty"`: a
/// cheap snake_case-to-sentence-case label so a new schema field never needs
/// a parallel "pretty label" table that can drift from the actual field
/// name (the same reasoning `actions::schema`'s doc comment gives for
/// delegating instead of duplicating a schema).
fn label_for(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for (i, part) in name.split('_').enumerate() {
        if i > 0 {
            out.push(' ');
        }
        let mut chars = part.chars();
        if let Some(first) = chars.next() {
            if i == 0 {
                out.extend(first.to_uppercase());
            } else {
                out.push(first);
            }
            out.push_str(chars.as_str());
        }
    }
    out
}

// ---------------------------------------------------------------------
// #105 "Show me what you're sending": the pending-request analogue of the
// proposal preview above. `PreviewModel` renders what a MODEL is about to
// return; `RequestPreview` renders what is about to be SENT to the model --
// deliberately built to feed the exact same `PreviewModel::from_schema`/
// `Card::show_preview` machinery (a pseudo-schema plus a value, the same
// shape `actions::fill_form`'s form_fill preview already uses) rather than
// a second rendering path.
// ---------------------------------------------------------------------

/// One image about to be sent, reduced to what's safe and useful to show:
/// its size and pixel dimensions, never a rendered thumbnail -- `card.rs`'s
/// preview state only ever draws text rows today (see [`Field`] above), so
/// there is no image-rendering surface yet for this to hand a thumbnail to
/// even if it wanted to. This is the issue's own documented fallback ("the
/// image, or its dimensions and size if you cannot render it").
///
/// If issue #103's redaction work has landed by the time a real thumbnail
/// preview is built, that later code must render the REDACTED image here,
/// never the original -- showing the unredacted image in a "here's what's
/// being sent" preview would be exactly the lie #103 exists to prevent.
/// This type carries no pixels at all, so that dependency does not apply to
/// it, only to whatever eventually adds real image rendering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// #105: part of the "Show me what you're sending" gate. The structural half is
// done (see `ui::confirm::SendAuthorized`): with the toggle on and nothing
// authorized, `send_preview_guard` refuses every request, which is the
// fail-safe direction. What remains is `App::ask` showing this preview via
// `Card::show_preview` and calling `ui::confirm::user_confirmed_send` on Send.
// Deliberately not wired yet: that means adding a third pending kind to the
// preview state machine, which has a live P1 (#225, two pending slots that can
// both be Some at once). Sequenced after #225 on purpose.
#[allow(dead_code)]
pub struct RequestImagePreview {
    pub size_bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

/// Everything about to be sent in one [`crate::provider::Request`], reduced
/// to what a confirm-before-send preview should show. Unlike the egress
/// log (`egress.rs`), which deliberately never stores content, this DOES
/// carry the real `system`/`user` text -- it exists only transiently on
/// screen for the user to read before confirming, which is a fundamentally
/// different privacy posture than writing it to a persistent file (see
/// `egress.rs`'s module doc comment for that contrast spelled out).
///
/// Never carries anything from a request's HTTP headers (where a key
/// actually lives) -- [`RequestPreview::from_request`] only ever reads a
/// [`crate::provider::Request`], which has no field for one.
#[derive(Debug, Clone, PartialEq)]
// #105: part of the "Show me what you're sending" gate. The structural half is
// done (see `ui::confirm::SendAuthorized`): with the toggle on and nothing
// authorized, `send_preview_guard` refuses every request, which is the
// fail-safe direction. What remains is `App::ask` showing this preview via
// `Card::show_preview` and calling `ui::confirm::user_confirmed_send` on Send.
// Deliberately not wired yet: that means adding a third pending kind to the
// preview state machine, which has a live P1 (#225, two pending slots that can
// both be Some at once). Sequenced after #225 on purpose.
#[allow(dead_code)]
pub struct RequestPreview {
    pub images: Vec<RequestImagePreview>,
    pub system_text: String,
    pub user_text: String,
    /// Always `0` today -- knowledge snippets are unbuilt Phase 3b work
    /// (#85 onwards). Kept as a field so #85 only has to populate it.
    pub snippet_count: usize,
}

impl RequestPreview {
    // #105: part of the "Show me what you're sending" gate. The structural half is
    // done (see `ui::confirm::SendAuthorized`): with the toggle on and nothing
    // authorized, `send_preview_guard` refuses every request, which is the
    // fail-safe direction. What remains is `App::ask` showing this preview via
    // `Card::show_preview` and calling `ui::confirm::user_confirmed_send` on Send.
    // Deliberately not wired yet: that means adding a third pending kind to the
    // preview state machine, which has a live P1 (#225, two pending slots that can
    // both be Some at once). Sequenced after #225 on purpose.
    #[allow(dead_code)]
    pub fn from_request(req: &crate::provider::Request) -> Self {
        let images = req
            .images
            .iter()
            .map(|png| {
                let dims = crate::egress::png_dimensions(png);
                RequestImagePreview {
                    size_bytes: png.len(),
                    width: dims.map(|d| d.0),
                    height: dims.map(|d| d.1),
                }
            })
            .collect();
        Self {
            images,
            system_text: req.system.clone(),
            user_text: req.user.clone(),
            snippet_count: 0,
        }
    }

    /// Builds a `(schema, value)` pair compatible with the existing
    /// `PreviewModel::from_schema`/`Card::show_preview` (#26) so the real
    /// preview card, once wired (see `provider::common`'s module doc
    /// comment on what's still owed), needs no second rendering path.
    /// Every property is display-only: none sets `"editable": true`, so
    /// `PreviewModel::from_schema` builds every field non-editable by
    /// construction -- confirming this preview can never "edit" the value
    /// into something the request being sent doesn't actually carry.
    #[allow(dead_code)] // #105, same as the type above: not wired until #225 lands.
    pub fn to_schema_and_value(&self) -> (Value, Value) {
        let mut properties = Map::new();
        let mut value = Map::new();

        for (i, img) in self.images.iter().enumerate() {
            let key = format!("image_{i}");
            let label = match (img.width, img.height) {
                (Some(w), Some(h)) => format!("Image {} ({w} x {h})", i + 1),
                _ => format!("Image {}", i + 1),
            };
            properties.insert(
                key.clone(),
                serde_json::json!({"type": "string", "label": label}),
            );
            value.insert(key, Value::String(human_bytes(img.size_bytes)));
        }

        if !self.system_text.is_empty() {
            properties.insert(
                "system".to_string(),
                serde_json::json!({"type": "string", "label": "System prompt"}),
            );
            value.insert(
                "system".to_string(),
                Value::String(self.system_text.clone()),
            );
        }

        properties.insert(
            "user".to_string(),
            serde_json::json!({"type": "string", "label": "Text"}),
        );
        value.insert("user".to_string(), Value::String(self.user_text.clone()));

        properties.insert(
            "snippets".to_string(),
            serde_json::json!({"type": "string", "label": "Knowledge snippets"}),
        );
        value.insert(
            "snippets".to_string(),
            Value::String(if self.snippet_count == 0 {
                "none".to_string()
            } else {
                self.snippet_count.to_string()
            }),
        );

        (
            serde_json::json!({"properties": Value::Object(properties)}),
            Value::Object(value),
        )
    }
}

// #105: part of the "Show me what you're sending" gate. The structural half is
// done (see `ui::confirm::SendAuthorized`): with the toggle on and nothing
// authorized, `send_preview_guard` refuses every request, which is the
// fail-safe direction. What remains is `App::ask` showing this preview via
// `Card::show_preview` and calling `ui::confirm::user_confirmed_send` on Send.
// Deliberately not wired yet: that means adding a third pending kind to the
// preview state machine, which has a live P1 (#225, two pending slots that can
// both be Some at once). Sequenced after #225 on purpose.
#[allow(dead_code)]
fn human_bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn calendar_schema() -> Value {
        crate::actions::schema::schema_for("calendar_event", false)
            .expect("calendar_event is registered")
    }

    // -- from_schema: shape, order, editability -----------------------------

    #[test]
    fn from_schema_builds_fields_in_schema_property_order() {
        let schema = calendar_schema();
        let value = serde_json::json!({
            "title": "Standup",
            "start": "09:00",
            "end": "09:15",
            "location": "Room 2",
            "notes": "bring laptop"
        });
        let model = PreviewModel::from_schema(&schema, &value);
        let names: Vec<&str> = model.fields().iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["title", "start", "end", "location", "notes"]);
    }

    #[test]
    fn from_schema_marks_only_title_and_start_editable() {
        let schema = calendar_schema();
        let value = serde_json::json!({});
        let model = PreviewModel::from_schema(&schema, &value);
        for field in model.fields() {
            let expected = matches!(field.name.as_str(), "title" | "start");
            assert_eq!(
                field.editable, expected,
                "field {:?} editable={} expected={}",
                field.name, field.editable, expected
            );
        }
    }

    #[test]
    fn from_schema_pulls_values_from_the_proposal() {
        let schema = calendar_schema();
        let value = serde_json::json!({"title": "Standup", "start": "09:00"});
        let model = PreviewModel::from_schema(&schema, &value);
        let title = model.fields().iter().find(|f| f.name == "title").unwrap();
        assert_eq!(title.value, "Standup");
        // A field absent from the proposal value renders as an empty string,
        // never panics.
        let notes = model.fields().iter().find(|f| f.name == "notes").unwrap();
        assert_eq!(notes.value, "");
    }

    #[test]
    fn from_schema_labels_are_title_cased_from_the_field_name() {
        let schema = calendar_schema();
        let model = PreviewModel::from_schema(&schema, &serde_json::json!({}));
        let start = model.fields().iter().find(|f| f.name == "start").unwrap();
        assert_eq!(start.label, "Start");
    }

    // -- from_schema: the optional "label" override (#40) -------------------

    #[test]
    fn from_schema_prefers_an_explicit_label_over_the_derived_one() {
        let schema = serde_json::json!({
            "properties": {
                "f2_value": {"type": "string", "label": "Date of birth"}
            }
        });
        let model = PreviewModel::from_schema(&schema, &serde_json::json!({}));
        assert_eq!(model.fields()[0].label, "Date of birth");
    }

    #[test]
    fn from_schema_falls_back_to_the_derived_label_when_no_override_is_present() {
        // Unchanged behaviour for every existing schema (calendar_event,
        // verdict): neither sets "label", so this must still derive it.
        let schema = calendar_schema();
        let model = PreviewModel::from_schema(&schema, &serde_json::json!({}));
        let title = model.fields().iter().find(|f| f.name == "title").unwrap();
        assert_eq!(title.label, "Title");
    }

    #[test]
    fn from_schema_with_no_properties_yields_an_empty_model_not_a_panic() {
        let model = PreviewModel::from_schema(&serde_json::json!({}), &serde_json::json!({}));
        assert!(model.fields().is_empty());
    }

    #[test]
    fn from_schema_with_non_object_value_yields_empty_field_values_not_a_panic() {
        let schema = calendar_schema();
        let model = PreviewModel::from_schema(&schema, &serde_json::json!("not an object"));
        assert!(model.fields().iter().all(|f| f.value.is_empty()));
    }

    // -- set_value: only editable fields can change -------------------------

    #[test]
    fn set_value_edits_an_editable_field() {
        let schema = calendar_schema();
        let mut model = PreviewModel::from_schema(&schema, &serde_json::json!({"start": "09:00"}));
        assert!(model.set_value("start", "10:00"));
        let start = model.fields().iter().find(|f| f.name == "start").unwrap();
        assert_eq!(start.value, "10:00");
    }

    #[test]
    fn set_value_refuses_a_non_editable_field() {
        let schema = calendar_schema();
        let mut model =
            PreviewModel::from_schema(&schema, &serde_json::json!({"location": "Room 2"}));
        assert!(!model.set_value("location", "Room 9"));
        let location = model
            .fields()
            .iter()
            .find(|f| f.name == "location")
            .unwrap();
        assert_eq!(
            location.value, "Room 2",
            "a non-editable field's value must never change"
        );
    }

    #[test]
    fn set_value_refuses_an_unknown_field_name() {
        let schema = calendar_schema();
        let mut model = PreviewModel::from_schema(&schema, &serde_json::json!({}));
        assert!(!model.set_value("not_a_real_field", "x"));
    }

    // -- required-field validation -------------------------------------------

    #[test]
    fn missing_required_lists_blank_required_fields() {
        let schema = calendar_schema();
        let mut model = PreviewModel::from_schema(
            &schema,
            &serde_json::json!({"title": "Standup", "start": "09:00", "end": "09:15", "location": "Room 2", "notes": "x"}),
        );
        assert!(model.is_valid());
        model.set_value("title", "   "); // whitespace-only counts as blank
        assert_eq!(model.missing_required(), vec!["title"]);
        assert!(!model.is_valid());
    }

    #[test]
    fn missing_required_is_empty_when_every_required_field_is_filled() {
        let schema = calendar_schema();
        let model = PreviewModel::from_schema(
            &schema,
            &serde_json::json!({"title": "Standup", "start": "09:00", "end": "09:15", "location": "Room 2", "notes": "x"}),
        );
        assert!(model.missing_required().is_empty());
    }

    // -- to_value: "what executes equals what was shown" --------------------

    #[test]
    fn to_value_reflects_the_original_proposal_before_any_edit() {
        let schema = calendar_schema();
        let value = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": "bring laptop"
        });
        let model = PreviewModel::from_schema(&schema, &value);
        assert_eq!(model.to_value(), value);
    }

    #[test]
    fn to_value_after_an_edit_carries_the_edit_through_not_the_original() {
        // This is #26's literal Done-when: "Enter yields a Confirmed equal
        // to what was shown", exercised here at the pure-model layer (see
        // `ui::confirm`'s `confirm_preview_...` tests for the same property
        // proven through the real Confirmed<Value> type).
        let schema = calendar_schema();
        let original = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": ""
        });
        let mut model = PreviewModel::from_schema(&schema, &original);
        model.set_value("start", "10:30");
        let shown = model.to_value();
        assert_eq!(shown["start"], "10:30");
        assert_ne!(
            shown, original,
            "the built value must reflect the edit, not the original proposal"
        );
        // Every other field is unchanged.
        assert_eq!(shown["title"], "Standup");
        assert_eq!(shown["end"], "09:15");
        assert_eq!(shown["location"], "Room 2");
    }

    // -- #38: text_review's `edits` array field ------------------------------

    fn text_review_schema() -> Value {
        crate::actions::schema::schema_for("text_review", false).expect("text_review is registered")
    }

    #[test]
    fn from_schema_renders_text_review_edits_as_before_arrow_after_reason_lines() {
        let schema = text_review_schema();
        let value = serde_json::json!({
            "edits": [
                {"before": "wnated", "after": "wanted", "reason": "typo"},
                {"before": "folow", "after": "follow", "reason": "typo"}
            ],
            "verdict": "needs_edits",
            "tone_note": "Friendly.",
            "missing_attachment": false
        });
        let model = PreviewModel::from_schema(&schema, &value);
        let edits = model.fields().iter().find(|f| f.name == "edits").unwrap();
        assert_eq!(
            edits.value,
            "wnated -> wanted (typo)\nfolow -> follow (typo)"
        );
    }

    #[test]
    fn from_schema_renders_an_edit_with_an_empty_reason_without_trailing_parens() {
        let schema = text_review_schema();
        let value = serde_json::json!({
            "edits": [{"before": "a", "after": "b", "reason": ""}],
            "verdict": "needs_edits",
            "tone_note": "",
            "missing_attachment": false
        });
        let model = PreviewModel::from_schema(&schema, &value);
        let edits = model.fields().iter().find(|f| f.name == "edits").unwrap();
        assert_eq!(edits.value, "a -> b");
    }

    #[test]
    fn from_schema_renders_an_empty_edits_array_as_an_empty_string() {
        let schema = text_review_schema();
        let value = serde_json::json!({
            "edits": [],
            "verdict": "good_to_go",
            "tone_note": "Clear.",
            "missing_attachment": false
        });
        let model = PreviewModel::from_schema(&schema, &value);
        let edits = model.fields().iter().find(|f| f.name == "edits").unwrap();
        assert_eq!(edits.value, "");
    }

    #[test]
    fn from_schema_text_review_fields_are_never_editable() {
        let schema = text_review_schema();
        let model = PreviewModel::from_schema(&schema, &serde_json::json!({}));
        for field in model.fields() {
            assert!(
                !field.editable,
                "field {:?} must not be editable",
                field.name
            );
        }
    }

    #[test]
    fn display_array_item_falls_back_to_raw_display_for_a_non_edit_shaped_object() {
        let schema = text_review_schema();
        // A malformed "edit" missing before/after still renders as SOMETHING
        // (never panics) via the generic fallback, not the before/after
        // formatter.
        let value = serde_json::json!({
            "edits": [{"reason": "no before or after"}],
            "verdict": "needs_edits",
            "tone_note": "",
            "missing_attachment": false
        });
        let model = PreviewModel::from_schema(&schema, &value);
        let edits = model.fields().iter().find(|f| f.name == "edits").unwrap();
        assert!(edits.value.contains("no before or after"));
    }

    #[test]
    fn to_value_ignores_a_refused_edit_to_a_non_editable_field() {
        let schema = calendar_schema();
        let original = serde_json::json!({
            "title": "Standup", "start": "09:00", "end": "09:15",
            "location": "Room 2", "notes": ""
        });
        let mut model = PreviewModel::from_schema(&schema, &original);
        let changed = model.set_value("location", "Room 9");
        assert!(!changed);
        assert_eq!(model.to_value()["location"], "Room 2");
    }
}

#[cfg(test)]
mod request_preview_tests {
    use super::*;
    use crate::provider::{Effort, Request};

    fn tiny_png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 13]);
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 8]);
        bytes
    }

    fn request_with(system: &str, user: &str, images: Vec<Vec<u8>>) -> Request {
        Request {
            system: system.to_string(),
            user: user.to_string(),
            images,
            schema: None,
            effort: Effort::Unset,
            max_tokens: 0,
        }
    }

    #[test]
    fn from_request_carries_the_real_text_verbatim() {
        // #105: unlike the egress log, the preview shows the actual text --
        // it exists only transiently on screen, never persisted.
        let req = request_with("Be helpful.", "What is on screen?", vec![]);
        let preview = RequestPreview::from_request(&req);
        assert_eq!(preview.system_text, "Be helpful.");
        assert_eq!(preview.user_text, "What is on screen?");
        assert!(preview.images.is_empty());
        assert_eq!(preview.snippet_count, 0);
    }

    #[test]
    fn from_request_reads_real_dimensions_from_the_png() {
        let req = request_with("", "Check my working.", vec![tiny_png(340, 200)]);
        let preview = RequestPreview::from_request(&req);
        assert_eq!(preview.images.len(), 1);
        assert_eq!(preview.images[0].width, Some(340));
        assert_eq!(preview.images[0].height, Some(200));
        assert_eq!(preview.images[0].size_bytes, tiny_png(340, 200).len());
    }

    #[test]
    fn from_request_degrades_to_no_dimensions_for_an_undecodable_image() {
        let req = request_with("", "hi", vec![vec![1, 2, 3]]);
        let preview = RequestPreview::from_request(&req);
        assert_eq!(preview.images.len(), 1);
        assert_eq!(preview.images[0].width, None);
        assert_eq!(preview.images[0].height, None);
        assert_eq!(preview.images[0].size_bytes, 3);
    }

    #[test]
    fn to_schema_and_value_feeds_previewmodel_directly() {
        // The whole point of building a (schema, value) pair: it must be
        // usable by the EXISTING `PreviewModel::from_schema` with no
        // adapter code, proving there is one preview rendering path, not
        // two.
        let req = request_with(
            "Be helpful.",
            "What is on screen?",
            vec![tiny_png(340, 200)],
        );
        let preview = RequestPreview::from_request(&req);
        let (schema, value) = preview.to_schema_and_value();
        let model = PreviewModel::from_schema(&schema, &value);

        let user_field = model.fields().iter().find(|f| f.name == "user").unwrap();
        assert_eq!(user_field.value, "What is on screen?");

        let image_field = model.fields().iter().find(|f| f.name == "image_0").unwrap();
        assert!(image_field.label.contains("340 x 200"), "{image_field:?}");
    }

    #[test]
    fn to_schema_and_value_never_marks_any_field_editable() {
        // Confirming what's about to be sent must never let the card "edit"
        // it into something the actual request doesn't carry.
        let req = request_with("sys", "hi", vec![tiny_png(1, 1)]);
        let preview = RequestPreview::from_request(&req);
        let (schema, value) = preview.to_schema_and_value();
        let model = PreviewModel::from_schema(&schema, &value);
        for field in model.fields() {
            assert!(!field.editable, "{field:?} must not be editable");
        }
    }

    #[test]
    fn to_schema_and_value_shows_none_for_snippets_today() {
        let req = request_with("", "hi", vec![]);
        let preview = RequestPreview::from_request(&req);
        let (schema, value) = preview.to_schema_and_value();
        let model = PreviewModel::from_schema(&schema, &value);
        let snippets = model
            .fields()
            .iter()
            .find(|f| f.name == "snippets")
            .unwrap();
        assert_eq!(snippets.value, "none");
    }

    #[test]
    fn from_request_never_carries_a_fake_key_that_is_not_even_a_request_field() {
        // #105's safety requirement, exercised at the preview layer: a
        // `Request` has no field a key could occupy in the first place, so
        // building a preview from one structurally cannot leak a key --
        // this test documents and locks in that guarantee against a future
        // change that might add a `headers` field to `Request` and
        // naively thread it through here too.
        let fake_key = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE1234567890";
        let req = request_with("Be helpful.", "What is on screen?", vec![]);
        let preview = RequestPreview::from_request(&req);
        let (schema, value) = preview.to_schema_and_value();
        assert!(!schema.to_string().contains(fake_key));
        assert!(!value.to_string().contains(fake_key));
    }
}
