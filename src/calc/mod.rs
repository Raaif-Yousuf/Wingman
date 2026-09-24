//! Issue #115: calculator and unit conversion without a model. Selection in,
//! local evaluation, card out -- works offline, with no provider configured,
//! and stays blocked while paused (the same rule every model-backed action
//! follows, even though this one never touches the network).
//!
//! Two pure evaluators, tried in a fixed order:
//! 1. [`units::parse_conversion_query`]: does the text look like
//!    `"<number> <unit> in|to <unit>"`? If so, the whole input is committed
//!    to being a conversion query -- an unrecognized unit is reported as
//!    such, never silently re-tried as arithmetic (see that function's doc
//!    comment for why).
//! 2. Otherwise [`expr::evaluate`]: a bounded recursive-descent arithmetic
//!    expression.
//!
//! [`evaluate`] is the single entry point both the pipeline below and
//! `app.rs`'s wiring call. [`run_on_selection`] adds the one Win32-touching
//! step (reading the current selection via `inputs::selection`) behind an
//! injectable trait, so the offline-evaluation logic above stays testable
//! with no selection/clipboard/UIA involved at all (AGENTS.md rule 8/9).

pub mod expr;
pub mod units;

use std::fmt;

/// Shared with [`expr`]'s tokenizer: the whole input (not just one number
/// literal) is rejected past this length before any parsing starts. 500
/// characters is far beyond any plausible pasted formula or "<number> <unit>
/// in <unit>" query, and short enough that even a pathological worst-case
/// input costs nothing to reject.
pub const MAX_INPUT_LEN: usize = 500;

#[derive(Debug, Clone, PartialEq)]
pub enum CalcError {
    Empty,
    Expr(expr::ExprError),
    Unit(units::UnitError),
}

impl fmt::Display for CalcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CalcError::Empty => write!(f, "Nothing to calculate."),
            CalcError::Expr(e) => write!(f, "{e}"),
            CalcError::Unit(e) => write!(f, "{e}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum CalcResult {
    /// `input` is the trimmed original text, echoed verbatim in
    /// [`CalcResult::headline`] -- never reformatted, so `"(12*7)/4"` stays
    /// `"(12*7)/4"` rather than becoming `"(12 * 7) / 4"` or similar.
    Expression { input: String, value: f64 },
    Conversion {
        value: f64,
        from_unit: String,
        to_value: f64,
        to_unit: String,
    },
}

impl CalcResult {
    /// The one line a card's `headline` shows, e.g. `"(12*7)/4 = 21"` or
    /// `"3.5 mi = 5.633 km"` (the task brief's own two examples,
    /// word-for-word).
    pub fn headline(&self) -> String {
        match self {
            CalcResult::Expression { input, value } => {
                format!("{input} = {}", format_significant(*value, 6))
            }
            CalcResult::Conversion {
                value,
                from_unit,
                to_value,
                to_unit,
            } => format!(
                "{} {from_unit} = {} {to_unit}",
                format_significant(*value, 4),
                format_significant(*to_value, 4)
            ),
        }
    }
}

/// Evaluates `input` as either a unit-conversion query or an arithmetic
/// expression (in that order -- see the module doc comment). The one
/// network-free, selection-free entry point; [`run_on_selection`] is the
/// thin wrapper that feeds it real selected text.
pub fn evaluate(input: &str) -> Result<CalcResult, CalcError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(CalcError::Empty);
    }
    if trimmed.len() > MAX_INPUT_LEN {
        return Err(CalcError::Expr(expr::ExprError::TooLong));
    }

    if let Some(parsed) = units::parse_conversion_query(trimmed) {
        let to_value =
            units::convert(parsed.value, &parsed.from, &parsed.to).map_err(CalcError::Unit)?;
        return Ok(CalcResult::Conversion {
            value: parsed.value,
            from_unit: parsed.from,
            to_value,
            to_unit: parsed.to,
        });
    }

    let value = expr::evaluate(trimmed).map_err(CalcError::Expr)?;
    Ok(CalcResult::Expression {
        input: trimmed.to_string(),
        value,
    })
}

/// Formats `x` to `sig` significant figures: an (effectively) integer value
/// is shown with no decimal point at all (`21.0` -> `"21"`, not
/// `"21.000"`), and every other value is rounded to `sig` significant digits
/// with trailing zeros trimmed (`3.5` at 4 sig figs stays `"3.5"`, not
/// `"3.500"`). Never panics: `x` is always finite here, since both
/// evaluators (`expr::evaluate`, `units::convert`) already reject
/// non-finite results before a [`CalcResult`] is ever built.
fn format_significant(x: f64, sig: usize) -> String {
    if x == 0.0 {
        return "0".to_string();
    }
    let sign = if x.is_sign_negative() { "-" } else { "" };
    let ax = x.abs();

    // Integers (within float noise) print with no decimal point.
    if ax < 1e15 && (ax - ax.round()).abs() < 1e-9 * ax.max(1.0) {
        return format!("{sign}{}", ax.round() as i64);
    }

    let magnitude = ax.log10().floor() as i32;
    let decimals = (sig as i32 - 1 - magnitude).max(0) as usize;
    let mut s = format!("{ax:.decimals$}");
    if s.contains('.') {
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    format!("{sign}{s}")
}

/// Seam between [`run_on_selection`] and the real selected text, so the
/// pipeline (trim/empty-check/evaluate/format) is exercised with an
/// injectable string instead of the real UIA/clipboard round trip
/// (`inputs::selection::get_selection_foreground`) -- AGENTS.md rule 8/9:
/// Win32 stays out of this module's own tests.
pub trait SelectionSource {
    /// Returns the selected text, or `Ok(None)` for "nothing selected"
    /// (`inputs::selection::SelectionSource::Empty`/`SkippedPasswordField`).
    fn selected_text(&self) -> anyhow::Result<Option<String>>;
}

/// The real production seam: `inputs::selection::get_selection_foreground`,
/// with the module's own default bound/budget. Must be called from a
/// worker thread, same requirement as `get_selection_foreground` itself
/// (see that function's doc comment) -- `run_on_selection` inherits this
/// requirement and does not enforce it itself.
pub struct ForegroundSelection;

impl SelectionSource for ForegroundSelection {
    fn selected_text(&self) -> anyhow::Result<Option<String>> {
        use crate::inputs::selection::{get_selection_foreground, SelectionSource as Src};
        let selection = get_selection_foreground(
            crate::inputs::selection::DEFAULT_MAX_CHARS,
            crate::inputs::selection::DEFAULT_CLIPBOARD_WAIT_BUDGET,
        )?;
        match selection.source {
            Src::Empty | Src::SkippedPasswordField => Ok(None),
            Src::Uia | Src::ClipboardFallback => Ok(Some(selection.text)),
        }
    }
}

/// What the tray item / hotkey / palette entry (once #25 exists) actually
/// shows: either the card headline text, or `None` for "there is nothing to
/// evaluate" -- a distinct outcome from a real [`CalcError`], so the caller
/// can word an empty selection differently from a malformed expression.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectionCalcOutcome {
    /// `(headline, detail)` -- `detail` is always empty today (no
    /// intermediate steps are shown), kept as a pair so a caller building an
    /// `Answer`-shaped card doesn't need a third case.
    Result {
        headline: String,
    },
    NoSelection,
    Error(CalcError),
}

/// The full pipeline: read the current selection, then evaluate it.
/// Pure logic ([`evaluate`], [`format_significant`]) is exercised directly
/// by this module's other tests; this function's own tests use a fake
/// [`SelectionSource`], so no real UIA/clipboard round trip runs in
/// automated tests (AGENTS.md rule 9).
pub fn run_on_selection(source: &impl SelectionSource) -> SelectionCalcOutcome {
    let text = match source.selected_text() {
        Ok(Some(text)) if !text.trim().is_empty() => text,
        Ok(_) => return SelectionCalcOutcome::NoSelection,
        Err(_) => return SelectionCalcOutcome::NoSelection,
    };
    match evaluate(&text) {
        Ok(result) => SelectionCalcOutcome::Result {
            headline: result.headline(),
        },
        Err(e) => SelectionCalcOutcome::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- evaluate: routes to conversion vs. expression -----------------------

    #[test]
    fn conversion_query_routes_to_units() {
        let result = evaluate("3.5 mi in km").unwrap();
        assert!(matches!(result, CalcResult::Conversion { .. }));
        assert_eq!(result.headline(), "3.5 mi = 5.633 km");
    }

    #[test]
    fn arithmetic_routes_to_expr() {
        let result = evaluate("(12*7)/4").unwrap();
        assert!(matches!(result, CalcResult::Expression { .. }));
        assert_eq!(result.headline(), "(12*7)/4 = 21");
    }

    #[test]
    fn unrecognized_unit_is_a_clear_message_not_a_fallback_to_arithmetic() {
        let err = evaluate("3.5 bananas in km").unwrap_err();
        assert!(matches!(
            err,
            CalcError::Unit(units::UnitError::UnknownUnit(_))
        ));
        assert!(err.to_string().contains("bananas"));
    }

    #[test]
    fn empty_input_is_a_clear_error() {
        assert_eq!(evaluate(""), Err(CalcError::Empty));
        assert_eq!(evaluate("   "), Err(CalcError::Empty));
    }

    #[test]
    fn too_long_input_is_rejected_before_parsing() {
        let long = "1".repeat(MAX_INPUT_LEN + 1);
        assert!(matches!(
            evaluate(&long),
            Err(CalcError::Expr(expr::ExprError::TooLong))
        ));
    }

    #[test]
    fn arithmetic_error_propagates_with_its_own_message() {
        let err = evaluate("1/0").unwrap_err();
        assert!(matches!(
            err,
            CalcError::Expr(expr::ExprError::DivisionByZero)
        ));
    }

    #[test]
    fn incompatible_units_propagate_with_a_clear_message() {
        let err = evaluate("5 km to kg").unwrap_err();
        assert!(matches!(
            err,
            CalcError::Unit(units::UnitError::IncompatibleDimensions { .. })
        ));
    }

    // -- format_significant ---------------------------------------------------

    #[test]
    fn table_of_formatted_numbers() {
        let cases: &[(f64, usize, &str)] = &[
            (21.0, 6, "21"),
            (0.0, 6, "0"),
            (-21.0, 6, "-21"),
            (3.5, 4, "3.5"),
            (5.632704, 4, "5.633"),
            (0.1, 4, "0.1"),
            (1234.5678, 6, "1234.57"),
            (0.00012345, 4, "0.0001234"), // 4 sig figs into a small magnitude
            (1000000.0, 4, "1000000"),    // integer path, not scientific
        ];
        for (x, sig, expected) in cases {
            assert_eq!(format_significant(*x, *sig), *expected, "x={x} sig={sig}");
        }
    }

    // -- headline formatting matches the task brief's own examples -----------

    #[test]
    fn headline_examples_from_the_task_brief() {
        assert_eq!(evaluate("(12*7)/4").unwrap().headline(), "(12*7)/4 = 21");
        assert_eq!(
            evaluate("3.5 mi in km").unwrap().headline(),
            "3.5 mi = 5.633 km"
        );
    }

    // -- run_on_selection, against a fake SelectionSource ---------------------

    struct FakeSelection(anyhow::Result<Option<String>>);

    impl SelectionSource for FakeSelection {
        fn selected_text(&self) -> anyhow::Result<Option<String>> {
            match &self.0 {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    #[test]
    fn pipeline_evaluates_the_selected_text() {
        let source = FakeSelection(Ok(Some("2+2".to_string())));
        let outcome = run_on_selection(&source);
        assert_eq!(
            outcome,
            SelectionCalcOutcome::Result {
                headline: "2+2 = 4".to_string()
            }
        );
    }

    #[test]
    fn pipeline_reports_no_selection_for_empty_text() {
        let source = FakeSelection(Ok(Some("   ".to_string())));
        assert_eq!(run_on_selection(&source), SelectionCalcOutcome::NoSelection);
    }

    #[test]
    fn pipeline_reports_no_selection_for_none() {
        let source = FakeSelection(Ok(None));
        assert_eq!(run_on_selection(&source), SelectionCalcOutcome::NoSelection);
    }

    #[test]
    fn pipeline_treats_a_selection_error_as_no_selection_not_a_crash() {
        // Matches the rest of the crate's "a UIA/clipboard hiccup degrades
        // gracefully" pattern (see `inputs::selection`'s own doc comment) --
        // never a panic, never a card that can't explain itself.
        let source = FakeSelection(Err(anyhow::anyhow!("UIA probe failed")));
        assert_eq!(run_on_selection(&source), SelectionCalcOutcome::NoSelection);
    }

    #[test]
    fn pipeline_surfaces_a_calc_error_for_malformed_selected_text() {
        let source = FakeSelection(Ok(Some("1/0".to_string())));
        let outcome = run_on_selection(&source);
        assert_eq!(
            outcome,
            SelectionCalcOutcome::Error(CalcError::Expr(expr::ExprError::DivisionByZero))
        );
    }
}
