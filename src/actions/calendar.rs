//! "Add event from screen" (#39): the first action to run the full **Look,
//! Propose, Confirm, Do** loop end to end. This module owns the built-in
//! action definition, the prompt (with today's local date and UTC offset
//! baked in, so the model can resolve a relative date/time and emit an
//! ISO 8601 date-time `calendar_add`'s parser can actually use -- see
//! `connectors::ics::parse_event_time` / the connector design doc's
//! "Timezone handling"), the documented "no event" shape, and a pure model
//! of the flow's states, so all of it is testable without Win32, a network
//! call, or a real file.
//!
//! Kept as its own file (not folded into `actions::mod`, which other
//! overnight agents are editing concurrently for their own new built-in
//! actions) so this feature lands as one new file plus the smallest
//! possible diff to `actions::mod`, `app.rs` and `ui::tray`.

use serde_json::Value;

use super::{Action, InputKind, Prefer};

/// This action's id. `app.rs`'s calendar flow looks resolved actions up by
/// this, the same way `actions::DEFAULT_ACTION_ID` anchors "Check my work".
pub const ACTION_ID: &str = "add-to-calendar";

/// The sentinel `title` value the prompt instructs the model to return when
/// no event is visible on screen (see [`build_prompt`]). Chosen to be
/// something no real event title would plausibly collide with, and checked
/// with an exact string match by [`is_no_event`] -- never a substring or
/// case-insensitive match, so a real event that happens to mention "no
/// event" in its title is never mistaken for the sentinel.
pub const NO_EVENT_TITLE: &str = "NO_EVENT";

/// The action's base prompt, before [`build_prompt`] appends today's date
/// and UTC offset. Kept separate from that addition because this is the
/// text an `actions.toml` override of `Action::prompt` replaces (see the
/// action-model design doc's "Origin tracking") -- the date/offset context
/// is always appended fresh at ask-time, by `app.rs`, never something a
/// static prompt string could carry.
pub const BASE_PROMPT: &str = "You are shown a screenshot of the user's screen. Find the ONE calendar event described or shown on screen (an email, a chat message, an invite, a flyer, a webpage, and so on) and extract it: its title, start time, end time if one is shown, location and any short notes worth keeping. If the screen shows more than one event, pick the one that is the clear focus of the screen (e.g. an open invite or the top message), not a list entry glimpsed in the background.";

/// The built-in "Add event from screen" action (#39): group Work, input
/// Screen, proposal `calendar_event` (#26), executor `calendar_add` (#34),
/// `confirm = true` -- the first built-in action that ever shows the
/// preview card instead of a plain answer.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Add event from screen".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Screen],
        proposal: "calendar_event".to_string(),
        executor: "calendar_add".to_string(),
        confirm: true,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

/// Formats a UTC offset in minutes as `+HH:MM`/`-HH:MM`, the exact suffix
/// `connectors::ics::parse_event_time` expects on a date-time string it can
/// resolve to [`crate::connectors::EventTime::Utc`].
pub fn format_utc_offset(total_minutes: i32) -> String {
    let sign = if total_minutes < 0 { '-' } else { '+' };
    let abs = total_minutes.unsigned_abs();
    format!("{sign}{:02}:{:02}", abs / 60, abs % 60)
}

/// The UTC offset, in minutes east of UTC, between a local and a UTC
/// [`CivilDateTime`] describing the *same instant* (i.e. the pair Win32's
/// `TzSpecificLocalTimeToSystemTime` produces for "right now"). Pure day-
/// count arithmetic (via [`days_from_civil`]), so it is correct across a
/// day boundary (e.g. local 23:30 at UTC+2 is still UTC 21:30 the same
/// day, but local 01:30 at UTC+2 is UTC 23:30 the PREVIOUS day) without
/// needing to know which one is "today" -- unlike a naive same-day
/// subtraction of the two `hour`/`minute` fields, which would be off by 24
/// hours exactly in that second case.
pub fn utc_offset_minutes(local: &CivilDateTime, utc: &CivilDateTime) -> i32 {
    (total_minutes_since_epoch(local) - total_minutes_since_epoch(utc)) as i32
}

fn total_minutes_since_epoch(dt: &CivilDateTime) -> i64 {
    let days = days_from_civil(
        dt.date.year as i64,
        dt.date.month as u32,
        dt.date.day as u32,
    );
    days * 24 * 60 + dt.hour as i64 * 60 + dt.minute as i64
}

use crate::connectors::civil_time::{days_from_civil, CivilDate, CivilDateTime};

/// The weekday name for a [`CivilDate`], via `days_from_civil`: the Unix
/// epoch (1970-01-01, day 0) was a Thursday, so `(days + 4).rem_euclid(7)`
/// indexes into a fixed Sunday-first table. **MEASURED 2026-09-17**: an
/// early version of [`build_prompt`] stated only the date, and
/// `gemma3:4b`'s live check (`calendar_add_live_extracts_a_friday_event_...`)
/// resolved "Friday" to that day's own literal date instead of the
/// following day -- i.e. it did not reliably compute the date's weekday
/// itself. Naming the weekday explicitly (`"Today is Thursday,
/// 2026-09-17"`) fixed it on a re-run; this function is what makes that
/// naming possible without asking `app.rs`'s Win32 helper to also compute
/// it (weekday is pure calendar arithmetic, so it belongs next to
/// `utc_offset_minutes`, not duplicated at the Win32 call site).
pub fn weekday_name(date: CivilDate) -> &'static str {
    const NAMES: [&str; 7] = [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ];
    let days = days_from_civil(date.year as i64, date.month as u32, date.day as u32);
    let index = (days + 4).rem_euclid(7) as usize;
    NAMES[index]
}

/// Builds the full system prompt for one "Add event from screen" ask:
/// `base_prompt` (the action's own `prompt` field -- the built-in
/// [`BASE_PROMPT`], or an `actions.toml` override) plus today's local date,
/// weekday name and UTC offset, plainly stated so the model can resolve a
/// relative date ("tomorrow", "Friday 3pm") against them without having to
/// compute the weekday itself (see [`weekday_name`]'s doc comment for the
/// MEASURED reason that matters), and the documented "no event"
/// instruction. `today`/`utc_offset_minutes` are computed by `app.rs`'s
/// Win32 helper (`local_today_and_utc_offset`, checked by hand per rule 8)
/// right before each ask, never cached -- see that function's doc comment.
pub fn build_prompt(base_prompt: &str, today: CivilDate, utc_offset_minutes: i32) -> String {
    let offset = format_utc_offset(utc_offset_minutes);
    let weekday = weekday_name(today);
    format!(
        "{base_prompt}\n\n\
Today is {weekday}, {y:04}-{m:02}-{d:02} local time, and the user's local UTC offset is \
{offset}. Resolve any relative date or time (for example \"tomorrow\" or \"Friday 3pm\") \
against that exact weekday and date, not against any date you might otherwise assume. Give \
\"start\" and \"end\" as either a bare YYYY-MM-DD date for an all-day event with no specific \
time, or an ISO 8601 date-time carrying that exact offset, for example \
2026-09-18T15:00:00{offset}. Leave \"end\" as an empty string if no end time is shown on \
screen. If you cannot find an event on screen at all, set \"title\" to exactly \
\"{NO_EVENT_TITLE}\" and leave \"start\", \"end\", \"location\" and \"notes\" as empty \
strings.",
        y = today.year,
        m = today.month,
        d = today.day,
    )
}

/// Parses a `calendar_event`-shaped [`crate::provider::Completion::text`]
/// into the raw proposal `Value` [`crate::ui::card::Card::show_preview`]
/// and `calendar_add`'s own parser expect. Unlike `parse_answer` there is
/// no separate typed Rust struct for this proposal kind -- the preview
/// card and `executors::calendar_add::parse_calendar_event` both already
/// read straight out of `serde_json::Value` (see the connector design
/// doc) -- so this only proves the completion is a JSON object carrying
/// the five schema fields as strings (`actions::schema`'s
/// `calendar_event_schema`), not a second typed representation of the
/// same shape.
pub fn parse_calendar_proposal(text: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("provider: completion text is not valid JSON: {e}"))?;
    let obj = value.as_object().ok_or_else(|| {
        anyhow::anyhow!("provider: calendar_event completion is not a JSON object")
    })?;
    for field in ["title", "start", "end", "location", "notes"] {
        if !obj.get(field).is_some_and(Value::is_string) {
            anyhow::bail!(
                "provider: calendar_event completion is missing a \"{field}\" string field"
            );
        }
    }
    Ok(value)
}

/// Whether `value` is the documented "no event visible" shape (see
/// [`build_prompt`]): an exact `title` match against [`NO_EVENT_TITLE`],
/// never a substring or case-insensitive one.
pub fn is_no_event(value: &Value) -> bool {
    value.get("title").and_then(Value::as_str) == Some(NO_EVENT_TITLE)
}

// ---------------------------------------------------------------------------
// The flow's pure state machine
// ---------------------------------------------------------------------------

/// The "Add event from screen" flow's states (#39's task brief: "Proposed
/// -> Previewed -> Confirmed -> Executed / Cancelled"), independent of
/// Win32/`Card`/the network, so the transitions are provable with a plain
/// unit test rather than only by driving a real card and a real provider.
/// `app.rs`'s real wiring (`add_event_from_screen` -> `on_calendar_result`
/// -> `Card::show_preview` -> `on_preview_decided`) follows exactly these
/// rules procedurally; this type exists to let the rules themselves be
/// asserted on directly.
// `app.rs`'s real wiring follows these rules procedurally rather than
// literally holding a `FlowState` value across the async gap (the card's
// own `CardState::Preview` already IS the "Previewed" state in
// production, and there is no second source of truth to keep in sync) --
// so this type and `advance` below are exercised only by this module's
// own tests, the same "pure logic proven by its own test suite, not yet a
// literal runtime dependency" status other forward-wired items in this
// crate have (e.g. `Action::hotkey`) until a caller needs the type
// itself, not just the rules it encodes.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq)]
pub enum FlowState {
    /// The provider returned a real (non-"no event") proposal.
    Proposed(Value),
    /// The proposal is on screen in the preview card, waiting for a
    /// decision.
    Previewed(Value),
    /// "Do it": exactly what `ui::confirm::confirm_preview` would build.
    Confirmed(Value),
    /// The executor ran successfully; carries the summary text the result
    /// card shows.
    Executed { summary: String },
    /// "Cancel"/Esc, or the documented "no event" shape -- nothing ever
    /// runs.
    Cancelled,
    /// The provider chain or the executor failed; carries the error text
    /// for the error card.
    Failed(String),
}

/// One thing that can happen to move the flow from one [`FlowState`] to
/// the next.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum FlowEvent {
    /// The provider chain finished (or failed) -- `Err` short-circuits to
    /// [`FlowState::Failed`] from any state.
    ProviderReturned(Result<Value, String>),
    /// The preview card is now on screen.
    Shown,
    /// The user pressed "Do it".
    Confirmed,
    /// The user pressed "Cancel"/Esc.
    Cancelled,
    /// The executor ran (a real one, or a scripted stand-in in a test --
    /// see this module's tests for how `executors::calendar_add`'s own
    /// injectable-connector tests already cover "no real file opened").
    Executed(Result<String, String>),
}

/// The whole flow as one pure transition function: `state` before the
/// event, `event`, `state` after. Every transition #39's task brief names
/// (Proposed -> Previewed -> Confirmed -> Executed / Cancelled) is one
/// match arm here. An event that does not make sense for the current
/// state (e.g. `Confirmed` while still `Proposed`) is a no-op, not a
/// panic: the real Win32 wiring can only ever fire events in the right
/// order (the card enforces it -- there is no "Do it" button before
/// `show_preview` runs), but a pure function driven by a scripted test
/// must never panic on an out-of-order event either.
#[allow(dead_code)]
pub fn advance(state: FlowState, event: FlowEvent) -> FlowState {
    match (state, event) {
        (_, FlowEvent::ProviderReturned(Err(e))) => FlowState::Failed(e),
        (_, FlowEvent::ProviderReturned(Ok(value))) if is_no_event(&value) => FlowState::Cancelled,
        (_, FlowEvent::ProviderReturned(Ok(value))) => FlowState::Proposed(value),
        (FlowState::Proposed(value), FlowEvent::Shown) => FlowState::Previewed(value),
        (FlowState::Previewed(value), FlowEvent::Confirmed) => FlowState::Confirmed(value),
        (FlowState::Previewed(_), FlowEvent::Cancelled) => FlowState::Cancelled,
        (FlowState::Confirmed(_), FlowEvent::Executed(Ok(summary))) => {
            FlowState::Executed { summary }
        }
        (FlowState::Confirmed(_), FlowEvent::Executed(Err(e))) => FlowState::Failed(e),
        (other, _) => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_proposal() -> Value {
        serde_json::json!({
            "title": "Team sync",
            "start": "2026-09-18T15:00:00-04:00",
            "end": "",
            "location": "Room 4B",
            "notes": ""
        })
    }

    fn no_event_proposal() -> Value {
        serde_json::json!({
            "title": NO_EVENT_TITLE,
            "start": "",
            "end": "",
            "location": "",
            "notes": ""
        })
    }

    // -- builtin_action ----------------------------------------------------

    #[test]
    fn builtin_action_matches_the_task_brief() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.name, "Add event from screen");
        assert_eq!(a.group.as_deref(), Some("Work"));
        assert_eq!(a.inputs, vec![InputKind::Screen]);
        assert_eq!(a.proposal, "calendar_event");
        assert_eq!(a.executor, "calendar_add");
        assert!(a.confirm, "this action must always show the preview card");
        assert!(!a.rate_difficulty);
        assert!(a.enabled);
        assert!(!a.prompt.is_empty());
    }

    // -- format_utc_offset / utc_offset_minutes -----------------------------

    #[test]
    fn format_utc_offset_formats_negative_zero_and_positive() {
        assert_eq!(format_utc_offset(-240), "-04:00");
        assert_eq!(format_utc_offset(0), "+00:00");
        assert_eq!(format_utc_offset(330), "+05:30");
    }

    #[test]
    fn utc_offset_minutes_computes_a_simple_same_day_offset() {
        let local = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 17,
            },
            hour: 11,
            minute: 0,
            second: 0,
        };
        let utc = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 17,
            },
            hour: 15,
            minute: 0,
            second: 0,
        };
        // Local is behind UTC by 4 hours -> UTC-04:00.
        assert_eq!(utc_offset_minutes(&local, &utc), -240);
    }

    #[test]
    fn utc_offset_minutes_handles_a_day_boundary_crossing() {
        // Local 01:30 on the 18th at UTC+02:00 is UTC 23:30 on the 17th.
        let local = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 18,
            },
            hour: 1,
            minute: 30,
            second: 0,
        };
        let utc = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 17,
            },
            hour: 23,
            minute: 30,
            second: 0,
        };
        assert_eq!(utc_offset_minutes(&local, &utc), 120);
    }

    // -- weekday_name ---------------------------------------------------------

    #[test]
    fn weekday_name_matches_known_reference_dates() {
        // 1970-01-01 (the Unix epoch) was a Thursday; 2026-09-17 was
        // MEASURED (`date -d 2026-09-17 +%A`) as a Thursday too, and
        // 2026-09-20 (used throughout this crate's other tests as a
        // sample event date) was a Sunday.
        assert_eq!(
            weekday_name(CivilDate {
                year: 1970,
                month: 1,
                day: 1
            }),
            "Thursday"
        );
        assert_eq!(
            weekday_name(CivilDate {
                year: 2026,
                month: 9,
                day: 17
            }),
            "Thursday"
        );
        assert_eq!(
            weekday_name(CivilDate {
                year: 2026,
                month: 9,
                day: 18
            }),
            "Friday"
        );
        assert_eq!(
            weekday_name(CivilDate {
                year: 2026,
                month: 9,
                day: 20
            }),
            "Sunday"
        );
    }

    // -- build_prompt --------------------------------------------------------

    #[test]
    fn build_prompt_includes_todays_date_weekday_and_offset() {
        let today = CivilDate {
            year: 2026,
            month: 9,
            day: 17,
        };
        let prompt = build_prompt(BASE_PROMPT, today, -240);
        assert!(prompt.contains("2026-09-17"));
        assert!(prompt.contains("Thursday"));
        assert!(prompt.contains("-04:00"));
        assert!(prompt.starts_with(BASE_PROMPT));
    }

    #[test]
    fn build_prompt_documents_the_no_event_sentinel() {
        let today = CivilDate {
            year: 2026,
            month: 9,
            day: 17,
        };
        let prompt = build_prompt(BASE_PROMPT, today, 0);
        assert!(prompt.contains(NO_EVENT_TITLE));
    }

    #[test]
    fn build_prompt_carries_a_user_override_of_the_base_prompt() {
        let today = CivilDate {
            year: 2026,
            month: 9,
            day: 17,
        };
        let prompt = build_prompt("custom override text", today, 0);
        assert!(prompt.starts_with("custom override text"));
    }

    // -- parse_calendar_proposal ---------------------------------------------

    #[test]
    fn parse_calendar_proposal_parses_a_full_object() {
        let text = sample_proposal().to_string();
        let value = parse_calendar_proposal(&text).expect("valid calendar_event JSON must parse");
        assert_eq!(value["title"], "Team sync");
    }

    #[test]
    fn parse_calendar_proposal_rejects_invalid_json() {
        let err = parse_calendar_proposal("not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn parse_calendar_proposal_rejects_a_non_object() {
        let err = parse_calendar_proposal("[1,2,3]").unwrap_err();
        assert!(err.to_string().contains("object"));
    }

    #[test]
    fn parse_calendar_proposal_rejects_a_missing_field() {
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("location");
        let err = parse_calendar_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("location"));
    }

    #[test]
    fn parse_calendar_proposal_rejects_a_non_string_field() {
        let mut value = sample_proposal();
        value["title"] = serde_json::json!(42);
        let err = parse_calendar_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("title"));
    }

    #[test]
    fn parse_calendar_proposal_accepts_the_no_event_shape() {
        let text = no_event_proposal().to_string();
        let value = parse_calendar_proposal(&text).expect("the no-event shape must still parse");
        assert!(is_no_event(&value));
    }

    // -- is_no_event ----------------------------------------------------------

    #[test]
    fn is_no_event_true_for_the_exact_sentinel() {
        assert!(is_no_event(&no_event_proposal()));
    }

    #[test]
    fn is_no_event_false_for_a_real_event() {
        assert!(!is_no_event(&sample_proposal()));
    }

    #[test]
    fn is_no_event_is_an_exact_match_not_a_substring() {
        let mut value = sample_proposal();
        value["title"] = serde_json::json!("NO_EVENT (kidding, there is one)");
        assert!(
            !is_no_event(&value),
            "a title merely containing the sentinel must not count as no-event"
        );
    }

    // -- advance: the pure flow state machine --------------------------------

    #[test]
    fn advance_provider_error_yields_failed_from_any_state() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(Err("boom".to_string())),
        );
        assert_eq!(next, FlowState::Failed("boom".to_string()));
    }

    #[test]
    fn advance_no_event_proposal_yields_cancelled_not_proposed() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(Ok(no_event_proposal())),
        );
        assert_eq!(next, FlowState::Cancelled);
    }

    #[test]
    fn advance_full_happy_path_proposed_to_executed() {
        let proposal = sample_proposal();
        let s = advance(
            FlowState::Cancelled, // starting state is irrelevant for ProviderReturned
            FlowEvent::ProviderReturned(Ok(proposal.clone())),
        );
        assert_eq!(s, FlowState::Proposed(proposal.clone()));

        let s = advance(s, FlowEvent::Shown);
        assert_eq!(s, FlowState::Previewed(proposal.clone()));

        let s = advance(s, FlowEvent::Confirmed);
        assert_eq!(s, FlowState::Confirmed(proposal));

        let s = advance(
            s,
            FlowEvent::Executed(Ok("added \"Team sync\" to your calendar".to_string())),
        );
        assert_eq!(
            s,
            FlowState::Executed {
                summary: "added \"Team sync\" to your calendar".to_string()
            }
        );
    }

    #[test]
    fn advance_cancel_from_previewed_yields_cancelled() {
        let proposal = sample_proposal();
        let s = FlowState::Previewed(proposal);
        let s = advance(s, FlowEvent::Cancelled);
        assert_eq!(s, FlowState::Cancelled);
    }

    #[test]
    fn advance_confirmed_with_executor_error_yields_failed() {
        let proposal = sample_proposal();
        let s = FlowState::Confirmed(proposal);
        let s = advance(s, FlowEvent::Executed(Err("disk full".to_string())));
        assert_eq!(s, FlowState::Failed("disk full".to_string()));
    }

    #[test]
    fn advance_an_out_of_order_event_is_a_no_op() {
        // "Confirmed" before the card ever showed a preview: cannot happen
        // through the real Win32 wiring (the card enforces the order), but
        // a scripted test must not panic on it either.
        let s = FlowState::Proposed(sample_proposal());
        let next = advance(s.clone(), FlowEvent::Confirmed);
        assert_eq!(
            next, s,
            "an out-of-order event must leave the state unchanged"
        );
    }

    #[test]
    fn advance_shown_on_a_terminal_state_is_a_no_op() {
        let s = advance(FlowState::Cancelled, FlowEvent::Shown);
        assert_eq!(s, FlowState::Cancelled);
    }
}
