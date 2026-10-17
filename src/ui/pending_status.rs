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
    /// "Fill this form" with no model (#132): reads the form's fields from
    /// the window and matches them to the profile on this PC. No screenshot,
    /// no model, nothing sent.
    ReadingForm,
    /// #551: "Summarize a link" is downloading the page the user confirmed.
    /// Carries the site name (the host being fetched), shown until the
    /// model is asked.
    ReadingPage { host: String },
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

/// Same idea for the site name in the "Reading ..." line (#551).
const MAX_HOST_CHARS: usize = 40;

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

/// Like `truncate` but keeps the END and cuts the front, with a leading
/// "...". A host's registrable domain is at the end, so a long
/// `login.bank.com.evil.example` must still show `evil.example`.
fn truncate_front(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let keep = max_chars.saturating_sub(3);
    let tail: String = s.chars().skip(n - keep).collect();
    format!("...{tail}")
}

/// The status line shown on the pending card (and set as the window's
/// accessible text via `SetWindowTextW`) for a given stage. Pure: no I/O, no
/// Win32.
pub fn pending_status_text(stage: &PendingStage) -> String {
    match stage {
        PendingStage::AskingModel { model_name } => {
            // #817: a plain name ("Claude Haiku 5.5"), not the provider id.
            let plain = crate::ui::palette_model::friendly_model_name(model_name);
            format!("Asking {}...", truncate(&plain, MAX_MODEL_NAME_CHARS))
        }
        PendingStage::ReadingSelection => "Reading your selection...".to_string(),
        PendingStage::ReadingForm => "Reading the form...".to_string(),
        PendingStage::ReadingPage { host } => {
            format!("Reading {}...", truncate_front(host, MAX_HOST_CHARS))
        }
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

/// #551: a stage message posted by a worker is only applied while the run
/// that posted it is still the current one. A run that ended (or was
/// replaced by a newer press) must not relabel the card of the run on
/// screen now. Pure so it is unit tested; the card's own `Pending` guard
/// covers the other half (a result card already showing).
pub fn stage_is_current(posted_for_run: u64, current_run: u64) -> bool {
    posted_for_run == current_run
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reading_page_text_names_the_site_and_no_model() {
        let text = pending_status_text(&PendingStage::ReadingPage {
            host: "example.com".to_string(),
        });
        assert_eq!(text, "Reading example.com...");
        assert!(!text.to_lowercase().contains("asking"));
        assert!(!text.contains('\u{2014}'));
    }

    #[test]
    fn reading_page_text_truncates_a_very_long_host() {
        let text = pending_status_text(&PendingStage::ReadingPage {
            host: format!("{}.evil.example", "a".repeat(47)),
        });
        // 60 chars in: the front is cut, the registrable domain stays.
        assert_eq!(
            text,
            format!("Reading ...{}.evil.example...", "a".repeat(24))
        );
    }

    #[test]
    fn a_stage_from_an_older_run_is_not_current() {
        assert!(stage_is_current(7, 7));
        assert!(!stage_is_current(6, 7));
        assert!(!stage_is_current(8, 7));
    }

    #[test]
    fn reading_selection_text_is_exact_and_does_not_mention_a_screenshot() {
        let text = pending_status_text(&PendingStage::ReadingSelection);
        assert_eq!(text, "Reading your selection...");
        assert!(!text.to_lowercase().contains("screen"));
    }

    #[test]
    fn reading_form_text_names_no_model_and_no_screenshot() {
        let text = pending_status_text(&PendingStage::ReadingForm);
        assert_eq!(text, "Reading the form...");
        assert!(!text.to_lowercase().contains("asking"));
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
            "Asking Claude Haiku..."
        );
    }

    /// #817: the card says "Asking Claude Haiku 5.5...", not the provider id
    /// "anthropic:claude-haiku-5-5..." the config uses.
    #[test]
    fn asking_model_shows_a_plain_name_not_the_provider_id() {
        let text = |name: &str| {
            pending_status_text(&PendingStage::AskingModel {
                model_name: name.to_string(),
            })
        };
        assert_eq!(
            text("anthropic:claude-haiku-5-5"),
            "Asking Claude Haiku 5.5..."
        );
        assert_eq!(text("openai:gpt-5.5"), "Asking GPT-5.5...");
        assert_eq!(text("ollama:gemma3:4b"), "Asking gemma3:4b...");
        assert!(!text("compat:some-self-hosted-model").contains("compat:"));
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
