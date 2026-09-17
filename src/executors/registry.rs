//! The executor registry (#31): the one place `Action.executor` (a plain
//! `String`, per the action-model design doc) turns into a real
//! [`super::Executor`]. A `match`, not a `HashMap`/`once_cell` lookup table:
//! with two entries, a data structure buys nothing a `match` doesn't already
//! give for free, and it keeps "is this name known" and "what to build for
//! it" in the same place a reviewer reads top to bottom.

use super::calendar_add::CalendarAddExecutor;
use super::clipboard::ClipboardExecutor;
use super::fill_form::FillFormExecutor;
use super::image_clipboard::ImageClipboardExecutor;
use super::none::NoneExecutor;
use super::replace_text::ReplaceTextExecutor;
use super::Executor;

/// Resolves an executor by name, or fails with the exact text an
/// `actions.toml` naming an unknown executor surfaces on an error card
/// (CLAUDE.md rule 7: a load error, not a panic; rule 11: no em dash).
// Unused outside its own test and `actions::resolve_executor` (also
// `#[allow(dead_code)]`) until the confirm-card issue calls a resolved
// executor for real -- see `executors::mod`'s `Effect` doc comment.
#[allow(dead_code)]
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Executor>> {
    match name {
        "none" => Ok(Box::new(NoneExecutor)),
        "clipboard" => Ok(Box::new(ClipboardExecutor::new())),
        "calendar_add" => Ok(Box::new(CalendarAddExecutor::new())),
        "image_clipboard" => Ok(Box::new(ImageClipboardExecutor::new())),
        "replace_text" => Ok(Box::new(ReplaceTextExecutor::new())),
        "fill_form" => Ok(Box::new(FillFormExecutor::new())),
        _ => anyhow::bail!(
            "No executor named \"{name}\". Check the action's executor field in actions.toml."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_none_by_name() {
        let executor = resolve("none").expect("\"none\" is a built-in executor");
        assert_eq!(executor.name(), "none");
    }

    #[test]
    fn resolves_clipboard_by_name() {
        let executor = resolve("clipboard").expect("\"clipboard\" is a built-in executor");
        assert_eq!(executor.name(), "clipboard");
    }

    #[test]
    fn resolves_calendar_add_by_name() {
        let executor = resolve("calendar_add").expect("\"calendar_add\" is a built-in executor");
        assert_eq!(executor.name(), "calendar_add");
        assert_eq!(executor.effect(), super::super::Effect::Writes);
    }

    #[test]
    fn resolves_image_clipboard_by_name() {
        let executor =
            resolve("image_clipboard").expect("\"image_clipboard\" is a built-in executor");
        assert_eq!(executor.name(), "image_clipboard");
        assert_eq!(executor.effect(), super::super::Effect::ReadOnly);
    }

    #[test]
    fn resolves_replace_text_by_name() {
        let executor = resolve("replace_text").expect("\"replace_text\" is a built-in executor");
        assert_eq!(executor.name(), "replace_text");
        assert_eq!(executor.effect(), super::super::Effect::Writes);
    }

    #[test]
    fn resolves_fill_form_by_name() {
        let executor = resolve("fill_form").expect("\"fill_form\" is a built-in executor");
        assert_eq!(executor.name(), "fill_form");
        assert_eq!(executor.effect(), super::super::Effect::Writes);
    }

    #[test]
    fn unknown_name_is_an_error_naming_it_with_no_em_dash() {
        let err = resolve("does-not-exist")
            .err()
            .expect("an unknown executor name must be an error");
        let msg = err.to_string();
        assert!(
            msg.contains("does-not-exist"),
            "error should name the unknown executor: {msg}"
        );
        assert!(
            msg.contains("actions.toml"),
            "error should point at where to fix it: {msg}"
        );
        assert!(
            !msg.contains('\u{2014}'),
            "no em dashes in card-facing text (rule 11): {msg}"
        );
    }
}
