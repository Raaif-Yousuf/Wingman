//! Issue #354: pure logic for the pending card's status text and its
//! reduce-motion decision. Split out from `card.rs` on purpose -- both
//! functions here are plain data in, data out, so they are unit tested
//! directly with no `HWND`/Win32 involved, while the one-line Win32 query
//! (`client_area_animation_enabled`) that feeds the reduce-motion decision
//! stays in `card.rs` next to the rest of the Win32 surface (there is
//! nothing in a single `SystemParametersInfoW` call worth a test).
//!
//! Card text intentionally never promises a cancel action (issues #324 and
//! #393 are both still open on master, so no working "press the key again
//! to cancel" route exists yet): `StillWorking`'s text names no mechanism.

/// The sub-stage the pending card is in, right after the key is pressed and
/// before an answer or error replaces it.
///
/// There used to be a `Capturing` variant ("Looking at your screen...")
/// meant to be shown while `capture::grab_raw` ran. It never actually
/// painted for a real user: capture happens synchronously on the main
/// thread inside `App::begin_model_action`, entirely BEFORE `show_pending`
/// is even called, so by the time the card exists there is nothing left to
/// "capture" -- the stage was dead code that happened to have a passing
/// unit test (issue #354 follow-up review). Removed rather than fixed in
/// place: every model-backed flow now knows its model label (mode-aware,
/// see `App::first_provider_model_label`) before capture even starts, so
/// `show_pending` can start directly at `AskingModel`, which is both
/// simpler than adding a worker-to-UI `WM_APP_*` message just to relabel a
/// stage nobody sees, and actually honest about what is happening when the
/// card first appears.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingStage {
    /// A model-backed flow (`ask`, `add_event_from_screen`,
    /// `fill_form_from_screen`) is in flight, from the moment the card
    /// appears (the chain that will be tried is already known then, even
    /// though the network request itself starts slightly later, on the
    /// worker thread) until an answer, error, or `StillWorking` replaces
    /// it. Carries the model label shown to the user (e.g.
    /// `"anthropic:claude-haiku"`), already truncated by the caller if it
    /// came from config rather than a literal; empty when no label could be
    /// resolved.
    AskingModel { model_name: String },
    /// `calculate_selection` (#354 follow-up review, finding 3): reads the
    /// current text selection, not a screenshot -- this names that
    /// truthfully instead of reusing a screenshot-flavored line.
    ReadingSelection,
    /// A generic, always-true placeholder for a pending flow that has no
    /// more specific honest text yet (e.g. `extract_text`'s OCR pass).
    Working,
    /// The request has been in flight long enough that the plain "Asking
    /// ..." line stops being reassuring. No action is promised here: see
    /// the module doc comment.
    StillWorking,
}

/// Longest model name shown verbatim in the "Asking ..." line before it is
/// truncated. Chosen so `"Asking " + name + "..."` still fits comfortably in
/// the pending card's width at the default text scale; a name from
/// `providers.toml` is user-supplied and unbounded in principle (issue
/// #169's downscale-limit lookup already treats provider config as
/// untrusted-length input the same way).
const MAX_MODEL_NAME_CHARS: usize = 28;

/// Caps `s` at `max_chars` `char`s. Returns the untouched string when it
/// already fits; otherwise the kept prefix, with no trailing marker of its
/// own -- the single "in progress" `"..."` the caller appends afterwards
/// does double duty as the truncation marker, so a truncated name never
/// grows a second, redundant ellipsis next to it.
fn truncate(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

/// The status line shown on the pending card (and set as the window's
/// accessible text via `SetWindowTextW`) for a given stage. Pure: no I/O, no
/// Win32.
pub fn pending_status_text(stage: &PendingStage) -> String {
    match stage {
        PendingStage::AskingModel { model_name } => {
            format!("Asking {}...", truncate(model_name, MAX_MODEL_NAME_CHARS))
        }
        PendingStage::ReadingSelection => "Reading your selection...".to_string(),
        PendingStage::Working => "Working...".to_string(),
        PendingStage::StillWorking => "Still working.".to_string(),
    }
}

/// Whether the pending glyph should animate (the spinning arc) or render as
/// a single static shape. Takes the already-queried OS preference rather
/// than querying it itself, so it stays a pure function -- the one-line SPI
/// wrapper lives in `card.rs` as `client_area_animation_enabled`.
pub fn pending_glyph_should_spin(animations_enabled: bool) -> bool {
    animations_enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_selection_text_is_exact_and_does_not_mention_a_screenshot() {
        let text = pending_status_text(&PendingStage::ReadingSelection);
        assert_eq!(text, "Reading your selection...");
        assert!(!text.to_lowercase().contains("screen"));
    }

    #[test]
    fn working_text_is_exact() {
        assert_eq!(pending_status_text(&PendingStage::Working), "Working...");
    }

    #[test]
    fn asking_model_text_is_exact_for_a_short_name() {
        assert_eq!(
            pending_status_text(&PendingStage::AskingModel {
                model_name: "claude-haiku".to_string()
            }),
            "Asking claude-haiku..."
        );
    }

    #[test]
    fn still_working_text_is_exact_and_names_no_cancel_mechanism() {
        let text = pending_status_text(&PendingStage::StillWorking);
        assert_eq!(text, "Still working.");
        // Issue #354's binding constraint: #324/#393 are both still open, so
        // this line must not promise a cancel action that does not exist on
        // master yet.
        assert!(!text.to_lowercase().contains("press"));
        assert!(!text.to_lowercase().contains("cancel"));
        assert!(!text.to_lowercase().contains("key again"));
    }

    #[test]
    fn asking_model_truncates_a_long_model_name() {
        let long_name = "a".repeat(60);
        let text = pending_status_text(&PendingStage::AskingModel {
            model_name: long_name,
        });
        assert_eq!(
            text,
            format!("Asking {}...", "a".repeat(MAX_MODEL_NAME_CHARS))
        );
        // Never two ellipses back to back.
        assert!(!text.contains("......"));
        assert!(text.chars().count() < 60 + "Asking ...".len());
    }

    #[test]
    fn asking_model_boundary_exactly_at_the_limit_is_not_truncated() {
        let name = "a".repeat(MAX_MODEL_NAME_CHARS);
        let text = pending_status_text(&PendingStage::AskingModel {
            model_name: name.clone(),
        });
        assert_eq!(text, format!("Asking {name}..."));
        assert!(!text.contains("......"));
    }

    #[test]
    fn asking_model_boundary_one_over_the_limit_is_truncated() {
        let name = "a".repeat(MAX_MODEL_NAME_CHARS + 1);
        let text = pending_status_text(&PendingStage::AskingModel { model_name: name });
        assert_eq!(
            text,
            format!("Asking {}...", "a".repeat(MAX_MODEL_NAME_CHARS))
        );
        assert!(!text.contains("......"));
    }

    #[test]
    fn asking_model_empty_name_is_not_truncated() {
        let text = pending_status_text(&PendingStage::AskingModel {
            model_name: String::new(),
        });
        assert_eq!(text, "Asking ...");
    }

    #[test]
    fn spin_decision_follows_the_animation_setting() {
        assert!(pending_glyph_should_spin(true));
        assert!(!pending_glyph_should_spin(false));
    }
}
