//! The `"calendar_add"` executor (#34): consumes a confirmed
//! `calendar_event` proposal, writes it through a connector (`"ics"` today,
//! the only one #35 implements), and returns an honest [`Undo`]. See the
//! connector design doc's `calendar_add executor` section for the type
//! choices this file makes and why (`Effect::Writes`, not a new `Effect`
//! variant; parses to `CalendarEvent` on the first line of `execute` and
//! never touches the raw JSON again).

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::connectors::{self, CalendarEvent, Connector};
use crate::ui::confirm::Confirmed;

use super::{Effect, Executor, Undo};

/// `Effect::Writes` (not read-only): a calendar_add always requires the
/// preview card's "Do it" confirmation (expansion plan §6, "Confirm";
/// executor design doc, `auto_confirm_read_only` refuses anything that is
/// not `Effect::ReadOnly`) -- there is no path from a proposal to a written
/// `.ics` file without a real user confirmation.
pub struct CalendarAddExecutor {
    connector: Box<dyn Connector>,
}

impl CalendarAddExecutor {
    /// Production constructor: resolves the built-in `"ics"` connector.
    /// `expect` is safe here -- `"ics"` is always registered (see
    /// `connectors::registry::resolve`'s own test); this is the same
    /// "unreachable in practice, still named" shape as an internal
    /// invariant, not a caller-facing fallible path.
    pub fn new() -> Self {
        Self {
            connector: connectors::registry::resolve("ics")
                .expect("\"ics\" is a built-in connector"),
        }
    }

    /// Test/forward-wiring constructor: an explicit connector, so a test
    /// can inject an `IcsConnector` wired to a fake `Opener` (or, once
    /// settings gains a "calendar connector" choice, a real alternate
    /// connector) instead of always resolving the fixed `"ics"` default.
    /// Unused outside this file's tests until settings gains that choice.
    #[allow(dead_code)]
    pub fn with_connector(connector: Box<dyn Connector>) -> Self {
        Self { connector }
    }
}

impl Default for CalendarAddExecutor {
    fn default() -> Self {
        Self::new()
    }
}

impl Executor for CalendarAddExecutor {
    fn name(&self) -> &'static str {
        "calendar_add"
    }

    fn effect(&self) -> Effect {
        Effect::Writes
    }

    fn execute(&self, confirmed: Confirmed<Value>) -> Result<Undo> {
        // The only place this executor reads `Value`: everything past this
        // line acts on the typed `CalendarEvent`, never the raw JSON again
        // (connector design doc: this is what actually gives the "consumes
        // Confirmed<CalendarEvent> only" guarantee, since the `Executor`
        // trait itself is fixed to `Confirmed<serde_json::Value>`).
        let value = confirmed.into_value();
        let event = parse_calendar_event(&value)?;

        let result = self
            .connector
            .create_calendar_event(&event)
            .with_context(|| {
                format!("calendar_add: connector \"{}\" failed", self.connector.id())
            })?;

        let path = result.path.clone();
        let opened_note = if result.opened {
            String::new()
        } else {
            format!(
                ", but it could not be opened automatically; open {} yourself",
                path.display()
            )
        };
        // Honest about what "Undo" actually does (connector design doc,
        // task brief for #34): it can only delete the temp .ics file this
        // executor wrote. It has no API handle to whatever event the
        // calendar app created from that file, so it cannot remove that
        // event -- the summary says so explicitly rather than leaving the
        // gap implicit, since `Undo` has one `summary` string, used both
        // as the description of what happened and of what undoing it does.
        let summary = format!(
            "Added \"{}\" to your calendar via {}{opened_note}. Undo removes this file; removing the event from your calendar app is a manual step.",
            event.title,
            path.display()
        );

        Ok(Undo::recording(summary, move || {
            std::fs::remove_file(&path)
                .with_context(|| format!("could not remove {}", path.display()))?;
            Ok(())
        }))
    }
}

/// Reads the `calendar_event` schema's five fields (`title`, `start`,
/// `end`, `location`, `notes` -- `notes` maps to `CalendarEvent.description`)
/// out of a confirmed proposal's JSON `Value`. A missing or unparseable
/// `title`/`start` is a hard error (rule 7: an error card, not a silent
/// no-op); an empty `end` is `None` (the connector's documented default
/// duration applies), a present-but-unparseable `end` is also a hard error.
fn parse_calendar_event(value: &Value) -> Result<CalendarEvent> {
    let title = value
        .get("title")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("calendar_add: proposal has no \"title\" field"))?
        .to_string();

    let start_str = value
        .get("start")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("calendar_add: proposal has no \"start\" field"))?;
    let start = connectors::parse_event_time(start_str)
        .with_context(|| format!("calendar_add: invalid \"start\" field \"{start_str}\""))?;

    let end = match value.get("end").and_then(Value::as_str) {
        Some(s) if !s.trim().is_empty() => Some(
            connectors::parse_event_time(s)
                .with_context(|| format!("calendar_add: invalid \"end\" field \"{s}\""))?,
        ),
        _ => None,
    };

    let location = value
        .get("location")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let description = value
        .get("notes")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok(CalendarEvent {
        title,
        start,
        end,
        location,
        description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use crate::connectors::{IcsConnector, Opener};
    use crate::ui::confirm::Proposal;

    // -- a fake opener that records calls; never opens a real file --------

    #[derive(Default)]
    struct RecordingOpener {
        opened: RefCell<Vec<PathBuf>>,
    }

    unsafe impl Sync for RecordingOpener {}

    impl Opener for RecordingOpener {
        fn open(&self, path: &Path) -> Result<()> {
            self.opened.borrow_mut().push(path.to_path_buf());
            Ok(())
        }
    }

    fn test_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wingman-calendar-add-test-{}-{}-{label}",
            std::process::id(),
            n
        ))
    }

    fn executor_with_recording_opener(label: &str) -> (CalendarAddExecutor, PathBuf) {
        let dir = test_temp_dir(label);
        let connector =
            IcsConnector::with_temp_dir_and_opener(dir.clone(), RecordingOpener::default());
        (
            CalendarAddExecutor::with_connector(Box::new(connector)),
            dir,
        )
    }

    fn confirmed(value: Value) -> Confirmed<Value> {
        crate::ui::confirm::confirm(Proposal::new(value), crate::ui::confirm::user_confirmed())
    }

    fn sample_proposal() -> Value {
        serde_json::json!({
            "title": "Standup",
            "start": "2026-09-20T14:00:00Z",
            "end": "2026-09-20T14:30:00Z",
            "location": "Room 4B",
            "notes": "Daily sync"
        })
    }

    // -- Effect ---------------------------------------------------------

    #[test]
    fn calendar_add_is_not_read_only() {
        let (executor, dir) = executor_with_recording_opener("effect");
        assert_eq!(executor.effect(), Effect::Writes);
        std::fs::remove_dir_all(&dir).ok();
    }

    // -- parse_calendar_event --------------------------------------------

    #[test]
    fn parses_a_full_proposal() {
        let event = parse_calendar_event(&sample_proposal()).unwrap();
        assert_eq!(event.title, "Standup");
        assert_eq!(event.location.as_deref(), Some("Room 4B"));
        assert_eq!(event.description.as_deref(), Some("Daily sync"));
        assert!(event.end.is_some());
    }

    #[test]
    fn empty_end_parses_as_missing_not_an_error() {
        let mut value = sample_proposal();
        value["end"] = serde_json::json!("");
        let event = parse_calendar_event(&value).unwrap();
        assert!(event.end.is_none());
    }

    #[test]
    fn missing_title_is_a_named_error_not_a_panic() {
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("title");
        let err = parse_calendar_event(&value).expect_err("no title must error");
        assert!(err.to_string().contains("title"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn unparseable_start_is_a_named_error() {
        let mut value = sample_proposal();
        value["start"] = serde_json::json!("whenever works");
        let err = parse_calendar_event(&value).expect_err("an unparseable start must error");
        assert!(err.to_string().contains("start"));
    }

    #[test]
    fn unparseable_but_present_end_is_a_named_error() {
        let mut value = sample_proposal();
        value["end"] = serde_json::json!("not a date");
        let err = parse_calendar_event(&value)
            .expect_err("an unparseable end must error, not be silently dropped");
        assert!(err.to_string().contains("end"));
    }

    #[test]
    fn blank_location_and_notes_become_none_not_empty_strings() {
        let mut value = sample_proposal();
        value["location"] = serde_json::json!("");
        value["notes"] = serde_json::json!("");
        let event = parse_calendar_event(&value).unwrap();
        assert!(event.location.is_none());
        assert!(event.description.is_none());
    }

    // -- execute: wires through to the connector and back ------------------

    #[test]
    fn execute_writes_an_ics_file_through_the_injected_connector_and_never_touches_the_real_shell()
    {
        let (executor, dir) = executor_with_recording_opener("execute");
        let undo = executor.execute(confirmed(sample_proposal())).unwrap();

        assert!(undo.summary.contains("Standup"));
        assert!(
            undo.summary.contains("manual step") && undo.summary.contains("calendar app"),
            "undo summary must be honest that it cannot remove the event from the \
             calendar app itself (only the temp file): {}",
            undo.summary
        );
        assert!(
            !undo.summary.contains('\u{2014}'),
            "no em dashes: {}",
            undo.summary
        );

        let written = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().is_some_and(|ext| ext == "ics"))
            .expect("execute must have written an .ics file");
        let content = std::fs::read_to_string(written.path()).unwrap();
        assert!(content.contains("SUMMARY:Standup"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn execute_on_an_invalid_proposal_errors_before_touching_the_connector() {
        let (executor, dir) = executor_with_recording_opener("invalid");
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("title");

        // `Result::expect_err` needs `Undo: Debug` for its panic message,
        // which `Undo` deliberately does not implement (its `restore`
        // field is a boxed closure); `.err().expect(...)` only needs the
        // error type, so it is used here instead.
        let err = executor
            .execute(confirmed(value))
            .err()
            .expect("an invalid proposal must error");
        assert!(err.to_string().contains("title"));

        // No file should have been written: the parse failure happens
        // before any connector call.
        let count = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
        assert_eq!(count, 0, "no file should be written on a parse failure");

        std::fs::remove_dir_all(&dir).ok();
    }

    // -- undo: deletes the temp file, is honest about the rest -------------

    #[test]
    fn undo_deletes_the_generated_ics_file() {
        let (executor, dir) = executor_with_recording_opener("undo");
        let undo = executor.execute(confirmed(sample_proposal())).unwrap();

        let ics_path = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .find(|e| e.path().extension().is_some_and(|ext| ext == "ics"))
            .expect("execute must have written an .ics file")
            .path();
        assert!(ics_path.exists());

        undo.undo().expect("undo must succeed");
        assert!(
            !ics_path.exists(),
            "undo must delete the generated .ics file"
        );

        std::fs::remove_dir_all(&dir).ok();
    }
}
