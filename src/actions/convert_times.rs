//! "Convert times to my time zone" (#315): every time on screen, in the
//! user's own zone.
//!
//! A generic action over the selection first and the screen otherwise, with
//! executor `"none"`: the card is the result. The model only reads the times
//! off the screen, as written (`time_list`: the text, the time, the date if
//! one is shown, the zone as written). It never converts anything. The
//! conversion is [`super::time_zones`], deterministic Rust over Windows' own
//! time zone data, run when the card text is built ([`card_text`]).
//!
//! Ambiguous zones (CST, IST, BST) are listed with every reading, never
//! picked. The rules for abbreviations and missing years are in
//! `time_zones`' module doc.
//!
//! Placing each converted time beside the original on screen is the overlay
//! (#325), not this action.

use serde_json::Value;

use super::generic_answer::CardText;
use super::time_zones::{
    format_offset, local_to_utc, local_with_offset, parse_date, parse_time, parse_zone,
    resolve_date, weekday, Reading, Resolved, TimeOfDay, TzProvider, WindowsZones, ZoneId,
    ZoneParse,
};
use super::{Action, InputKind, Prefer};
use crate::connectors::civil_time::{
    civil_datetime_to_unix, days_from_civil, now_utc, CivilDate, CivilDateTime,
};
use crate::provider::sanitize_model_text;
use crate::ui::card::{HEADLINE_CHARS_PER_LINE, HEADLINE_MAX_LINES};

/// This action's id. Runs through the generic dispatch path
/// (`App::run_generic_action`), like an `actions.toml` entry.
pub const ACTION_ID: &str = "convert-times-to-my-zone";

/// The proposal kind (`actions::schema::schema_for` key).
pub const PROPOSAL_KIND: &str = "time_list";

/// Most times converted from one screen. More are ignored.
pub const MAX_TIMES: usize = 12;
/// Longest headline, in characters: the collapsed card's whole headline
/// budget, less a little slack for word wrap.
pub const MAX_HEADLINE_CHARS: usize = HEADLINE_MAX_LINES as usize * HEADLINE_CHARS_PER_LINE - 15;
/// Longest card detail, in characters, like the sibling actions.
pub const MAX_DETAIL_CHARS: usize = 700;
/// Longest piece of model text kept (`text`, `time`, `date`, `zone`).
const MAX_FIELD_CHARS: usize = 60;

/// What the card says when no time was found.
pub const NOTHING_HEADLINE: &str = "No times found on screen";

/// The action's prompt. The model finds; it does not convert.
pub const BASE_PROMPT: &str = "You are shown what is on the user's screen, as text if some was selected and as a screenshot otherwise. List every time of day that is shown, in the order they appear, at most 12. Do not convert anything and do not work out any time zone yourself: copy what is written. For each one fill `text`: the time as it appears with the day or date words next to it, for example \"Thursday 9:00 PT\", at most 40 characters. Then `time`: the clock time only, as written, for example \"9:00 PM\" or \"21:00\". Then `date`: the date or day only if one is shown for that time, as written, for example \"Thursday\", \"Oct 22\" or \"2026-10-22\", otherwise an empty string. Then `zone`: the time zone exactly as written, for example \"PT\", \"CET\", \"IST\", \"UTC+5:30\", otherwise an empty string. Never guess a zone or a date that is not shown. For a range such as \"9-10am PT\", list the start and the end as two times. If no time of day is shown, return an empty list. Use plain text only: no markdown, no LaTeX, and no em dashes.";

/// The built-in action: group Work, the selection when there is one and the
/// screen otherwise, proposal `time_list`, executor `none`, `confirm = false`
/// (read-only: the answer is the card).
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Convert times to my time zone".to_string(),
        group: Some("Work".to_string()),
        inputs: vec![InputKind::Selection, InputKind::Screen],
        proposal: PROPOSAL_KIND.to_string(),
        executor: "none".to_string(),
        confirm: false,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

const WEEKDAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

fn field(item: &Value, key: &str) -> String {
    let s = item.get(key).and_then(Value::as_str).unwrap_or("");
    sanitize_model_text(
        &s.split_whitespace().collect::<Vec<_>>().join(" "),
        MAX_FIELD_CHARS,
    )
}

fn clock(t: &CivilDateTime, twelve_hour: bool) -> String {
    if twelve_hour {
        let h = match t.hour % 12 {
            0 => 12,
            h => h,
        };
        format!(
            "{h}:{:02} {}",
            t.minute,
            if t.hour < 12 { "AM" } else { "PM" }
        )
    } else {
        format!("{}:{:02}", t.hour, t.minute)
    }
}

fn day_label(d: &CivilDate, with_year: bool) -> String {
    let base = format!(
        "{} {} {}",
        WEEKDAYS[weekday(d) as usize % 7],
        d.day,
        MONTHS[(d.month as usize).clamp(1, 12) - 1]
    );
    if with_year {
        format!("{base} {}", d.year)
    } else {
        base
    }
}

fn days(d: &CivilDate) -> i64 {
    days_from_civil(d.year as i64, d.month as u32, d.day as u32)
}

/// One reading of one time, as a card fragment: "Thu 22 Oct, 5:00 PM (next
/// day, UTC-7 to UTC-4)", or the reason it cannot be converted. The two
/// offsets are the source zone's and yours on that date, so what was used is
/// visible ("9:00 PST" in July reads as UTC-7).
fn convert_one(
    p: &dyn TzProvider,
    now: i64,
    time: TimeOfDay,
    date_text: &str,
    reading: &Reading,
) -> Result<String, String> {
    let lookup = || format!("cannot look up the time zone {}", reading.name);
    let (source_now, _) = local_with_offset(p, &reading.zone, now).ok_or_else(lookup)?;
    let spec =
        parse_date(date_text).map_err(|()| format!("the date \"{date_text}\" is not clear"))?;
    let date = resolve_date(spec.as_ref(), source_now.date)
        .ok_or_else(|| format!("there is no such date as \"{date_text}\""))?;
    let local = CivilDateTime {
        date,
        hour: time.hour,
        minute: time.minute,
        second: 0,
    };
    let (instant, repeated) = match local_to_utc(p, &reading.zone, &local).ok_or_else(lookup)? {
        Resolved::Instant(t) => (t, false),
        Resolved::Repeated(t) => (t, true),
        Resolved::Skipped => {
            return Err(format!(
                "that time does not exist on {}: the clocks skip ahead",
                day_label(&date, date.year != source_now.date.year)
            ))
        }
    };
    let (_, from_offset) = local_with_offset(p, &reading.zone, instant).ok_or_else(lookup)?;
    let (mine, offset) = local_with_offset(p, &ZoneId::Local, instant)
        .ok_or_else(|| "cannot look up your time zone".to_string())?;
    // The year is shown whenever the date is not in the current year, so a
    // weekday that only fits 2031 is visible as such.
    let with_year = date.year != source_now.date.year || mine.date.year != source_now.date.year;
    let mut notes = Vec::new();
    match days(&mine.date) - days(&date) {
        0 => {}
        1 => notes.push("next day".to_string()),
        -1 => notes.push("previous day".to_string()),
        n => notes.push(format!("{n:+} days")),
    }
    notes.push(format!(
        "{} to {}",
        format_offset(from_offset),
        format_offset(offset)
    ));
    if repeated {
        notes.push("that hour happens twice, the first is shown".to_string());
    }
    Ok(format!(
        "{}, {} ({})",
        day_label(&mine.date, with_year),
        clock(&mine, time.twelve_hour),
        notes.join(", ")
    ))
}

/// "label is result" or "label: reason".
fn line(label: &str, r: Result<String, String>) -> String {
    match r {
        Ok(s) => format!("{label} is {s}"),
        Err(e) => format!("{label}: {e}"),
    }
}

/// The card lines for one found time: one line, or a header and one line per
/// reading when the zone is ambiguous.
fn lines_for(p: &dyn TzProvider, now: i64, item: &Value) -> Vec<String> {
    let (time_text, date_text, zone_text) = (
        field(item, "time"),
        field(item, "date"),
        field(item, "zone"),
    );
    let mut label = field(item, "text");
    if label.is_empty() {
        label = [date_text.as_str(), time_text.as_str(), zone_text.as_str()]
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" ");
    }
    if label.is_empty() {
        return Vec::new();
    }
    let Some(time) = parse_time(&time_text) else {
        return vec![format!("{label}: the time is not clear")];
    };
    match parse_zone(&zone_text) {
        ZoneParse::Missing => vec![format!("{label}: no time zone is shown")],
        ZoneParse::Unknown => vec![format!("{label}: I do not know the zone \"{zone_text}\"")],
        ZoneParse::Readings(readings) if readings.len() == 1 => {
            vec![line(
                &label,
                convert_one(p, now, time, &date_text, &readings[0]),
            )]
        }
        ZoneParse::Readings(readings) => {
            let mut lines = vec![format!("{label}: {zone_text} can mean more than one zone")];
            for reading in &readings {
                let text = match convert_one(p, now, time, &date_text, reading) {
                    Ok(s) | Err(s) => s,
                };
                lines.push(format!("  If {}: {text}", reading.name));
            }
            lines
        }
    }
}

/// The card text for a `time_list` proposal, with the zone data and the
/// clock injected. The headline is the first time (and how many more there
/// are); the detail lists the times until it reaches [`MAX_DETAIL_CHARS`],
/// then says "and N more".
pub fn card_text_with(proposal: &Value, p: &dyn TzProvider, now_secs: i64) -> CardText {
    let items = proposal
        .get("times")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let skipped_items = items.len().saturating_sub(MAX_TIMES);
    let entries: Vec<Vec<String>> = items
        .iter()
        .take(MAX_TIMES)
        .map(|item| lines_for(p, now_secs, item))
        .filter(|lines| !lines.is_empty())
        .collect();
    let Some(first) = entries.first() else {
        return CardText {
            headline: NOTHING_HEADLINE.to_string(),
            detail: String::new(),
        };
    };
    let more = entries.len() - 1 + skipped_items;
    let suffix = match more {
        0 => String::new(),
        n => format!(" (+{n} more)"),
    };
    let budget = MAX_HEADLINE_CHARS - suffix.chars().count();
    let headline = format!("{}{suffix}", sanitize_model_text(&first[0], budget));
    if more == 0 && first.len() == 1 {
        return CardText {
            headline,
            detail: String::new(),
        };
    }
    let mut lines: Vec<String> = Vec::new();
    let mut used = 0;
    let mut shown = 0;
    for entry in &entries {
        let cost: usize = entry.iter().map(|l| l.chars().count() + 1).sum();
        if shown > 0 && used + cost > MAX_DETAIL_CHARS {
            break;
        }
        used += cost;
        shown += 1;
        lines.extend(entry.iter().cloned());
    }
    let left = entries.len() - shown + skipped_items;
    if left > 0 {
        lines.push(format!("and {left} more"));
    }
    CardText {
        headline,
        detail: lines.join("\n"),
    }
}

/// [`card_text_with`] over Windows' time zone data and the real clock.
pub fn card_text(proposal: &Value) -> CardText {
    let now = civil_datetime_to_unix(&now_utc());
    card_text_with(proposal, &WindowsZones::new(), now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::time_zones::WindowsZones;
    use serde_json::json;

    /// Eastern is the "local" zone in these tests, whatever the machine's.
    fn at(proposal: &Value, y: i32, mo: u8, d: u8) -> CardText {
        let now = civil_datetime_to_unix(&CivilDateTime {
            date: CivilDate {
                year: y,
                month: mo,
                day: d,
            },
            hour: 12,
            minute: 0,
            second: 0,
        });
        card_text_with(
            proposal,
            &WindowsZones::with_local("Eastern Standard Time"),
            now,
        )
    }

    fn item(text: &str, time: &str, date: &str, zone: &str) -> Value {
        json!({"text": text, "time": time, "date": date, "zone": zone})
    }

    #[test]
    fn one_time_is_a_single_line_with_the_day_and_the_offset() {
        // 2026-10-15 is a Thursday. 9:00 PM Pacific (UTC-7) is midnight
        // Eastern (UTC-4): the next day.
        let c = at(
            &json!({"times": [item("Thursday 9:00 PM PT", "9:00 PM", "Thursday", "PT")]}),
            2026,
            10,
            15,
        );
        assert_eq!(
            c.headline,
            "Thursday 9:00 PM PT is Fri 16 Oct, 12:00 AM (next day, UTC-7 to UTC-4)"
        );
        assert_eq!(c.detail, "");
    }

    #[test]
    fn the_twenty_four_hour_style_is_kept() {
        let c = at(
            &json!({"times": [item("Standup 14:00 CET", "14:00", "", "CET")]}),
            2026,
            1,
            14,
        );
        // January: CET is UTC+1 and Eastern UTC-5, six hours apart.
        assert_eq!(c.headline, "Standup 14:00 CET is Wed 14 Jan, 8:00 (UTC+1 to UTC-5)");
    }

    #[test]
    fn several_times_list_in_the_detail_and_count_in_the_headline() {
        let c = at(
            &json!({"times": [
                item("9:00 UTC", "9:00", "", "UTC"),
                item("10:00 UTC", "10:00", "", "UTC"),
                item("11:00 UTC", "11:00", "", "UTC"),
            ]}),
            2026,
            1,
            14,
        );
        assert!(c.headline.ends_with("(+2 more)"), "{}", c.headline);
        assert_eq!(c.detail.lines().count(), 3);
        assert!(c.detail.lines().nth(2).unwrap().contains("6:00"));
    }

    #[test]
    fn an_ambiguous_zone_shows_every_reading_and_picks_none() {
        let c = at(
            &json!({"times": [item("9:00 IST", "9:00", "", "IST")]}),
            2026,
            1,
            14,
        );
        assert!(
            c.headline.contains("can mean more than one zone"),
            "{}",
            c.headline
        );
        assert!(c.detail.contains("If India (UTC+5:30)"));
        assert!(c.detail.contains("If Ireland"));
        assert!(c.detail.contains("If Israel"));
        // India 9:00 is 03:30 UTC, 22:30 the day before in Eastern.
        assert!(c.detail.contains("Tue 13 Jan, 22:30"), "{}", c.detail);
    }

    #[test]
    fn a_skipped_time_is_said_not_converted() {
        let c = at(
            &json!({"times": [item("2:30 AM PT", "2:30 AM", "Mar 8", "PT")]}),
            2026,
            3,
            1,
        );
        assert_eq!(
            c.headline,
            "2:30 AM PT: that time does not exist on Sun 8 Mar: the clocks skip ahead"
        );
    }

    #[test]
    fn the_source_offset_used_is_shown_and_pst_in_july_reads_as_utc_minus_7() {
        let c = at(
            &json!({"times": [item("9:00 AM PST", "9:00 AM", "Jul 15", "PST")]}),
            2026,
            7,
            1,
        );
        assert_eq!(
            c.headline,
            "9:00 AM PST is Wed 15 Jul, 12:00 PM (UTC-7 to UTC-4)"
        );
    }

    #[test]
    fn the_year_is_shown_when_the_date_is_not_this_year() {
        // A Thursday the 13th of November only comes in 2031 from 2026.
        let c = at(
            &json!({"times": [item("Thu Nov 13 9:00 UTC", "9:00", "Thu Nov 13", "UTC")]}),
            2026,
            10,
            15,
        );
        assert!(c.headline.contains("Thu 13 Nov 2031"), "{}", c.headline);
        // This year: no year.
        let c = at(
            &json!({"times": [item("Fri Nov 13 9:00 UTC", "9:00", "Fri Nov 13", "UTC")]}),
            2026,
            10,
            15,
        );
        assert!(!c.headline.contains("2026"), "{}", c.headline);
    }

    #[test]
    fn error_lines_read_as_a_reason_after_the_label() {
        for p in [
            item("t", "9:00", "Feb 30", "UTC"),
            item("t", "9:00", "1/2/99999", "UTC"),
        ] {
            let c = at(&json!({"times": [p]}), 2026, 1, 14);
            assert!(c.headline.starts_with("t: "), "{}", c.headline);
            assert!(!c.headline.contains(" is "), "{}", c.headline);
        }
        let c = at(&json!({"times": [item("t", "9:00", "Feb 30", "UTC")]}), 2026, 1, 14);
        assert_eq!(c.headline, "t: there is no such date as \"Feb 30\"");
        let c = at(&json!({"times": [item("t", "9:00", "3/4", "UTC")]}), 2026, 1, 14);
        assert_eq!(c.headline, "t: the date \"3/4\" is not clear");
    }

    #[test]
    fn a_repeated_time_is_flagged() {
        let c = at(
            &json!({"times": [item("1:30 AM PT", "1:30 AM", "Nov 1", "PT")]}),
            2026,
            10,
            15,
        );
        assert!(c.headline.contains("happens twice"), "{}", c.headline);
    }

    #[test]
    fn a_dst_boundary_uses_the_rules_for_that_date() {
        // Before the US change (Mar 1) Pacific is UTC-8 and Eastern UTC-5.
        let before = at(
            &json!({"times": [item("t", "9:00", "Mar 2", "PT")]}),
            2026,
            3,
            1,
        );
        // On Mar 20 Eastern is UTC-4 and Central Europe still UTC+1.
        let eu = at(
            &json!({"times": [item("t", "15:00", "Mar 20", "CET")]}),
            2026,
            3,
            1,
        );
        assert!(
            before.headline.contains("12:00 (UTC-8 to UTC-5)"),
            "{}",
            before.headline
        );
        assert!(eu.headline.contains("10:00 (UTC+1 to UTC-4)"), "{}", eu.headline);
    }

    #[test]
    fn a_missing_zone_or_an_unknown_zone_is_said() {
        let c = at(
            &json!({"times": [
                item("9:00", "9:00", "", ""),
                item("9:00 XYZ", "9:00", "", "XYZ"),
                item("9:00 fish", "fish", "", "PT"),
                item("3/4 9:00 PT", "9:00", "3/4", "PT"),
            ]}),
            2026,
            1,
            14,
        );
        let lines: Vec<&str> = c.detail.lines().collect();
        assert!(lines[0].contains("no time zone is shown"));
        assert!(lines[1].contains("do not know the zone"));
        assert!(lines[2].contains("time is not clear"));
        assert!(lines[3].contains("is not clear"));
    }

    #[test]
    fn nothing_found_says_so_and_bad_proposals_do_not_panic() {
        for p in [
            json!({"times": []}),
            json!({}),
            json!("text"),
            json!({"times": [1, null, {}]}),
            json!({"times": [{"text": " ", "time": "", "date": "", "zone": ""}]}),
        ] {
            let c = at(&p, 2026, 1, 14);
            assert_eq!(c.headline, NOTHING_HEADLINE, "{p}");
            assert_eq!(c.detail, "");
        }
    }

    #[test]
    fn the_list_is_capped_and_the_headline_fits_the_collapsed_card() {
        let many: Vec<Value> = (0..20)
            .map(|i| item(&format!("{i}:00 UTC"), &format!("{i}:00"), "", "UTC"))
            .collect();
        let c = at(&json!({ "times": many }), 2026, 1, 14);
        assert_eq!(c.detail.lines().count(), MAX_TIMES + 1);
        assert_eq!(c.detail.lines().last(), Some("and 8 more"));
        assert!(c.headline.ends_with("(+19 more)"), "{}", c.headline);
        // Long lines hit the character cap before the item cap.
        let wide: Vec<Value> = (0..MAX_TIMES)
            .map(|_| item(&"w".repeat(55), "9:00", "", "IST"))
            .collect();
        let c = at(&json!({ "times": wide }), 2026, 1, 14);
        assert!(c.detail.chars().count() <= MAX_DETAIL_CHARS + 40, "{}", c.detail.len());
        assert!(c.detail.lines().last().unwrap().starts_with("and "));
        let long = "very long label ".repeat(10);
        let c = at(
            &json!({"times": [item(&long, "9:00", "", "UTC")]}),
            2026,
            1,
            14,
        );
        let wrapped = c.headline.chars().count().div_ceil(HEADLINE_CHARS_PER_LINE);
        assert!(wrapped <= HEADLINE_MAX_LINES as usize, "{}", c.headline);
    }

    #[test]
    fn em_dashes_never_reach_the_card() {
        let c = at(
            &json!({"times": [item("9:00 \u{2014} PT", "9:00", "", "PT")]}),
            2026,
            1,
            14,
        );
        assert!(!c.headline.contains('\u{2014}') && !c.detail.contains('\u{2014}'));
    }

    #[test]
    fn the_entry_is_a_read_only_answer_over_selection_then_screen() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.inputs, vec![InputKind::Selection, InputKind::Screen]);
        assert_eq!(a.proposal, PROPOSAL_KIND);
        assert_eq!(a.executor, "none");
        assert!(!a.confirm);
        assert!(!a.prompt.contains('\u{2014}'));
        assert!(crate::actions::schema::schema_for(&a.proposal, false).is_some());
        crate::actions::resolve_executor(&a).expect("the none executor resolves");
    }

    #[test]
    fn the_card_text_is_wired_into_the_generic_result_card() {
        let proposal = json!({"times": [item("9:00 UTC", "9:00", "", "UTC")]});
        let c = crate::actions::generic_answer::result_card_text(
            "Convert times to my time zone",
            "none",
            PROPOSAL_KIND,
            "",
            &proposal,
        );
        assert!(c.headline.starts_with("9:00 UTC is "), "{}", c.headline);
    }

    #[test]
    fn the_example_file_prompt_is_the_built_in_prompt() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("actions.example.toml");
        let text = std::fs::read_to_string(path).unwrap().replace('\r', "");
        let file = crate::actions::parse_actions_file(&text).unwrap();
        let a = file
            .actions
            .iter()
            .find(|a| a.id == ACTION_ID)
            .expect("the example file lists the action");
        assert_eq!(a.prompt.replace('\r', ""), BASE_PROMPT);
    }

    #[test]
    fn the_action_is_visible_by_default_and_dispatches_generically() {
        let shown = crate::actions::visible(
            crate::actions::merge_actions(crate::actions::builtin_actions(), vec![]),
            &[],
        );
        assert!(shown.iter().any(|r| r.action.id == ACTION_ID));
        assert_eq!(
            crate::ui::palette_model::dispatch_target_for(ACTION_ID),
            Some(crate::ui::palette_model::DispatchTarget::Generic(
                ACTION_ID.to_string()
            ))
        );
    }
}
