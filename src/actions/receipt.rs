//! "Receipt to expense row" (#70): read a receipt off the screen and put one
//! expense row on the clipboard.
//!
//! Nothing here needs new plumbing. The model fills the same `table` proposal
//! as "Table to CSV" (#69) with a fixed header and a single row; the preview
//! card shows that row, and "Do it" runs the `clipboard` executor, which writes
//! it through [`super::table`]'s CSV quoting and spreadsheet-formula guard. A
//! merchant name read off a hostile receipt cannot run as a formula.

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "receipt-to-expense-row";

/// The header the prompt asks for. The row below it has one cell per column.
#[cfg(test)]
const COLUMNS: [&str; 5] = ["Date", "Merchant", "Amount", "Currency", "Category"];

/// The action's prompt. The total is copied, never added up: a model that
/// does its own arithmetic on a receipt gets the amount wrong silently.
pub const BASE_PROMPT: &str = "You are shown a receipt or an invoice (as text, or as a screenshot if no text was selected). Fill `columns` with exactly these five headers in this order: \"Date\", \"Merchant\", \"Amount\", \"Currency\", \"Category\". Then fill `rows` with exactly one row of five cells, one per column. Date: the date printed on the receipt, written as YYYY-MM-DD when the year is shown and exactly as printed when it is not. Merchant: the shop or company name as printed. Amount: the final total paid, copied as printed, digits and decimal point only, with no currency symbol; do not add up the items yourself and do not use a subtotal. Currency: the three-letter code (USD, EUR, GBP and so on) when the receipt shows a code or an unambiguous symbol, otherwise an empty string. Category: one of Meals, Groceries, Travel, Fuel, Lodging, Office, Software, Entertainment, Health, Other; use Other when unsure. Use an empty string for anything the receipt does not show and never guess a value. If there is no receipt, return empty lists. Use plain text only: no markdown and no LaTeX.";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `table`, executor `clipboard`, `confirm = true`
/// so the user sees the row before it is copied.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Receipt to expense row".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_entry_confirms_then_copies_a_table() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.proposal, "table");
        assert_eq!(a.executor, "clipboard");
        assert!(a.confirm, "the row is shown before it is copied");
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
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
    fn the_prompt_names_every_column_in_order() {
        let mut at = 0;
        for col in COLUMNS {
            let needle = format!("\"{col}\"");
            let found = BASE_PROMPT[at..]
                .find(&needle)
                .unwrap_or_else(|| panic!("{col} missing or out of order"));
            at += found + needle.len();
        }
        assert!(BASE_PROMPT.contains("never guess"));
        assert!(BASE_PROMPT.contains("one row"));
    }

    #[test]
    fn one_row_becomes_a_header_and_a_row() {
        let p = json!({
            "columns": COLUMNS,
            "rows": [["2026-10-03", "Cafe Nero, Main St", "12.50", "GBP", "Meals"]]
        });
        assert_eq!(
            crate::actions::table::csv_from_value(&p).unwrap(),
            "Date,Merchant,Amount,Currency,Category\r\n\
             2026-10-03,\"Cafe Nero, Main St\",12.50,GBP,Meals"
        );
    }

    #[test]
    fn a_hostile_merchant_name_is_not_a_formula() {
        let p = json!({
            "columns": COLUMNS,
            "rows": [["2026-10-03", "=HYPERLINK(\"http://x\")", "5.00", "USD", "Other"]]
        });
        let csv = crate::actions::table::csv_from_value(&p).unwrap();
        assert!(csv.contains("\"'=HYPERLINK("), "{csv}");
    }

    #[test]
    fn no_receipt_is_an_empty_table() {
        let p = json!({"columns": [], "rows": []});
        assert!(crate::actions::table::is_empty_result("table", &p));
        assert!(crate::actions::table::csv_from_value(&p).is_none());
    }
}
