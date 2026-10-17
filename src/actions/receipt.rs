//! "Receipt to expense row" (#70): read a receipt off the screen and put one
//! expense row on the clipboard.
//!
//! Nothing here needs new plumbing. The model fills the same `table` proposal
//! as "Table to CSV" (#69) with a fixed header and a single row; the preview
//! card shows that row, and "Do it" runs the `clipboard` executor, which writes
//! it through [`super::table`]'s tab-separated quoting and spreadsheet-formula guard. A
//! merchant name read off a hostile receipt cannot run as a formula.

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "receipt-to-expense-row";

/// The header the prompt asks for. The row below it has one cell per column.
const COLUMNS: [&str; 5] = ["Date", "Merchant", "Amount", "Currency", "Category"];

/// Forces a `table` proposal for this action into the shape the prompt asks
/// for: the five fixed headers and at most one row of five cells (extra cells
/// dropped, missing ones empty). The model is asked for this, but nothing else
/// enforces it, and what the card shows is what is copied. A proposal with no
/// rows, or one that is not a table, is returned unchanged so the empty and
/// unreadable answers still apply.
pub fn clamp_receipt(proposal: &serde_json::Value) -> serde_json::Value {
    let Some(table) = super::table::Table::from_value(proposal) else {
        return proposal.clone();
    };
    let Some(first) = table.rows.into_iter().next() else {
        return proposal.clone();
    };
    let mut row: Vec<String> = first.into_iter().take(COLUMNS.len()).collect();
    row.resize(COLUMNS.len(), String::new());
    // #710: tidy what can be tidied for certain; the rest is left as the
    // model wrote it and named on the card by [`card_note`].
    let row: Vec<String> = row
        .iter()
        .enumerate()
        .map(|(i, cell)| check_cell(i, cell).0)
        .collect();
    serde_json::json!({ "columns": COLUMNS, "rows": [row] })
}

/// The categories the prompt offers.
const CATEGORIES: [&str; 10] = [
    "Meals",
    "Groceries",
    "Travel",
    "Fuel",
    "Lodging",
    "Office",
    "Software",
    "Entertainment",
    "Health",
    "Other",
];

/// Checks one cell of the row and tidies it when the fix is certain. Returns
/// the cell and whether it passed. An empty cell passes (the prompt asks for
/// an empty string when the receipt does not show a value). A cell that fails
/// comes back as it was, only trimmed: the card shows it and says so, and the
/// user decides, so nothing is guessed (#710). Column 1, the merchant, is free
/// text and always passes.
fn check_cell(column: usize, cell: &str) -> (String, bool) {
    let cell = cell.trim();
    if cell.is_empty() {
        return (String::new(), true);
    }
    let fixed = match column {
        0 => normalise_date(cell),
        2 => normalise_amount(cell),
        3 => normalise_currency(cell),
        4 => CATEGORIES
            .iter()
            .find(|c| c.eq_ignore_ascii_case(cell))
            .map(|c| (*c).to_string()),
        _ => Some(cell.to_string()),
    };
    match fixed {
        Some(ok) => (ok, true),
        None => (cell.to_string(), false),
    }
}

const MONTHS: [&str; 12] = [
    "jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec",
];

/// A month name and a day with no year, in either order (`Oct 3`, `3 Oct`,
/// `October 3rd`). The prompt asks for a date exactly as printed when the
/// receipt shows no year, so this form is expected and kept as it is.
fn is_yearless_date(cell: &str) -> bool {
    let words: Vec<&str> = cell
        .split(|c: char| c.is_whitespace() || c == ',' || c == '.')
        .filter(|w| !w.is_empty())
        .collect();
    let [a, b] = words.as_slice() else {
        return false;
    };
    let month = |w: &str| {
        let w = w.to_ascii_lowercase();
        w.len() >= 3
            && w.bytes().all(|b| b.is_ascii_lowercase())
            && MONTHS.iter().any(|m| w.starts_with(m))
    };
    let day = |w: &str| {
        let digits = w.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let suffix = w[digits.len()..].to_ascii_lowercase();
        matches!(digits.len(), 1 | 2)
            && digits.bytes().all(|b| b.is_ascii_digit())
            && matches!(suffix.as_str(), "" | "st" | "nd" | "rd" | "th")
            && digits.parse::<u32>().is_ok_and(|d| (1..=31).contains(&d))
    };
    (month(a) && day(b)) || (day(a) && month(b))
}

/// `YYYY-MM-DD`, from a year-first date with `-`, `/` or `.` between the
/// parts (a time after it is dropped). A yearless date such as `Oct 3` is
/// kept as printed. `None` for anything else, including a day that the month
/// does not have.
fn normalise_date(cell: &str) -> Option<String> {
    if is_yearless_date(cell) {
        return Some(cell.to_string());
    }
    let date = cell
        .split_once(['T', ' '])
        .filter(|(_, rest)| rest.starts_with(|c: char| c.is_ascii_digit()))
        .map_or(cell, |(date, _)| date);
    let mut parts = date.split(['-', '/', '.']);
    let (y, m, d) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() || y.len() != 4 {
        return None;
    }
    let number = |s: &str, max_len: usize| {
        (!s.is_empty() && s.len() <= max_len && s.bytes().all(|b| b.is_ascii_digit()))
            .then(|| s.parse::<u32>().ok())
            .flatten()
    };
    let (y, m, d) = (number(y, 4)?, number(m, 2)?, number(d, 2)?);
    let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=days)
        .contains(&d)
        .then(|| format!("{y:04}-{m:02}-{d:02}"))
}

/// Digits and one decimal point, with an optional leading minus. One leading
/// or trailing currency symbol and spaces are dropped, and so are thousands commas when they sit
/// exactly between groups of three digits (`1,234.50`). `12,50` could be a
/// European decimal, so it is not touched. `None` when it is not a number.
fn normalise_amount(cell: &str) -> Option<String> {
    const SYMBOLS: [char; 4] = ['$', '\u{20ac}', '\u{a3}', '\u{a5}'];
    // Only a symbol at the start (after a minus) or the end counts; one in the
    // middle (`1$2`) means the cell is not a plain amount.
    let cell = cell.trim();
    let (neg, cell) = match cell.strip_prefix('-') {
        Some(rest) => ("-", rest.trim_start()),
        None => ("", cell),
    };
    let cell = match cell.strip_prefix(SYMBOLS) {
        Some(rest) => rest,
        None => cell.strip_suffix(SYMBOLS).unwrap_or(cell),
    };
    let stripped: String = neg
        .chars()
        .chain(cell.chars().filter(|c| !c.is_whitespace()))
        .collect();
    let (sign, body) = match stripped.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", stripped.as_str()),
    };
    let (whole, fraction) = match body.split_once('.') {
        Some((w, f)) => (w, Some(f)),
        None => (body, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if fraction.is_some_and(|f| !digits(f)) {
        return None;
    }
    let groups: Vec<&str> = whole.split(',').collect();
    let whole_ok = match groups.as_slice() {
        [only] => digits(only),
        [first, rest @ ..] => {
            digits(first) && first.len() <= 3 && rest.iter().all(|g| g.len() == 3 && digits(g))
        }
        [] => false,
    };
    whole_ok.then(|| match fraction {
        Some(f) => format!("{sign}{}.{f}", groups.concat()),
        None => format!("{sign}{}", groups.concat()),
    })
}

/// A three-letter code in capitals. The euro and pound signs name one
/// currency each, so they become `EUR` and `GBP`; `$` and `¥` are shared by
/// several currencies and stay unresolved.
fn normalise_currency(cell: &str) -> Option<String> {
    match cell {
        "\u{20ac}" => Some("EUR".to_string()),
        "\u{a3}" => Some("GBP".to_string()),
        _ if cell.len() == 3 && cell.bytes().all(|b| b.is_ascii_alphabetic()) => {
            Some(cell.to_ascii_uppercase())
        }
        _ => None,
    }
}

/// Columns whose cell is filled but did not pass the check, in column order.
/// Empty for a proposal that is not a one-row receipt table.
pub fn unchecked_columns(proposal: &serde_json::Value) -> Vec<&'static str> {
    let Some(table) = super::table::Table::from_value(proposal) else {
        return Vec::new();
    };
    let Some(row) = table.rows.first() else {
        return Vec::new();
    };
    COLUMNS
        .iter()
        .enumerate()
        .filter(|(i, _)| !check_cell(*i, row.get(*i).map_or("", String::as_str)).1)
        .map(|(_, name)| *name)
        .collect()
}

/// The card note for the cells that did not pass, put in front of an
/// existing note. `None` when there is nothing to say and no existing note.
pub fn card_note(proposal: &serde_json::Value, existing: Option<&str>) -> Option<String> {
    let bad = unchecked_columns(proposal);
    let list = match bad.as_slice() {
        [] => return existing.map(str::to_string),
        [one] => (*one).to_string(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    };
    let own = format!("Check before pasting: {list} could not be read in the usual form.");
    Some(match existing {
        Some(rest) if !rest.is_empty() => format!("{own}\n\n{rest}"),
        _ => own,
    })
}

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
            crate::actions::table::tsv_from_value(&clamp_receipt(&p)).unwrap(),
            "Date\tMerchant\tAmount\tCurrency\tCategory\r\n\
             2026-10-03\tCafe Nero, Main St\t12.50\tGBP\tMeals"
        );
    }

    #[test]
    fn a_hostile_merchant_name_is_not_a_formula() {
        let p = json!({
            "columns": COLUMNS,
            "rows": [["2026-10-03", "=HYPERLINK(\"http://x\")", "5.00", "USD", "Other"]]
        });
        let csv = crate::actions::table::tsv_from_value(&clamp_receipt(&p)).unwrap();
        assert!(csv.contains("\"'=HYPERLINK("), "{csv}");
    }

    #[test]
    fn no_receipt_is_an_empty_table() {
        let p = clamp_receipt(&json!({"columns": [], "rows": []}));
        assert!(crate::actions::table::is_empty_result("table", &p));
        assert!(crate::actions::table::tsv_from_value(&p).is_none());
        let headers_only = clamp_receipt(&json!({"columns": COLUMNS, "rows": []}));
        assert!(crate::actions::table::is_empty_result(
            "table",
            &headers_only
        ));
    }

    #[test]
    fn two_rows_and_wrong_columns_become_one_row_with_fixed_headers() {
        let p = json!({
            "columns": ["A", "B", "C", "D", "E", "F", "G"],
            "rows": [
                ["2026-10-03", "Cafe", "1.00", "USD", "Meals", "x", "y"],
                ["2026-10-04", "Other", "2.00", "USD", "Meals", "x", "y"]
            ]
        });
        let out = clamp_receipt(&p);
        assert_eq!(out["columns"], json!(COLUMNS));
        assert_eq!(
            out["rows"],
            json!([["2026-10-03", "Cafe", "1.00", "USD", "Meals"]])
        );
    }

    #[test]
    fn a_short_row_is_padded_and_a_non_table_is_left_alone() {
        let out = clamp_receipt(&json!({"columns": ["x"], "rows": [["2026-10-03", "Cafe"]]}));
        assert_eq!(out["rows"], json!([["2026-10-03", "Cafe", "", "", ""]]));
        let junk = json!({"verdict": "hello"});
        assert_eq!(clamp_receipt(&junk), junk);
    }

    fn row(cells: [&str; 5]) -> serde_json::Value {
        json!({ "columns": COLUMNS, "rows": [cells] })
    }

    fn cells(p: &serde_json::Value) -> Vec<String> {
        p["rows"][0]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn a_currency_symbol_and_thousands_commas_are_dropped_from_the_amount() {
        for (raw, want) in [
            ("$12.50", "12.50"),
            ("\u{20ac} 1,234.50", "1234.50"),
            (" 7 ", "7"),
            ("-3.20", "-3.20"),
        ] {
            let out = clamp_receipt(&row(["2026-10-03", "Cafe", raw, "USD", "Meals"]));
            assert_eq!(cells(&out)[2], want, "{raw:?}");
            assert!(unchecked_columns(&out).is_empty(), "{raw:?}");
        }
    }

    #[test]
    fn an_amount_that_is_not_a_number_is_kept_and_flagged() {
        for raw in ["twelve", "12,50", "1.2.3", "12.50 USD"] {
            let out = clamp_receipt(&row(["2026-10-03", "Cafe", raw, "USD", "Meals"]));
            assert_eq!(cells(&out)[2], raw);
            assert_eq!(unchecked_columns(&out), ["Amount"], "{raw:?}");
        }
    }

    #[test]
    fn a_currency_is_a_three_letter_code() {
        let out = clamp_receipt(&row(["2026-10-03", "Cafe", "5", " gbp ", "Meals"]));
        assert_eq!(cells(&out)[3], "GBP");
        let out = clamp_receipt(&row(["2026-10-03", "Cafe", "5", "\u{20ac}", "Meals"]));
        assert_eq!(cells(&out)[3], "EUR");
        assert!(unchecked_columns(&out).is_empty());
        for raw in ["dollars", "$", "US"] {
            let out = clamp_receipt(&row(["2026-10-03", "Cafe", "5", raw, "Meals"]));
            assert_eq!(cells(&out)[3], raw);
            assert_eq!(unchecked_columns(&out), ["Currency"], "{raw:?}");
        }
    }

    #[test]
    fn a_category_outside_the_list_is_kept_and_flagged() {
        let out = clamp_receipt(&row(["2026-10-03", "Cafe", "5", "USD", "meals"]));
        assert_eq!(cells(&out)[4], "Meals");
        let out = clamp_receipt(&row(["2026-10-03", "Cafe", "5", "USD", "Snacks"]));
        assert_eq!(cells(&out)[4], "Snacks");
        assert_eq!(unchecked_columns(&out), ["Category"]);
    }

    #[test]
    fn a_date_is_year_month_day_or_flagged() {
        for (raw, want) in [
            ("2026-10-03", "2026-10-03"),
            ("2026/10/3", "2026-10-03"),
            ("2026.1.30", "2026-01-30"),
            ("2026-10-03 14:22", "2026-10-03"),
        ] {
            let out = clamp_receipt(&row([raw, "Cafe", "5", "USD", "Meals"]));
            assert_eq!(cells(&out)[0], want, "{raw:?}");
            assert!(unchecked_columns(&out).is_empty(), "{raw:?}");
        }
        for raw in [
            "2026-13-01",
            "2026-02-30",
            "yesterday",
            "Oct 32",
            "Oct 3 4 5",
        ] {
            let out = clamp_receipt(&row([raw, "Cafe", "5", "USD", "Meals"]));
            assert_eq!(cells(&out)[0], raw);
            assert_eq!(unchecked_columns(&out), ["Date"], "{raw:?}");
        }
    }

    #[test]
    fn a_date_with_no_year_is_kept_as_printed_and_not_flagged() {
        for raw in ["Oct 3", "3 Oct", "October 3rd", "Oct. 3", "Oct 3,"] {
            let out = clamp_receipt(&row([raw, "Cafe", "5", "USD", "Meals"]));
            assert_eq!(cells(&out)[0], raw);
            assert!(unchecked_columns(&out).is_empty(), "{raw:?}");
        }
    }

    #[test]
    fn a_refund_amount_pastes_as_a_number_not_text() {
        use super::super::table::guard_formula;
        let out = clamp_receipt(&row(["2026-10-03", "Cafe", "-3.20", "USD", "Meals"]));
        assert_eq!(cells(&out)[2], "-3.20");
        assert!(unchecked_columns(&out).is_empty());
        assert_eq!(guard_formula("-3.20"), "-3.20");
        for risky in ["=1+1", "+1+1", "@SUM(A1)", "-1+1", "-cmd"] {
            assert!(guard_formula(risky).starts_with('\''), "{risky:?}");
        }
    }

    #[test]
    fn a_currency_symbol_only_counts_at_the_ends_of_an_amount() {
        for (raw, want) in [("$12", "12"), ("12\u{20ac}", "12"), ("-$3.20", "-3.20")] {
            let out = clamp_receipt(&row(["", "", raw, "", ""]));
            assert_eq!(cells(&out)[2], want, "{raw:?}");
            assert!(unchecked_columns(&out).is_empty(), "{raw:?}");
        }
        for raw in ["1$2", "$$5", "$5$"] {
            let out = clamp_receipt(&row(["", "", raw, "", ""]));
            assert_eq!(cells(&out)[2], raw);
            assert_eq!(unchecked_columns(&out), ["Amount"], "{raw:?}");
        }
    }

    #[test]
    fn empty_cells_are_not_flagged_and_the_note_names_the_columns() {
        let out = clamp_receipt(&row(["", "", "", "", ""]));
        assert!(unchecked_columns(&out).is_empty());
        assert_eq!(card_note(&out, None), None);
        let out = clamp_receipt(&row(["yesterday", "Cafe", "lots", "USD", "Meals"]));
        let note = card_note(&out, Some("Footer.")).unwrap();
        assert!(note.contains("Date and Amount"), "{note}");
        assert!(note.ends_with("Footer."), "{note}");
        assert!(!note.contains('\u{2014}'));
        assert!(unchecked_columns(&json!({"verdict": "x"})).is_empty());
    }
}
