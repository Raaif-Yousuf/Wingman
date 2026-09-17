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
    /// A human-readable label derived from `name` (`"start"` -> `"Start"`).
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
    /// crate-wide, CLAUDE.md rule 3): the card renders fields top-to-bottom
    /// in exactly the order the schema declares them, so a schema author
    /// controls the form's reading order by construction, not by a second
    /// ordering list that could drift from it.
    ///
    /// A property is editable when its schema entry sets `"editable":
    /// true`; every other schema keyword (`"type"`, `"enum"`, ...) is
    /// ignored here -- this is a rendering concern, not a validation one,
    /// and reading an extra, non-standard keyword out of an otherwise
    /// ordinary JSON Schema does not change what gets sent to a provider
    /// (`actions::schema::schema_for` is the only thing that does that).
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
                    let field_value = value_obj.get(name).map(display_string).unwrap_or_default();
                    Field {
                        name: name.clone(),
                        label: label_for(name),
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
/// show in a text field for them).
fn display_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
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
