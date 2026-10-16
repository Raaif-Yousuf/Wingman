//! "Table to CSV" (#69): read a table off the screen and put it on the
//! clipboard as tab-separated text, which a spreadsheet pastes into columns (#708).
//!
//! The model fills `columns` (the header names) and then `rows` (each row a
//! list of cell strings). The preview card lists the rows; "Do it" runs the
//! `"clipboard"` executor, which turns the proposal into tab-separated text with
//! [`tsv_from_value`]. The quoting is done in Rust, never trusted to the
//! model.
//!
//! Decision (#708): tab-separated text only, in the one text format on the
//! clipboard. Excel and Sheets split pasted text on tabs, so CSV landed in one
//! column. A second CSV clipboard format would need raw clipboard code for a
//! rarer target; the action keeps its name and id.
//!
//! [`Table`] and its [`tsv`](Table::tsv) are the shared piece: a receipt or a
//! chart read into the same `table` proposal (#70, #114) gets the same quoting
//! and the same spreadsheet-formula guard for free.

use serde_json::Value;

use super::{Action, InputKind, Prefer};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "table-to-csv";

/// The proposal kind this module reads (`actions::schema`'s `table`).
pub const PROPOSAL_KIND: &str = "table";

/// The most rows and columns kept from a model's table. Real screen tables
/// are far smaller; the caps stop a runaway reply from exhausting memory.
pub const MAX_ROWS: usize = 5000;
pub const MAX_COLUMNS: usize = 200;

/// The note on the preview card when the table was cut to the caps.
pub const CAPPED_NOTE: &str =
    "The table was too big, so only the first 5000 rows and 200 columns are kept.";

/// What the card says when the model's reply is not a table it can read.
pub const NOT_READ_HEADLINE: &str = "The table could not be read";

/// What the card says when the screen holds no table.
pub const NO_TABLE_HEADLINE: &str = "No table found";

/// The action's prompt. Copying the cells as shown is the load-bearing part:
/// a model that "tidies" numbers or fills gaps corrupts the data silently.
pub const BASE_PROMPT: &str = "You are shown a table (as text, or as a screenshot if no text was selected). Find the main table. First fill `columns` with its column headers in order, copied as shown; if the table has no header row, use \"Column 1\", \"Column 2\" and so on. Then fill `rows` with one list of cells per table row, in order, left to right, each cell as a plain string copied exactly as shown: keep the numbers, currency symbols, percent signs and units as written, and do not reformat, round, sum or correct anything. Use an empty string for an empty cell, so every row has as many cells as there are columns. Do not include the header row in `rows`, and do not include total rows unless the table shows them. If a cell has several lines, join them with a space. If there is no table, return empty lists. Use plain text only: no markdown and no LaTeX.";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `table`, executor `clipboard`, `confirm = true`
/// so the user sees the rows before they are copied.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Table to CSV".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
        proposal: PROPOSAL_KIND.to_string(),
        executor: "clipboard".to_string(),
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

/// A table read off the screen: header names and rows of cell text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
    /// True when rows or columns past the caps were dropped.
    pub capped: bool,
}

impl Table {
    /// Reads a `table` proposal (`columns`, `rows`). A number or bool cell is
    /// written as text, so a model that returns `12` instead of `"12"` still
    /// works. `None` when there is no `columns` or `rows` list, or a row is
    /// not a list (the table could not be read). Past [`MAX_ROWS`] rows or
    /// [`MAX_COLUMNS`] columns the extra is dropped and `capped` is set: a
    /// model that runs away must not exhaust memory in the tray process.
    pub fn from_value(proposal: &Value) -> Option<Table> {
        let columns_in = proposal.get("columns")?.as_array()?;
        let rows_in = proposal.get("rows")?.as_array()?;
        let mut capped = columns_in.len() > MAX_COLUMNS || rows_in.len() > MAX_ROWS;
        let columns = columns_in.iter().take(MAX_COLUMNS).map(cell_text).collect();
        let mut rows = Vec::with_capacity(rows_in.len().min(MAX_ROWS));
        for row in rows_in.iter().take(MAX_ROWS) {
            let cells = row.as_array()?;
            capped |= cells.len() > MAX_COLUMNS;
            rows.push(cells.iter().take(MAX_COLUMNS).map(cell_text).collect());
        }
        Some(Table {
            columns,
            rows,
            capped,
        })
    }

    /// The table as a `table` proposal again (what the preview card shows
    /// when the table was capped, so the card matches what is copied).
    pub fn to_value(&self) -> Value {
        serde_json::json!({"columns": self.columns, "rows": self.rows})
    }

    /// True when there is nothing to copy: no rows, or every cell blank.
    pub fn is_empty(&self) -> bool {
        self.rows.iter().flatten().all(|c| c.trim().is_empty())
    }

    /// The table as tab-separated text: a header line, then one line per row,
    /// CRLF line endings, no trailing newline. Every line has as many cells
    /// as the widest row (at most [`MAX_COLUMNS`]), short rows padded with
    /// empty cells.
    pub fn tsv(&self) -> String {
        let width = self
            .rows
            .iter()
            .map(Vec::len)
            .chain([self.columns.len()])
            .max()
            .unwrap_or(0);
        let line = |cells: &[String]| {
            (0..width)
                .map(|i| tsv_field(cells.get(i).map_or("", String::as_str)))
                .collect::<Vec<_>>()
                .join("\t")
        };
        std::iter::once(line(&self.columns))
            .chain(self.rows.iter().map(|r| line(r)))
            .collect::<Vec<_>>()
            .join("\r\n")
    }
}

fn cell_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// One tab-separated field: made safe for a spreadsheet first (see
/// [`guard_formula`]), then quoted when it holds a tab, quote, CR or LF
/// (quotes doubled). A comma needs no quoting here, which is the point: a
/// spreadsheet splits pasted text on tabs, and CSV text pasted into Excel
/// lands in one column (#708).
pub fn tsv_field(cell: &str) -> String {
    let safe = guard_formula(cell);
    if safe.contains(['\t', '"', '\r', '\n']) {
        format!("\"{}\"", safe.replace('"', "\"\""))
    } else {
        safe
    }
}

/// Spreadsheet formula guard. Excel and Sheets run a cell that starts with
/// `=`, `+`, `-` or `@` (or a tab or CR) as a formula, so a table read off a
/// hostile page could run one when pasted. Leading spaces do not stop it, so
/// the check looks past them. Such a cell gets a leading apostrophe (before
/// the original text), which a spreadsheet shows as plain text. The
/// exception is a number: `-5`, `+3.5`, `-1,234.50`, `-12%`, `-$5` and `-1e3`
/// are data, not formulas, and stay as they are.
pub fn guard_formula(cell: &str) -> String {
    let trimmed = cell.trim_start();
    let risky = cell.starts_with(['\t', '\r']) || trimmed.starts_with(['=', '+', '-', '@']);
    if risky && !is_signed_number(trimmed) {
        format!("'{cell}")
    } else {
        cell.to_string()
    }
}

/// A sign, an optional currency symbol, then digits with `.` and `,`, an
/// optional exponent (`e3`, `E-3`) or a trailing `%`, and at least one digit.
/// Nothing else: any other character makes it text that gets the guard.
fn is_signed_number(cell: &str) -> bool {
    let Some(rest) = cell.strip_prefix(['+', '-']) else {
        return false;
    };
    let rest = rest
        .strip_prefix(['$', '\u{20ac}', '\u{a3}', '\u{a5}'])
        .unwrap_or(rest);
    let rest = rest.strip_suffix('%').unwrap_or(rest);
    let (mantissa, exponent) = match rest.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e)),
        None => (rest, None),
    };
    let digits = |s: &str| !s.is_empty() && s.chars().all(|c| c.is_ascii_digit());
    let exponent_ok = match exponent {
        None => true,
        Some(e) => digits(e.strip_prefix(['+', '-']).unwrap_or(e)),
    };
    mantissa.chars().any(|c| c.is_ascii_digit())
        && mantissa
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
        && exponent_ok
}

/// The tab-separated text for a confirmed `table` proposal, `None` when it has no table or
/// the table is empty.
pub fn tsv_from_value(proposal: &Value) -> Option<String> {
    let table = Table::from_value(proposal)?;
    (!table.is_empty()).then(|| table.tsv())
}

/// Whether a model proposal for the `table` kind has nothing to copy: it
/// reads as a table with no cells. Always false for any other proposal kind.
pub fn is_empty_result(proposal_kind: &str, proposal: &Value) -> bool {
    proposal_kind == PROPOSAL_KIND && Table::from_value(proposal).is_some_and(|t| t.is_empty())
}

/// Whether a model proposal for the `table` kind is not a table at all (a
/// missing list, or a row that is not a list). Always false for any other
/// proposal kind.
pub fn is_unreadable(proposal_kind: &str, proposal: &Value) -> bool {
    proposal_kind == PROPOSAL_KIND && Table::from_value(proposal).is_none()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn table(columns: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|s| s.to_string()).collect())
                .collect(),
            capped: false,
        }
    }

    #[test]
    fn a_plain_table_has_the_right_columns() {
        let t = table(
            &["Item", "Qty", "Price"],
            &[&["Pen", "2", "1.50"], &["Ink", "1", "7.25"]],
        );
        assert_eq!(t.tsv(), "Item\tQty\tPrice\r\nPen\t2\t1.50\r\nInk\t1\t7.25");
    }

    #[test]
    fn tabs_quotes_and_newlines_are_quoted_but_commas_are_not() {
        assert_eq!(tsv_field("a,b"), "a,b");
        assert_eq!(tsv_field("a\tb"), "\"a\tb\"");
        assert_eq!(tsv_field("say \"hi\""), "\"say \"\"hi\"\"\"");
        assert_eq!(tsv_field("one\ntwo"), "\"one\ntwo\"");
        assert_eq!(tsv_field("one\r\ntwo"), "\"one\r\ntwo\"");
        assert_eq!(tsv_field("plain"), "plain");
        assert_eq!(tsv_field(""), "");
    }

    #[test]
    fn short_rows_are_padded_and_long_rows_are_kept() {
        let t = table(&["A", "B"], &[&["1"], &["1", "2", "3"]]);
        assert_eq!(t.tsv(), "A\tB\t\r\n1\t\t\r\n1\t2\t3");
    }

    #[test]
    fn formula_cells_get_an_apostrophe() {
        assert_eq!(guard_formula("=1+1"), "'=1+1");
        assert_eq!(guard_formula("@SUM(A1)"), "'@SUM(A1)");
        assert_eq!(guard_formula("+cmd|' /C calc'!A0"), "'+cmd|' /C calc'!A0");
        assert_eq!(guard_formula("-2+3"), "'-2+3");
        assert_eq!(guard_formula("\t=1"), "'\t=1");
        assert_eq!(guard_formula("\r=1"), "'\r=1");
        assert_eq!(guard_formula("-A1"), "'-A1");
        // A bare sign is not a number.
        assert_eq!(guard_formula("-"), "'-");
        assert_eq!(guard_formula("+"), "'+");
        // The guard runs before the quoting.
        assert_eq!(
            tsv_field("=HYPERLINK(\"x\",\"y\")"),
            "\"'=HYPERLINK(\"\"x\"\",\"\"y\"\")\""
        );
        // No quote or tab in the cell: the guard alone, no quoting.
        assert_eq!(tsv_field("=1+1"), "'=1+1");
    }

    #[test]
    fn numbers_and_ordinary_text_are_left_alone() {
        for ok in [
            "-5",
            "+3.5",
            "-1,234.50",
            "-12%",
            "+1",
            "Pen",
            "a=b",
            "x-y",
            "5-3",
            "",
            " ",
        ] {
            assert_eq!(guard_formula(ok), ok, "{ok:?} must not change");
        }
    }

    #[test]
    fn the_header_row_is_guarded_too() {
        let t = table(&["=evil", "ok"], &[&["1", "2"]]);
        assert_eq!(t.tsv(), "'=evil\tok\r\n1\t2");
    }

    #[test]
    fn a_proposal_becomes_tab_separated_text() {
        let p = json!({"columns": ["Name", "Note"], "rows": [["Ann", "hi, there"], ["Bo", 12]]});
        assert_eq!(
            tsv_from_value(&p).unwrap(),
            "Name\tNote\r\nAnn\thi, there\r\nBo\t12"
        );
    }

    #[test]
    fn nothing_to_copy_is_none() {
        assert!(tsv_from_value(&json!({"columns": [], "rows": []})).is_none());
        assert!(tsv_from_value(&json!({"columns": ["A"], "rows": [[""]]})).is_none());
        assert!(tsv_from_value(&json!({"rows": [["a"]]})).is_none());
        assert!(tsv_from_value(&json!({"columns": ["A"], "rows": "x"})).is_none());
        assert!(tsv_from_value(&json!({"columns": ["A"], "rows": ["x"]})).is_none());
        assert!(is_empty_result(
            "table",
            &json!({"columns": [], "rows": []})
        ));
        assert!(!is_empty_result("text_answer", &json!({})));
        assert!(!is_empty_result(
            "table",
            &json!({"columns": ["A"], "rows": [["1"]]})
        ));
    }

    #[test]
    fn an_unreadable_reply_is_told_apart_from_an_empty_table() {
        let empty = json!({"columns": [], "rows": []});
        let bad_row = json!({"columns": ["A"], "rows": ["x"]});
        let no_rows = json!({"columns": ["A"]});
        assert!(is_empty_result("table", &empty) && !is_unreadable("table", &empty));
        assert!(is_unreadable("table", &bad_row) && !is_empty_result("table", &bad_row));
        assert!(is_unreadable("table", &no_rows));
        assert!(!is_unreadable("text_answer", &bad_row));
    }

    #[test]
    fn a_runaway_table_is_cut_to_the_caps_and_says_so() {
        let row: Vec<Value> = (0..MAX_COLUMNS + 5).map(|i| json!(i.to_string())).collect();
        let rows: Vec<Value> = (0..MAX_ROWS + 10).map(|_| json!(row)).collect();
        let t = Table::from_value(&json!({"columns": row, "rows": rows})).unwrap();
        assert!(t.capped);
        assert_eq!(t.rows.len(), MAX_ROWS);
        assert_eq!(t.columns.len(), MAX_COLUMNS);
        assert!(t.rows.iter().all(|r| r.len() == MAX_COLUMNS));
        // The card value matches what is copied.
        assert_eq!(Table::from_value(&t.to_value()).unwrap().rows, t.rows);
        let small = Table::from_value(&json!({"columns": ["A"], "rows": [["1"]]})).unwrap();
        assert!(!small.capped);
    }

    #[test]
    fn leading_spaces_do_not_hide_a_formula() {
        assert_eq!(guard_formula(" =1+1"), "' =1+1");
        assert_eq!(guard_formula("  @SUM(A1)"), "'  @SUM(A1)");
        assert_eq!(guard_formula(" -5"), " -5");
    }

    #[test]
    fn currency_and_exponent_numbers_stay_but_lookalikes_are_guarded() {
        for ok in ["-$5", "+\u{20ac}1,200.50", "-1e3", "-1.5E-3", "-$1,000%"] {
            assert_eq!(guard_formula(ok), ok, "{ok:?}");
        }
        for bad in [
            "-$", "-e3", "-1e", "-1e+", "-$5+2", "-1e3+1", "-1e3e4", "-$A1",
        ] {
            assert_eq!(guard_formula(bad), format!("'{bad}"), "{bad:?}");
        }
    }

    #[test]
    fn the_entry_confirms_then_copies_a_table() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.proposal, "table");
        assert_eq!(a.executor, "clipboard");
        assert!(a.confirm);
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert!(!a.prompt.contains('\u{2014}'), "no em dashes in the prompt");
    }
}
