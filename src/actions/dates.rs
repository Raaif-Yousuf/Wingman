//! Relative dates made concrete (#336): "next Tuesday", "in 3 days",
//! "tomorrow", "end of the month" worked out against a date.
//!
//! Pure. Nothing here reads the clock: the caller passes the user's local
//! `today` and the first day of their week, so every result is reproducible
//! and unit-tested against fixed dates (month ends, a year end, leap years).
//! No model is involved.
//!
//! **Not guessed.** A phrase that has two honest readings comes back as
//! [`Resolution::Either`] with both dates ("next Friday" said on a Thursday is
//! tomorrow or a week on Friday). A phrase that names no single day ("next
//! week", "soon", "in a few days") is not resolved at all. The accepted forms
//! are listed in [`resolve`]. Everything is English.
//!
//! **A date already on the page wins.** A weekday written next to a month and
//! a day ("Friday, Sep 25", "Sep 25 (Friday)", "Friday the 25th") is skipped
//! by [`find`], so a label never contradicts the date the text already gives.

use crate::connectors::civil_time::{civil_from_days, days_from_civil, CivilDate};

/// The first day of the week, which decides what "this week" and "next week"
/// mean. Monday is the default here (ISO 8601); Sunday is common in the US.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekStart {
    Monday,
    Sunday,
}

/// What a phrase works out to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// One date.
    One(CivilDate),
    /// Two honest readings, earlier first. Neither is picked.
    Either(CivilDate, CivilDate),
}

/// A phrase found in some text and what it works out to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// The phrase as written (original letter case, spaces collapsed).
    pub phrase: String,
    pub resolution: Resolution,
}

/// The most text [`find`] reads, in characters. Anything past it is ignored.
pub const MAX_SCAN_CHARS: usize = 50_000;
/// The most distinct phrases [`find`] returns.
pub const MAX_FOUND: usize = 100;
/// The longest phrase, in words.
const MAX_WORDS: usize = 5;
/// The biggest number accepted in "in N days" and the like.
const MAX_COUNT: i64 = 1000;

// -- calendar arithmetic -----------------------------------------------------

fn valid(d: CivilDate) -> bool {
    (1..=12).contains(&d.month) && d.day >= 1 && u32::from(d.day) <= month_len(d.year, d.month)
}

fn is_leap(y: i32) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn month_len(y: i32, m: u8) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap(y) => 29,
        _ => 28,
    }
}

fn to_days(d: CivilDate) -> i64 {
    days_from_civil(i64::from(d.year), u32::from(d.month), u32::from(d.day))
}

fn from_days(days: i64) -> CivilDate {
    let (y, m, d) = civil_from_days(days);
    CivilDate {
        year: y as i32,
        month: m as u8,
        day: d as u8,
    }
}

/// Weekday of a day number, 0 is Monday.
fn weekday_of(days: i64) -> i64 {
    // 1970-01-01 was a Thursday.
    (days + 3).rem_euclid(7)
}

/// A date `n` months on (or back), the day kept when the month has it and
/// otherwise the last day of that month: 31 Jan plus one month is 28 Feb
/// (29 in a leap year).
fn add_months(d: CivilDate, n: i64) -> CivilDate {
    let index = i64::from(d.year) * 12 + i64::from(d.month) - 1 + n;
    let year = index.div_euclid(12) as i32;
    let month = (index.rem_euclid(12) + 1) as u8;
    let day = d.day.min(month_len(year, month) as u8);
    CivilDate { year, month, day }
}

fn last_of_month(d: CivilDate, months_on: i64) -> CivilDate {
    let m = add_months(CivilDate { day: 1, ..d }, months_on);
    CivilDate {
        day: month_len(m.year, m.month) as u8,
        ..m
    }
}

/// Position of a weekday within a week that starts on `ws` (0 is the first day).
fn week_pos(weekday: i64, ws: WeekStart) -> i64 {
    match ws {
        WeekStart::Monday => weekday,
        WeekStart::Sunday => (weekday + 1) % 7,
    }
}

/// The day number of weekday `target` in the same week as `today`.
fn in_this_week(today: i64, target: i64, ws: WeekStart) -> i64 {
    today - week_pos(weekday_of(today), ws) + week_pos(target, ws)
}

// -- labels ------------------------------------------------------------------

const WEEKDAY_SHORT: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
const MONTH_SHORT: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// "Tue Sep 29", with the year added ("Fri Jan 1, 2027") when it is not the
/// year of `today`. An impossible date is shown as numbers rather than
/// panicking.
pub fn label(date: CivilDate, today: CivilDate) -> String {
    if !valid(date) {
        return format!("{:04}-{:02}-{:02}", date.year, date.month, date.day);
    }
    let wd = WEEKDAY_SHORT[weekday_of(to_days(date)) as usize];
    let month = MONTH_SHORT[usize::from(date.month) - 1];
    if date.year == today.year {
        format!("{wd} {month} {}", date.day)
    } else {
        format!("{wd} {month} {}, {}", date.day, date.year)
    }
}

/// [`label`] for one date, or both readings joined with " or ".
pub fn describe(resolution: &Resolution, today: CivilDate) -> String {
    match resolution {
        Resolution::One(d) => label(*d, today),
        Resolution::Either(a, b) => format!("{} or {}", label(*a, today), label(*b, today)),
    }
}

// -- words -------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct Tok {
    /// Lowercased.
    text: String,
    start: usize,
    end: usize,
}

/// Runs of letters and digits, with their byte positions.
fn tokenize(s: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    for (i, c) in s.char_indices() {
        if c.is_alphanumeric() {
            start.get_or_insert(i);
        } else if let Some(st) = start.take() {
            out.push(Tok {
                text: s[st..i].to_lowercase(),
                start: st,
                end: i,
            });
        }
    }
    if let Some(st) = start {
        out.push(Tok {
            text: s[st..].to_lowercase(),
            start: st,
            end: s.len(),
        });
    }
    out
}

/// The phrase parser's result: how many words it used, what they mean, and
/// whether it was a weekday phrase (those are skipped next to a written date).
struct Parsed {
    consumed: usize,
    resolution: Resolution,
    weekday_phrase: bool,
}

#[derive(Clone, Copy)]
enum Unit {
    Day,
    Week,
    Month,
    Year,
}

fn unit_of(t: &str) -> Option<Unit> {
    match t {
        "day" | "days" => Some(Unit::Day),
        "week" | "weeks" => Some(Unit::Week),
        "month" | "months" => Some(Unit::Month),
        "year" | "years" => Some(Unit::Year),
        _ => None,
    }
}

/// 1 to [`MAX_COUNT`], as digits or `a`, `an`, `one` to `twelve`.
fn count_of(t: &str) -> Option<i64> {
    let n = match t {
        "a" | "an" | "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        digits if digits.len() <= 4 && digits.bytes().all(|b| b.is_ascii_digit()) => {
            digits.parse().ok()?
        }
        _ => return None,
    };
    (1..=MAX_COUNT).contains(&n).then_some(n)
}

/// 0 is Monday. Abbreviations are only taken when `abbreviated` is set, and
/// never `sat` or `sun`.
fn weekday_name(t: &str, abbreviated: bool) -> Option<i64> {
    match t {
        "monday" => Some(0),
        "tuesday" => Some(1),
        "wednesday" => Some(2),
        "thursday" => Some(3),
        "friday" => Some(4),
        "saturday" => Some(5),
        "sunday" => Some(6),
        "mon" if abbreviated => Some(0),
        "tue" | "tues" if abbreviated => Some(1),
        "wed" if abbreviated => Some(2),
        "thu" | "thur" | "thurs" if abbreviated => Some(3),
        "fri" if abbreviated => Some(4),
        _ => None,
    }
}

/// `today` moved by `n` units, forward (`sign` 1) or back (`sign` -1).
fn shifted(today: i64, n: i64, unit: Unit, sign: i64) -> CivilDate {
    let base = from_days(today);
    match unit {
        Unit::Day => from_days(today + sign * n),
        Unit::Week => from_days(today + sign * n * 7),
        Unit::Month => add_months(base, sign * n),
        Unit::Year => add_months(base, sign * n * 12),
    }
}

/// "this", "next" or "last" before a weekday. See [`resolve`] for the rules:
/// when the weekday has two honest readings both are returned, earlier first.
fn modified_weekday(kind: &str, target: i64, today: i64, ws: WeekStart) -> Option<Resolution> {
    let cur = in_this_week(today, target, ws);
    let day = from_days;
    Some(match kind {
        "this" if cur >= today => Resolution::One(day(cur)),
        "this" => Resolution::Either(day(cur), day(cur + 7)),
        "next" if cur > today => Resolution::Either(day(cur), day(cur + 7)),
        "next" => Resolution::One(day(cur + 7)),
        "last" if cur < today => Resolution::Either(day(cur - 7), day(cur)),
        "last" => Resolution::One(day(cur - 7)),
        _ => return None,
    })
}

/// A weekday of a named week: "Friday next week".
fn weekday_of_week(kind: &str, target: i64, today: i64, ws: WeekStart) -> Option<CivilDate> {
    let shift = match kind {
        "this" => 0,
        "next" => 7,
        "last" => -7,
        _ => return None,
    };
    Some(from_days(in_this_week(today, target, ws) + shift))
}

/// Parses a phrase at the start of `w` (lowercase words).
fn parse(w: &[&str], today: i64, ws: WeekStart) -> Option<Parsed> {
    let td = from_days(today);
    let one = |consumed: usize, date: CivilDate| {
        Some(Parsed {
            consumed,
            resolution: Resolution::One(date),
            weekday_phrase: false,
        })
    };
    let weekday_phrase = |consumed: usize, resolution: Resolution| {
        Some(Parsed {
            consumed,
            resolution,
            weekday_phrase: true,
        })
    };
    let first = *w.first()?;
    let second = w.get(1).copied().unwrap_or("");
    let third = w.get(2).copied().unwrap_or("");
    match first {
        "today" | "tonight" => one(1, td),
        "tomorrow" => one(1, from_days(today + 1)),
        "yesterday" => one(1, from_days(today - 1)),
        "eom" => one(1, last_of_month(td, 0)),
        "eoy" => one(
            1,
            CivilDate {
                month: 12,
                day: 31,
                ..td
            },
        ),
        "the" if second == "day" => {
            let mut p = parse(&w[1..], today, ws)?;
            p.consumed += 1;
            Some(p)
        }
        "day" if second == "after" && third == "tomorrow" => one(3, from_days(today + 2)),
        "day" if second == "before" && third == "yesterday" => one(3, from_days(today - 2)),
        "this" if matches!(second, "morning" | "afternoon" | "evening") => one(2, td),
        "this" | "next" | "last" => {
            let target = weekday_name(second, true)?;
            weekday_phrase(2, modified_weekday(first, target, today, ws)?)
        }
        "in" => {
            let n = count_of(second)?;
            let unit = unit_of(third)?;
            one(3, shifted(today, n, unit, 1))
        }
        "end" if second == "of" => {
            // "end of [the|this|next] month|year|week".
            let (consumed, period) = match third {
                "the" | "this" | "next" => (4, w.get(3).copied().unwrap_or("")),
                other => (3, other),
            };
            let on = i64::from(third == "next");
            match period {
                "month" => one(consumed, last_of_month(td, on)),
                "year" => one(
                    consumed,
                    CivilDate {
                        year: td.year + on as i32,
                        month: 12,
                        day: 31,
                    },
                ),
                "week" => {
                    let start = today - week_pos(weekday_of(today), ws) + 7 * on;
                    let friday = from_days(in_this_week(today, 4, ws) + 7 * on);
                    Some(Parsed {
                        consumed,
                        resolution: Resolution::Either(friday, from_days(start + 6)),
                        weekday_phrase: false,
                    })
                }
                _ => None,
            }
        }
        _ => {
            if let Some(target) = weekday_name(first, false) {
                if third == "week" {
                    if let Some(date) = weekday_of_week(second, target, today, ws) {
                        return weekday_phrase(3, Resolution::One(date));
                    }
                }
                // A bare weekday is the next one; on that very day it could
                // be today or a week on.
                let ahead = (target - weekday_of(today)).rem_euclid(7);
                return weekday_phrase(
                    1,
                    if ahead == 0 {
                        Resolution::Either(td, from_days(today + 7))
                    } else {
                        Resolution::One(from_days(today + ahead))
                    },
                );
            }
            // "3 days ago", "a week from now".
            let n = count_of(first)?;
            let unit = unit_of(second)?;
            match (third, w.get(3).copied().unwrap_or("")) {
                ("ago", _) => one(3, shifted(today, n, unit, -1)),
                ("from", "now" | "today") => one(4, shifted(today, n, unit, 1)),
                _ => None,
            }
        }
    }
}

// -- public API --------------------------------------------------------------

/// The week start for a Windows `LOCALE_IFIRSTDAYOFWEEK` value (0 is Monday,
/// 6 is Sunday). Sunday for 6, Monday for everything else. A locale whose week
/// starts on Saturday (5) is read as Monday, which is the nearer of the two.
pub fn week_start_from_locale(first_day_of_week: u32) -> WeekStart {
    if first_day_of_week == 6 {
        WeekStart::Sunday
    } else {
        WeekStart::Monday
    }
}

/// What the single phrase `phrase` works out to against `today`, or `None`
/// when it is not one of the forms [`find`] accepts or has anything else in
/// it. [`find`] is the entry point in the app; this is the same parser over
/// one phrase.
#[cfg(test)]
pub fn resolve(phrase: &str, today: CivilDate, week: WeekStart) -> Option<Resolution> {
    if !valid(today) {
        return None;
    }
    let toks = tokenize(phrase);
    // Words joined by anything but spaces ("in -3 days") are not a phrase.
    if toks.windows(2).any(|p| {
        !phrase[p[0].end..p[1].start]
            .chars()
            .all(char::is_whitespace)
    }) {
        return None;
    }
    let words: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
    let parsed = parse(&words, to_days(today), week)?;
    (parsed.consumed == words.len()).then_some(parsed.resolution)
}

/// Finds the relative-date phrases in `text`, in order, each distinct phrase
/// once (letter case ignored). Reads at most [`MAX_SCAN_CHARS`] characters and
/// returns at most [`MAX_FOUND`] phrases. An invalid `today` finds nothing.
///
/// Accepted (case does not matter): `today`, `tonight`, `tomorrow`,
/// `yesterday`, `the day after tomorrow`, `the day before yesterday`,
/// `this morning|afternoon|evening`; `in N days|weeks|months|years`,
/// `N days|weeks|months|years ago`, `N ... from now|today` (N is 1 to 1000, as
/// digits or `a`, `an`, `one` to `twelve`); `this|next|last <weekday>`;
/// `<weekday> this|next|last week`; a bare `<weekday>`; `end of
/// the|this|next month|year|week`, `eom`, `eoy`.
///
/// Weekdays are written in full. `Mon`, `Tue(s)`, `Wed`, `Thu(r)(s)` and
/// `Fri` are also accepted after `this`, `next` or `last`. `Sat` and `Sun` are
/// not: they are ordinary words.
///
/// What each form means: "next Tuesday" is next week's Tuesday when this
/// week's has passed, and both readings when it is still ahead ("next Friday"
/// said on a Thursday). "this Friday" is this week's Friday, or both readings
/// when it has passed. "last Friday" mirrors "next". A bare weekday is the next
/// one, or today and a week on when it is today. "in 1 month" keeps the day, or
/// the month's last day when it is shorter (31 Jan is 28 Feb). "end of the
/// week" is Friday or the last day of the week.
pub fn find(text: &str, today: CivilDate, week: WeekStart) -> Vec<Found> {
    if !valid(today) {
        return Vec::new();
    }
    let cut = text
        .char_indices()
        .nth(MAX_SCAN_CHARS)
        .map_or(text.len(), |(i, _)| i);
    let text = &text[..cut];
    let toks = tokenize(text);
    let today_days = to_days(today);
    let mut found: Vec<Found> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut i = 0;
    while i < toks.len() && found.len() < MAX_FOUND {
        let mut end = i + 1;
        while end < toks.len()
            && end - i < MAX_WORDS
            && text[toks[end - 1].end..toks[end].start]
                .chars()
                .all(char::is_whitespace)
        {
            end += 1;
        }
        let words: Vec<&str> = toks[i..end].iter().map(|t| t.text.as_str()).collect();
        if let Some(p) = parse(&words, today_days, week) {
            let last = i + p.consumed - 1;
            if !(p.weekday_phrase && near_written_date(&toks, i, last)) {
                let phrase = toks[i..=last]
                    .iter()
                    .map(|t| &text[t.start..t.end])
                    .collect::<Vec<_>>()
                    .join(" ");
                if seen.insert(phrase.to_lowercase()) {
                    found.push(Found {
                        phrase,
                        resolution: p.resolution,
                    });
                }
                i = last + 1;
                continue;
            }
        }
        i += 1;
    }
    found
}

// -- a date already written next to a weekday --------------------------------

fn is_month(t: &str) -> bool {
    matches!(
        t,
        "january"
            | "february"
            | "march"
            | "april"
            | "may"
            | "june"
            | "july"
            | "august"
            | "september"
            | "october"
            | "november"
            | "december"
            | "jan"
            | "feb"
            | "mar"
            | "apr"
            | "jun"
            | "jul"
            | "aug"
            | "sep"
            | "sept"
            | "oct"
            | "nov"
            | "dec"
    )
}

/// 1 to 31, plain.
fn is_day_number(t: &str) -> bool {
    t.len() <= 2 && t.parse::<u8>().is_ok_and(|n| (1..=31).contains(&n))
}

/// `25th`, `1st`, `2nd`, `3rd`.
fn is_ordinal(t: &str) -> bool {
    ["st", "nd", "rd", "th"]
        .iter()
        .any(|suffix| t.strip_suffix(suffix).is_some_and(is_day_number))
}

/// Whether the words around the weekday phrase at `toks[first..=last]` form a
/// written date ("Sep 25", "25 September", "the 25th").
fn near_written_date(toks: &[Tok], first: usize, last: usize) -> bool {
    let word = |i: Option<usize>| i.and_then(|i| toks.get(i)).map_or("", |t| t.text.as_str());
    let (n0, n1) = (word(Some(last + 1)), word(Some(last + 2)));
    let p0 = word(first.checked_sub(1));
    let p1 = word(first.checked_sub(2));
    (is_month(n0) && is_day_number(n1))
        || (is_day_number(n0) && is_month(n1))
        || (n0 == "the" && is_ordinal(n1))
        || is_ordinal(n0)
        || (is_day_number(p0) && is_month(p1))
        || (is_month(p0) && is_day_number(p1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn d(year: i32, month: u8, day: u8) -> CivilDate {
        CivilDate { year, month, day }
    }

    fn one(year: i32, month: u8, day: u8) -> Option<Resolution> {
        Some(Resolution::One(d(year, month, day)))
    }

    fn either(a: (i32, u8, u8), b: (i32, u8, u8)) -> Option<Resolution> {
        Some(Resolution::Either(d(a.0, a.1, a.2), d(b.0, b.1, b.2)))
    }

    /// Wednesday 23 September 2026.
    const WED: CivilDate = CivilDate {
        year: 2026,
        month: 9,
        day: 23,
    };

    fn r(phrase: &str, today: CivilDate) -> Option<Resolution> {
        resolve(phrase, today, WeekStart::Monday)
    }

    fn phrases(text: &str, today: CivilDate) -> Vec<String> {
        find(text, today, WeekStart::Monday)
            .into_iter()
            .map(|f| f.phrase)
            .collect()
    }

    // -- the calendar is what the tests assume ---------------------------------

    #[test]
    fn the_reference_days_are_the_weekdays_the_tests_say() {
        assert_eq!(weekday_of(to_days(WED)), 2, "2026-09-23 is a Wednesday");
        assert_eq!(weekday_of(to_days(d(2026, 9, 29))), 1, "a Tuesday");
        assert_eq!(weekday_of(to_days(d(2026, 9, 27))), 6, "a Sunday");
        assert_eq!(weekday_of(to_days(d(2026, 12, 28))), 0, "a Monday");
    }

    // -- single days -------------------------------------------------------------

    #[test]
    fn today_tomorrow_and_yesterday() {
        assert_eq!(r("today", WED), one(2026, 9, 23));
        assert_eq!(r("tonight", WED), one(2026, 9, 23));
        assert_eq!(r("this morning", WED), one(2026, 9, 23));
        assert_eq!(r("this afternoon", WED), one(2026, 9, 23));
        assert_eq!(r("this evening", WED), one(2026, 9, 23));
        assert_eq!(r("tomorrow", WED), one(2026, 9, 24));
        assert_eq!(r("yesterday", WED), one(2026, 9, 22));
        assert_eq!(r("the day after tomorrow", WED), one(2026, 9, 25));
        assert_eq!(r("day after tomorrow", WED), one(2026, 9, 25));
        assert_eq!(r("the day before yesterday", WED), one(2026, 9, 21));
    }

    #[test]
    fn letter_case_and_spacing_do_not_matter() {
        assert_eq!(r("TOMORROW", WED), one(2026, 9, 24));
        assert_eq!(r("Next   Tuesday", WED), one(2026, 9, 29));
        assert_eq!(r("In 3\nDays", WED), one(2026, 9, 26));
    }

    #[test]
    fn tomorrow_crosses_a_month_and_a_year() {
        assert_eq!(r("tomorrow", d(2026, 9, 30)), one(2026, 10, 1));
        assert_eq!(r("tomorrow", d(2026, 12, 31)), one(2027, 1, 1));
        assert_eq!(r("yesterday", d(2027, 1, 1)), one(2026, 12, 31));
        assert_eq!(r("yesterday", d(2026, 10, 1)), one(2026, 9, 30));
    }

    #[test]
    fn tomorrow_across_february_in_leap_and_common_years() {
        assert_eq!(r("tomorrow", d(2028, 2, 28)), one(2028, 2, 29));
        assert_eq!(r("tomorrow", d(2028, 2, 29)), one(2028, 3, 1));
        assert_eq!(r("tomorrow", d(2027, 2, 28)), one(2027, 3, 1));
        assert_eq!(
            r("tomorrow", d(2100, 2, 28)),
            one(2100, 3, 1),
            "2100 is not a leap year"
        );
        assert_eq!(r("tomorrow", d(2000, 2, 28)), one(2000, 2, 29), "2000 is");
    }

    // -- counted offsets -----------------------------------------------------------

    #[test]
    fn in_n_days_weeks_months_and_years() {
        assert_eq!(r("in 3 days", WED), one(2026, 9, 26));
        assert_eq!(r("in 1 day", WED), one(2026, 9, 24));
        assert_eq!(r("in one day", WED), one(2026, 9, 24));
        assert_eq!(r("in a week", WED), one(2026, 9, 30));
        assert_eq!(r("in an hour", WED), None, "hours are not days");
        assert_eq!(r("in 2 weeks", WED), one(2026, 10, 7));
        assert_eq!(r("in twelve days", WED), one(2026, 10, 5));
        assert_eq!(r("in 1 month", WED), one(2026, 10, 23));
        assert_eq!(r("in 4 months", WED), one(2027, 1, 23));
        assert_eq!(r("in 12 months", WED), one(2027, 9, 23));
        assert_eq!(r("in a year", WED), one(2027, 9, 23));
    }

    #[test]
    fn in_3_days_crosses_a_month_and_a_year() {
        assert_eq!(r("in 3 days", d(2026, 9, 29)), one(2026, 10, 2));
        assert_eq!(r("in 3 days", d(2026, 12, 30)), one(2027, 1, 2));
        assert_eq!(r("in 365 days", d(2026, 9, 23)), one(2027, 9, 23));
        assert_eq!(
            r("in 366 days", d(2027, 9, 23)),
            one(2028, 9, 23),
            "through 29 Feb 2028"
        );
    }

    #[test]
    fn a_month_on_keeps_the_day_or_clamps_to_the_month_end() {
        assert_eq!(r("in 1 month", d(2028, 1, 31)), one(2028, 2, 29));
        assert_eq!(r("in 1 month", d(2027, 1, 31)), one(2027, 2, 28));
        assert_eq!(r("in 1 month", d(2026, 8, 31)), one(2026, 9, 30));
        assert_eq!(r("in 1 month", d(2026, 12, 31)), one(2027, 1, 31));
        assert_eq!(r("in 1 year", d(2028, 2, 29)), one(2029, 2, 28));
        assert_eq!(r("in 4 years", d(2028, 2, 29)), one(2032, 2, 29));
        assert_eq!(r("1 month ago", d(2026, 3, 31)), one(2026, 2, 28));
    }

    #[test]
    fn counted_offsets_back_and_from_now() {
        assert_eq!(r("5 days ago", WED), one(2026, 9, 18));
        assert_eq!(r("a week ago", WED), one(2026, 9, 16));
        assert_eq!(r("2 weeks ago", WED), one(2026, 9, 9));
        assert_eq!(r("3 months ago", WED), one(2026, 6, 23));
        assert_eq!(r("3 days from now", WED), one(2026, 9, 26));
        assert_eq!(r("2 weeks from today", WED), one(2026, 10, 7));
        assert_eq!(r("a week from now", WED), one(2026, 9, 30));
        assert_eq!(r("3 days ago", d(2026, 1, 2)), one(2025, 12, 30));
    }

    // -- weekdays -------------------------------------------------------------------

    #[test]
    fn next_weekday_that_has_passed_this_week_is_next_weeks() {
        // Wednesday: Monday and Tuesday are behind us.
        assert_eq!(r("next Tuesday", WED), one(2026, 9, 29));
        assert_eq!(r("next Monday", WED), one(2026, 9, 28));
        // The same weekday as today is a week on, not today.
        assert_eq!(r("next Wednesday", WED), one(2026, 9, 30));
    }

    #[test]
    fn next_weekday_still_ahead_this_week_shows_both_readings() {
        assert_eq!(r("next Friday", WED), either((2026, 9, 25), (2026, 10, 2)));
        assert_eq!(
            r("next Thursday", WED),
            either((2026, 9, 24), (2026, 10, 1))
        );
        assert_eq!(r("next Sunday", WED), either((2026, 9, 27), (2026, 10, 4)));
    }

    #[test]
    fn this_weekday() {
        assert_eq!(r("this Friday", WED), one(2026, 9, 25));
        assert_eq!(r("this Wednesday", WED), one(2026, 9, 23));
        assert_eq!(r("this Sunday", WED), one(2026, 9, 27));
        // Already passed this week: that day, or the one coming. Both shown.
        assert_eq!(r("this Monday", WED), either((2026, 9, 21), (2026, 9, 28)));
        assert_eq!(r("this Tuesday", WED), either((2026, 9, 22), (2026, 9, 29)));
    }

    #[test]
    fn last_weekday() {
        assert_eq!(r("last Friday", WED), one(2026, 9, 18));
        assert_eq!(r("last Wednesday", WED), one(2026, 9, 16));
        assert_eq!(r("last Sunday", WED), one(2026, 9, 20));
        // Earlier this week: that day, or the one a week before. Both shown.
        assert_eq!(r("last Tuesday", WED), either((2026, 9, 15), (2026, 9, 22)));
        assert_eq!(r("last Monday", WED), either((2026, 9, 14), (2026, 9, 21)));
    }

    #[test]
    fn a_bare_weekday_is_the_next_one() {
        assert_eq!(r("Friday", WED), one(2026, 9, 25));
        assert_eq!(r("Tuesday", WED), one(2026, 9, 29));
        assert_eq!(r("Sunday", WED), one(2026, 9, 27));
        // On that very day it could mean today or a week on.
        assert_eq!(r("Wednesday", WED), either((2026, 9, 23), (2026, 9, 30)));
    }

    #[test]
    fn weekday_with_a_week_is_exact() {
        assert_eq!(r("Friday this week", WED), one(2026, 9, 25));
        assert_eq!(r("Friday next week", WED), one(2026, 10, 2));
        assert_eq!(r("Monday this week", WED), one(2026, 9, 21));
        assert_eq!(r("Tuesday last week", WED), one(2026, 9, 15));
        assert_eq!(r("Monday next week", WED), one(2026, 9, 28));
    }

    #[test]
    fn a_weekday_across_a_year_end() {
        let mon = d(2026, 12, 28);
        assert_eq!(r("next Friday", mon), either((2027, 1, 1), (2027, 1, 8)));
        assert_eq!(r("Friday next week", mon), one(2027, 1, 8));
        assert_eq!(r("next Monday", mon), one(2027, 1, 4));
        assert_eq!(r("in 5 days", mon), one(2027, 1, 2));
    }

    #[test]
    fn abbreviations_work_after_this_next_or_last_but_not_sat_or_sun() {
        assert_eq!(r("next tue", WED), one(2026, 9, 29));
        assert_eq!(r("next tues", WED), one(2026, 9, 29));
        assert_eq!(r("this fri", WED), one(2026, 9, 25));
        assert_eq!(r("last thurs", WED), one(2026, 9, 17));
        assert_eq!(r("next mon", WED), one(2026, 9, 28));
        assert_eq!(r("this wed", WED), one(2026, 9, 23));
        assert_eq!(r("next sat", WED), None);
        assert_eq!(r("this sun", WED), None);
        assert_eq!(r("fri", WED), None, "a bare abbreviation is not a phrase");
        assert_eq!(r("wed", WED), None);
    }

    #[test]
    fn the_week_start_changes_this_and_next_week() {
        let sun = d(2026, 9, 27);
        // Monday-first: the week of Sunday 27 Sep began Monday 21 Sep.
        assert_eq!(
            resolve("this Monday", sun, WeekStart::Monday),
            either((2026, 9, 21), (2026, 9, 28))
        );
        assert_eq!(
            resolve("Friday this week", sun, WeekStart::Monday),
            one(2026, 9, 25)
        );
        assert_eq!(
            resolve("Friday next week", sun, WeekStart::Monday),
            one(2026, 10, 2)
        );
        // Sunday-first: that same Sunday starts a new week.
        assert_eq!(
            resolve("this Monday", sun, WeekStart::Sunday),
            one(2026, 9, 28)
        );
        assert_eq!(
            resolve("Friday this week", sun, WeekStart::Sunday),
            one(2026, 10, 2)
        );
        assert_eq!(
            resolve("Friday last week", sun, WeekStart::Sunday),
            one(2026, 9, 25)
        );
        assert_eq!(
            resolve("Friday next week", sun, WeekStart::Sunday),
            one(2026, 10, 9)
        );
    }

    #[test]
    fn the_windows_first_day_of_week_picks_the_week_start() {
        assert_eq!(week_start_from_locale(0), WeekStart::Monday);
        assert_eq!(week_start_from_locale(6), WeekStart::Sunday);
        assert_eq!(
            week_start_from_locale(5),
            WeekStart::Monday,
            "Saturday is read as Monday"
        );
        assert_eq!(
            week_start_from_locale(99),
            WeekStart::Monday,
            "garbage is read as Monday"
        );
    }

    // -- ends of periods --------------------------------------------------------------

    #[test]
    fn end_of_the_month() {
        assert_eq!(r("end of the month", WED), one(2026, 9, 30));
        assert_eq!(r("end of month", WED), one(2026, 9, 30));
        assert_eq!(r("end of this month", WED), one(2026, 9, 30));
        assert_eq!(r("EOM", WED), one(2026, 9, 30));
        assert_eq!(r("end of the month", d(2026, 9, 30)), one(2026, 9, 30));
        assert_eq!(r("end of the month", d(2026, 12, 3)), one(2026, 12, 31));
    }

    #[test]
    fn end_of_the_month_in_february() {
        assert_eq!(r("end of the month", d(2028, 2, 10)), one(2028, 2, 29));
        assert_eq!(r("end of the month", d(2027, 2, 10)), one(2027, 2, 28));
        assert_eq!(r("end of next month", d(2027, 1, 15)), one(2027, 2, 28));
        assert_eq!(r("end of next month", d(2028, 1, 15)), one(2028, 2, 29));
    }

    #[test]
    fn end_of_next_month_and_year() {
        assert_eq!(r("end of next month", WED), one(2026, 10, 31));
        assert_eq!(r("end of next month", d(2026, 12, 15)), one(2027, 1, 31));
        assert_eq!(r("end of the year", WED), one(2026, 12, 31));
        assert_eq!(r("end of year", WED), one(2026, 12, 31));
        assert_eq!(r("EOY", d(2027, 3, 1)), one(2027, 12, 31));
        assert_eq!(r("end of next year", WED), one(2027, 12, 31));
    }

    #[test]
    fn end_of_the_week_is_friday_or_the_last_day_of_the_week() {
        assert_eq!(
            r("end of the week", WED),
            either((2026, 9, 25), (2026, 9, 27))
        );
        assert_eq!(
            r("end of next week", WED),
            either((2026, 10, 2), (2026, 10, 4))
        );
        assert_eq!(
            resolve("end of the week", WED, WeekStart::Sunday),
            either((2026, 9, 25), (2026, 9, 26))
        );
    }

    // -- not resolved rather than guessed -----------------------------------------------

    #[test]
    fn vague_phrases_are_not_resolved() {
        for p in [
            "",
            "soon",
            "later",
            "next week",
            "next month",
            "last week",
            "next year",
            "this week",
            "the weekend",
            "next weekend",
            "in a few days",
            "in a couple of weeks",
            "in days",
            "in 0 days",
            "in 1001 days",
            "in 99999999999999999999 days",
            "in -3 days",
            "in 3",
            "3 days",
            "days ago",
            "next",
            "this",
            "end of",
            "end of the",
            "end of the day",
            "Fridays",
            "tomorrowland",
            "yesterdays",
            "next Tuesdays",
            "someday",
        ] {
            assert_eq!(r(p, WED), None, "{p:?} must not resolve");
        }
    }

    #[test]
    fn a_phrase_with_anything_else_in_it_is_not_resolved() {
        assert_eq!(r("next Tuesday please", WED), None);
        assert_eq!(r("see you tomorrow", WED), None);
        assert_eq!(r("tomorrow, Friday", WED), None);
    }

    #[test]
    fn an_impossible_reference_date_resolves_nothing() {
        for bad in [
            d(2026, 13, 1),
            d(2026, 0, 5),
            d(2026, 2, 30),
            d(2027, 2, 29),
            d(2026, 9, 0),
        ] {
            assert_eq!(r("tomorrow", bad), None, "{bad:?}");
            assert!(
                find("tomorrow", bad, WeekStart::Monday).is_empty(),
                "{bad:?}"
            );
        }
    }

    // -- finding phrases in text ----------------------------------------------------------

    #[test]
    fn finds_each_phrase_in_order() {
        let text = "Let's meet next Tuesday, then ship tomorrow or in 3 days. \
                    The report is due end of the month.";
        assert_eq!(
            phrases(text, WED),
            ["next Tuesday", "tomorrow", "in 3 days", "end of the month"]
        );
        let found = find(text, WED, WeekStart::Monday);
        assert_eq!(found[0].resolution, Resolution::One(d(2026, 9, 29)));
        assert_eq!(found[3].resolution, Resolution::One(d(2026, 9, 30)));
    }

    #[test]
    fn the_phrase_keeps_its_letter_case_and_collapses_spaces() {
        assert_eq!(
            phrases("NEXT   TUESDAY\nor in 3\n days", WED),
            ["NEXT TUESDAY", "in 3 days"]
        );
    }

    #[test]
    fn a_repeated_phrase_is_listed_once() {
        assert_eq!(
            phrases("Tomorrow. And tomorrow. And TOMORROW.", WED),
            ["Tomorrow"]
        );
    }

    #[test]
    fn punctuation_inside_a_phrase_breaks_it() {
        assert!(phrases("in 3, days", WED).is_empty());
        assert_eq!(
            phrases("next. Tuesday", WED),
            ["Tuesday"],
            "only the bare weekday"
        );
        assert_eq!(phrases("(tomorrow)", WED), ["tomorrow"]);
    }

    #[test]
    fn words_that_only_contain_a_phrase_are_not_found() {
        assert!(phrases("Tomorrowland opens. Fridays are fun. Mintoday.", WED).is_empty());
        assert!(
            phrases("admin 3 days ago", WED).len() == 1,
            "only '3 days ago' is a phrase"
        );
    }

    #[test]
    fn a_weekday_next_to_a_written_date_is_left_alone() {
        for text in [
            "Friday, Sep 25",
            "Friday Sep 25",
            "Friday 25 September",
            "Friday the 25th",
            "Friday 25th",
            "Sep 25 (Friday)",
            "Sep 25, Friday",
            "25 Sep Friday",
            "next Friday, Sep 25",
            "Tuesday the 1st",
        ] {
            assert!(phrases(text, WED).is_empty(), "{text:?}");
        }
    }

    #[test]
    fn a_weekday_with_a_time_or_ordinary_words_is_still_found() {
        assert_eq!(phrases("Friday 5pm", WED), ["Friday"]);
        assert_eq!(phrases("Friday at 5", WED), ["Friday"]);
        assert_eq!(phrases("Friday may work", WED), ["Friday"]);
        assert_eq!(phrases("on Friday the meeting moves", WED), ["Friday"]);
        assert_eq!(phrases("by Friday 3 items are due", WED), ["Friday"]);
    }

    #[test]
    fn tomorrow_beside_a_written_date_is_still_found() {
        assert_eq!(phrases("Due tomorrow (Sep 24)", WED), ["tomorrow"]);
    }

    #[test]
    fn a_longer_phrase_wins_over_a_shorter_one_inside_it() {
        assert_eq!(phrases("Friday next week", WED), ["Friday next week"]);
        assert_eq!(
            phrases("the day after tomorrow", WED),
            ["the day after tomorrow"]
        );
        assert_eq!(phrases("end of next month", WED), ["end of next month"]);
    }

    #[test]
    fn multibyte_text_is_not_cut_in_the_middle_of_a_character() {
        assert_eq!(
            phrases(
                "Caf\u{e9} na\u{ef}ve \u{65e5}\u{672c} tomorrow \u{1f600}",
                WED
            ),
            ["tomorrow"]
        );
        assert!(phrases("\u{130}\u{130} in 3 days", WED).len() <= 1);
        assert!(phrases("\u{1f600}\u{1f600}", WED).is_empty());
    }

    #[test]
    fn empty_and_odd_input_find_nothing() {
        assert!(phrases("", WED).is_empty());
        assert!(phrases("   \n\t ", WED).is_empty());
        assert!(phrases("!!! ??? ...", WED).is_empty());
        assert!(phrases("12345 67890", WED).is_empty());
    }

    #[test]
    fn a_huge_text_is_cut_and_still_fast() {
        let text = "tomorrow ".repeat(200_000);
        let found = find(&text, WED, WeekStart::Monday);
        assert_eq!(found.len(), 1);
        let late = format!("{} tomorrow", "x ".repeat(MAX_SCAN_CHARS));
        assert!(
            find(&late, WED, WeekStart::Monday).is_empty(),
            "past the scan limit"
        );
    }

    #[test]
    fn many_distinct_phrases_are_capped() {
        let text: String = (1..=500).map(|n| format!("in {n} days. ")).collect();
        assert_eq!(find(&text, WED, WeekStart::Monday).len(), MAX_FOUND);
    }

    // -- labels ---------------------------------------------------------------------------

    #[test]
    fn labels_name_the_weekday_and_add_the_year_only_when_it_differs() {
        assert_eq!(label(d(2026, 9, 29), WED), "Tue Sep 29");
        assert_eq!(label(d(2027, 1, 1), WED), "Fri Jan 1, 2027");
        assert_eq!(label(d(2028, 2, 29), d(2028, 1, 1)), "Tue Feb 29");
        assert_eq!(
            label(d(2026, 2, 30), WED),
            "2026-02-30",
            "an impossible date is not mislabelled"
        );
    }

    #[test]
    fn two_readings_are_joined_with_or() {
        let both = Resolution::Either(d(2026, 9, 25), d(2026, 10, 2));
        assert_eq!(describe(&both, WED), "Fri Sep 25 or Fri Oct 2");
        assert_eq!(
            describe(&Resolution::One(d(2026, 9, 29)), WED),
            "Tue Sep 29"
        );
    }
}
