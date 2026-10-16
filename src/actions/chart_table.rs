//! "Chart to data table" (#114): read the values off a chart on screen and put
//! them on the clipboard as a table.
//!
//! The model fills the same `table` proposal as "Table to CSV" (#69): a
//! label column, then one value column per series. Nothing printed on a chart
//! is exact (a bar has no number on it), so what the model returns is an
//! estimate and the action says so in two places. In code, every value column
//! header gets " (estimate)" ([`mark_estimates`]), so the CSV carries the
//! warning into the spreadsheet. On the card, a note sits under the rows
//! ([`card_note`]). Neither relies on the model remembering to say it.

use serde_json::Value;

use super::table::Table;
use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "chart-to-data-table";

/// The text added to each value column header.
pub const ESTIMATE_SUFFIX: &str = " (estimate)";

/// The note on the preview card: the values are read off a picture.
pub const ESTIMATE_NOTE: &str =
    "These values are estimates read off the chart, not exact figures. Check them against the source.";

/// The action's prompt. Printed numbers are copied; anything else is read
/// against the axis and rounded, never invented.
pub const BASE_PROMPT: &str = "You are shown a chart (a bar chart, line chart, pie chart or similar), as a screenshot. Turn it into a table. First fill `columns`: the first header names what the labels are (the axis title if there is one, otherwise \"Label\"), then one header per data series, named as in the legend, or with the axis title or unit when there is a single series. Then fill `rows` with one list of cells per bar, point or slice, in the order shown, each cell a plain string. The first cell is the label exactly as shown. The other cells are the values: copy a number that is printed on the chart exactly as shown, otherwise read the value against the axis and give a plain number with at most three significant digits, with no units or symbols. For a pie chart use the printed percentage when there is one. If a value is unclear or cannot be read, use an empty string and never invent one. Do not add totals, trends or comments. If the screen holds no chart, return empty lists. Use plain text only: no markdown and no LaTeX.";

/// The built-in action: group Work, a screenshot input, proposal `table`,
/// executor `clipboard`, `confirm = true` so the user sees the estimated rows
/// before they are copied.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Chart to data table".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Screen],
        proposal: super::table::PROPOSAL_KIND.to_string(),
        executor: "clipboard".to_string(),
        confirm: true,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

/// The `table` proposal with every value column header (every column but the
/// first) marked as an estimate. A header that already says "estimate" is
/// left alone, so a model that obeys a custom prompt is not marked twice.
/// A reply that does not read as a table comes back unchanged.
pub fn mark_estimates(proposal: &Value) -> Value {
    let Some(mut table) = Table::from_value(proposal) else {
        return proposal.clone();
    };
    for header in table.columns.iter_mut().skip(1) {
        if !header.to_lowercase().contains("estimate") {
            header.push_str(ESTIMATE_SUFFIX);
        }
    }
    table.to_value()
}

/// The card note for a chart table: the estimate warning, then any note the
/// card would otherwise show (a cap or fallback note), on its own paragraph.
pub fn card_note(other: Option<&str>) -> String {
    match other {
        Some(note) if !note.is_empty() => format!("{ESTIMATE_NOTE}\n\n{note}"),
        _ => ESTIMATE_NOTE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_entry_confirms_then_copies_a_table_read_from_the_screen() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.proposal, "table");
        assert_eq!(a.executor, "clipboard");
        assert!(a.confirm, "the estimates are shown before they are copied");
        assert_eq!(a.inputs, vec![InputKind::Screen]);
        assert!(!a.prompt.contains('\u{2014}'), "no em dashes in the prompt");
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the clipboard executor resolves");
    }

    #[test]
    fn it_is_registered_as_a_built_in() {
        let ids: Vec<String> = crate::actions::builtin_actions()
            .into_iter()
            .map(|a| a.id)
            .collect();
        assert!(ids.iter().any(|id| id == ACTION_ID), "{ids:?}");
    }

    #[test]
    fn the_prompt_says_to_estimate_and_never_invent() {
        for needle in [
            "never invent",
            "against the axis",
            "pie chart",
            "empty lists",
        ] {
            assert!(BASE_PROMPT.contains(needle), "{needle}");
        }
    }

    #[test]
    fn every_value_column_is_marked_and_the_label_column_is_not() {
        let p = json!({
            "columns": ["Month", "Sales", "Costs"],
            "rows": [["Jan", "10", "4"], ["Feb", "12", "5"]]
        });
        let marked = mark_estimates(&p);
        assert_eq!(
            marked["columns"],
            json!(["Month", "Sales (estimate)", "Costs (estimate)"])
        );
        assert_eq!(marked["rows"], p["rows"], "the cells are untouched");
    }

    #[test]
    fn the_marked_headers_reach_the_csv() {
        let p = json!({"columns": ["Fruit", "Count"], "rows": [["Apple", "3"], ["Pear", "5"]]});
        assert_eq!(
            crate::actions::table::csv_from_value(&mark_estimates(&p)).unwrap(),
            "Fruit,Count (estimate)\r\nApple,3\r\nPear,5"
        );
    }

    #[test]
    fn a_header_that_already_says_estimate_is_not_marked_twice() {
        let p = json!({"columns": ["X", "Value (estimate)", "Approx. estimate"], "rows": [["a", "1", "2"]]});
        assert_eq!(mark_estimates(&p)["columns"], p["columns"]);
    }

    #[test]
    fn a_reply_that_is_not_a_table_comes_back_unchanged() {
        for p in [
            json!({"columns": ["A"]}),
            json!({"columns": ["A"], "rows": ["x"]}),
            json!({"text": "hi"}),
        ] {
            assert_eq!(mark_estimates(&p), p);
        }
    }

    #[test]
    fn a_single_column_table_has_nothing_to_mark() {
        let p = json!({"columns": ["Label"], "rows": [["a"]]});
        assert_eq!(mark_estimates(&p)["columns"], json!(["Label"]));
    }

    #[test]
    fn the_card_note_leads_with_the_warning_and_keeps_the_other_note() {
        assert_eq!(card_note(None), ESTIMATE_NOTE);
        let both = card_note(Some("Answered by the backup model."));
        assert!(both.starts_with(ESTIMATE_NOTE));
        assert!(
            both.ends_with("\n\nAnswered by the backup model."),
            "{both}"
        );
        assert!(!ESTIMATE_NOTE.contains('\u{2014}'));
    }

    #[test]
    fn a_hostile_label_is_still_guarded_in_the_csv() {
        let p = json!({"columns": ["L", "V"], "rows": [["=cmd()", "1"]]});
        let csv = crate::actions::table::csv_from_value(&mark_estimates(&p)).unwrap();
        assert!(csv.contains("\r\n'=cmd(),1"), "{csv}");
    }

    #[test]
    fn the_table_type_is_the_shared_one() {
        // `mark_estimates` goes through `Table`, so the caps apply too.
        let cells: Vec<Value> = (0..3).map(|i| json!(i.to_string())).collect();
        let p = json!({"columns": cells, "rows": [cells]});
        assert!(Table::from_value(&mark_estimates(&p)).is_some());
    }
}
