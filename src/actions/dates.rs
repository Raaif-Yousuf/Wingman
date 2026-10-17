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
/// mean. Monday is the default here (ISO 8601); Sunday is common in the US and
/// Saturday in parts of the Middle East.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeekStart {
    Monday,
    Sunday,
    Saturday,
}

/// A time of day written after a date phrase, or worked out from the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Time {
    /// Minutes since midnight, 0 to 1439.
    pub minute_of_day: u16,
    /// True when the text gave no clock time and one was taken for it ("EOD"
    /// is 17:00). The card says so. A time written as digits is not assumed.
    pub assumed: bool,
}

impl Time {
    /// "17:00".
    pub fn label(self) -> String {
        format!(
            "{:02}:{:02}",
            self.minute_of_day / 60,
            self.minute_of_day % 60
        )
    }
}

/// What "EOD" and "end of day" are taken as: 17:00.
const END_OF_DAY: u16 = 17 * 60;
const MINUTES_PER_DAY: i64 = 24 * 60;

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
    /// True when the day was cut back to the last day of a shorter month
    /// ("in 1 month" from 31 Jan is 28 Feb). The card says so.
    pub clamped: bool,
    /// The time of day, when the text wrote one or an hour offset gave one.
    pub time: Option<Time>,
}

/// Said on the card after a clamped date.
pub const CLAMPED_NOTE: &str = "last day of a shorter month";

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
        WeekStart::Saturday => (weekday + 2) % 7,
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

/// [`describe`] for a found phrase, with ", 17:00" after the date when it has
/// a time.
pub fn describe_found(found: &Found, today: CivilDate) -> String {
    let date = describe(&found.resolution, today);
    match found.time {
        Some(t) => format!("{date}, {}", t.label()),
        None => date,
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
    /// A month or year step that landed on a shorter month, so the day was
    /// cut back to that month's last day.
    clamped: bool,
    /// Set by hour and minute offsets, which know the time they land on.
    time: Option<Time>,
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

/// Minutes in an hour or minute unit word.
fn clock_unit(t: &str) -> Option<i64> {
    match t {
        "hour" | "hours" | "hr" | "hrs" => Some(60),
        "minute" | "minutes" | "min" | "mins" => Some(1),
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
/// `now` is the minute of the day, for "in 2 hours"; without it an hour or
/// minute offset is not resolved.
fn parse(w: &[&str], today: i64, now: Option<u16>, ws: WeekStart) -> Option<Parsed> {
    let td = from_days(today);
    let one = |consumed: usize, date: CivilDate| {
        Some(Parsed {
            consumed,
            resolution: Resolution::One(date),
            weekday_phrase: false,
            clamped: false,
            time: None,
        })
    };
    let weekday_phrase = |consumed: usize, resolution: Resolution| {
        Some(Parsed {
            consumed,
            resolution,
            weekday_phrase: true,
            clamped: false,
            time: None,
        })
    };
    // An hour or minute offset from the clock, which can cross midnight.
    let clocked = |consumed: usize, minutes: i64, sign: i64| {
        let now = i64::from(now.filter(|m| i64::from(*m) < MINUTES_PER_DAY)?);
        let total = today * MINUTES_PER_DAY + now + sign * minutes;
        Some(Parsed {
            consumed,
            resolution: Resolution::One(from_days(total.div_euclid(MINUTES_PER_DAY))),
            weekday_phrase: false,
            clamped: false,
            time: Some(Time {
                minute_of_day: total.rem_euclid(MINUTES_PER_DAY) as u16,
                assumed: false,
            }),
        })
    };
    // A counted offset. When a month or year step lands on a shorter month
    // the day is cut back to its last day, and `clamped` says so.
    let counted = |consumed: usize, n: i64, unit: Unit, sign: i64| {
        let date = shifted(today, n, unit, sign);
        Some(Parsed {
            consumed,
            resolution: Resolution::One(date),
            weekday_phrase: false,
            clamped: matches!(unit, Unit::Month | Unit::Year) && date.day != td.day,
            time: None,
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
            let mut p = parse(w.get(1..)?, today, now, ws)?;
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
            if let Some(per) = clock_unit(third) {
                return clocked(3, n * per, 1);
            }
            let unit = unit_of(third)?;
            counted(3, n, unit, 1)
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
                    let last = from_days(start + 6);
                    // A Saturday-first week ends on its Friday: one day.
                    Some(Parsed {
                        consumed,
                        resolution: if friday == last {
                            Resolution::One(friday)
                        } else {
                            Resolution::Either(friday, last)
                        },
                        weekday_phrase: false,
                        clamped: false,
                        time: None,
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
                // A bare weekday has no tense: "met him Friday" is the one
                // just gone, "see you Friday" the one coming. Both are shown,
                // earlier first. On that very day it is today or a week on.
                let ahead = (target - weekday_of(today)).rem_euclid(7);
                return weekday_phrase(
                    1,
                    if ahead == 0 {
                        Resolution::Either(td, from_days(today + 7))
                    } else {
                        Resolution::Either(from_days(today + ahead - 7), from_days(today + ahead))
                    },
                );
            }
            // "3 days ago", "a week from now".
            let n = count_of(first)?;
            if let Some(per) = clock_unit(second) {
                return match (third, w.get(3).copied().unwrap_or("")) {
                    ("ago", _) => clocked(3, n * per, -1),
                    ("from", "now") => clocked(4, n * per, 1),
                    _ => None,
                };
            }
            let unit = unit_of(second)?;
            match (third, w.get(3).copied().unwrap_or("")) {
                ("ago", _) => counted(3, n, unit, -1),
                ("from", "now" | "today") => counted(4, n, unit, 1),
                _ => None,
            }
        }
    }
}

// -- public API --------------------------------------------------------------

/// The week start for a Windows `LOCALE_IFIRSTDAYOFWEEK` value (0 is Monday,
/// 5 is Saturday, 6 is Sunday). Anything else is Monday.
pub fn week_start_from_locale(first_day_of_week: u32) -> WeekStart {
    match first_day_of_week {
        5 => WeekStart::Saturday,
        6 => WeekStart::Sunday,
        _ => WeekStart::Monday,
    }
}

// -- a time of day after the phrase ------------------------------------------

/// Whether only whitespace lies between two tokens.
fn space_between(text: &str, a: &Tok, b: &Tok) -> bool {
    text.get(a.end..b.start)
        .is_some_and(|gap| !gap.is_empty() && gap.chars().all(char::is_whitespace))
}

/// Whether exactly a colon lies between two tokens.
fn colon_between(text: &str, a: &Tok, b: &Tok) -> bool {
    text.get(a.end..b.start) == Some(":")
}

/// Whether the gap between a phrase and the time after it is only spaces, or
/// one comma with spaces around it ("tomorrow, 5pm", "tomorrow,5pm").
fn time_gap(text: &str, a: &Tok, b: &Tok) -> bool {
    let Some(gap) = text.get(a.end..b.start) else {
        return false;
    };
    !gap.is_empty()
        && gap.chars().filter(|&c| c == ',').count() <= 1
        && gap.chars().all(|c| c.is_whitespace() || c == ',')
}

/// `am` is false, `pm` is true.
fn meridiem(t: &str) -> Option<bool> {
    match t {
        "am" => Some(false),
        "pm" => Some(true),
        _ => None,
    }
}

/// A meridiem that starts with `letter`, the text of (or the end of) the
/// token `toks[k]`: `am`, `pm`, or the dotted `a.m` and `p.m` (the dot
/// splits them into two tokens, `a` and `m`). Returns the last token used and
/// whether it is pm.
fn meridiem_from(text: &str, toks: &[Tok], k: usize, letter: &str) -> Option<(usize, bool)> {
    if let Some(pm) = meridiem(letter) {
        return Some((k, pm));
    }
    let pm = match letter {
        "a" => false,
        "p" => true,
        _ => return None,
    };
    let (this, next) = (toks.get(k)?, toks.get(k + 1)?);
    (next.text == "m" && text.get(this.end..next.start) == Some(".")).then_some((k + 1, pm))
}

/// An ASCII number of one or two digits at the start of `t`, and what follows
/// it. Anything else (more digits, other scripts) is not a clock number.
fn split_number(t: &str) -> Option<(u16, &str)> {
    let cut = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let digits = t.get(..cut)?;
    if digits.is_empty() || digits.len() > 2 {
        return None;
    }
    Some((digits.parse().ok()?, t.get(cut..)?))
}

/// A 12-hour clock time: hour 1 to 12 with am or pm.
fn twelve_hour(hour: u16, pm: bool) -> Option<u16> {
    (1..=12)
        .contains(&hour)
        .then_some(hour % 12 + if pm { 12 } else { 0 })
}

/// A clock time starting at `toks[j]`: `5pm`, `5 pm`, `5:30pm`, `5:30 pm`, or
/// 24-hour `17:00` and `09:15`. Returns the last token used and the minute of
/// the day. A time with no am or pm is only taken when the hour is written with
/// two digits and a colon, because "5:30" could be either half of the day.
fn clock_time(text: &str, toks: &[Tok], j: usize) -> Option<(usize, u16)> {
    let first = toks.get(j)?;
    let (hour, rest) = split_number(&first.text)?;
    // `5pm`, `5p.m.`.
    if let Some((last, pm)) = meridiem_from(text, toks, j, rest) {
        return Some((last, twelve_hour(hour, pm)? * 60));
    }
    if !rest.is_empty() {
        return None;
    }
    let next = toks.get(j + 1)?;
    // `5 pm`, `5 p.m.`.
    if let Some((last, pm)) = meridiem_from(text, toks, j + 1, &next.text) {
        if !space_between(text, first, next) {
            return None;
        }
        return Some((last, twelve_hour(hour, pm)? * 60));
    }
    // `5:30`, `5:30pm`, `5:30 pm`, `17:00`.
    if !colon_between(text, first, next) {
        return None;
    }
    let (minute, tail) = split_number(&next.text)?;
    // Minutes are written with two digits, and run 0 to 59.
    if next.text.len() - tail.len() != 2 || minute > 59 {
        return None;
    }
    let (last, pm) = match (meridiem_from(text, toks, j + 1, tail), tail.is_empty()) {
        (Some((last, pm)), _) => (last, Some(pm)),
        (None, true) => match toks.get(j + 2) {
            Some(after) if space_between(text, next, after) => {
                match meridiem_from(text, toks, j + 2, &after.text) {
                    Some((last, pm)) => (last, Some(pm)),
                    None => (j + 1, None),
                }
            }
            _ => (j + 1, None),
        },
        (None, false) => return None,
    };
    let hour24 = match pm {
        Some(pm) => twelve_hour(hour, pm)?,
        None => {
            if first.text.len() != 2 || hour > 23 {
                return None;
            }
            hour
        }
    };
    Some((last, hour24 * 60 + minute))
}

/// A time of day written right after the phrase that ends at `toks[from - 1]`:
/// `EOD`, `COB`, `end of day`, `noon` or a clock time, each with an optional
/// `at` or `by` before it. Returns the last token used. `None` when the next
/// words are anything else, so no time is shown rather than a wrong one.
fn read_time(text: &str, toks: &[Tok], from: usize) -> Option<(usize, Time)> {
    let prev = toks.get(from.checked_sub(1)?)?;
    let mut j = from;
    let first = toks.get(j)?;
    if !time_gap(text, prev, first) {
        return None;
    }
    if matches!(first.text.as_str(), "at" | "by") {
        let after = toks.get(j + 1)?;
        if !space_between(text, first, after) {
            return None;
        }
        j += 1;
    }
    let t = toks.get(j)?;
    let assumed = |last: usize| {
        Some((
            last,
            Time {
                minute_of_day: END_OF_DAY,
                assumed: true,
            },
        ))
    };
    match t.text.as_str() {
        "eod" | "cob" => return assumed(j),
        "noon" => {
            return Some((
                j,
                Time {
                    minute_of_day: 12 * 60,
                    assumed: false,
                },
            ))
        }
        "end" => {
            let (of, day) = (toks.get(j + 1)?, toks.get(j + 2)?);
            let spaced = space_between(text, t, of) && space_between(text, of, day);
            return (spaced && of.text == "of" && day.text == "day")
                .then(|| assumed(j + 2))
                .flatten();
        }
        _ => {}
    }
    let (last, minute_of_day) = clock_time(text, toks, j)?;
    Some((
        last,
        Time {
            minute_of_day,
            assumed: false,
        },
    ))
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
    let parsed = parse(&words, to_days(today), None, week)?;
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
/// when it has passed. "last Friday" mirrors "next". A bare weekday has no tense,
/// so it gives the one just gone and the one coming (today and a week on when it
/// is today). "in 1 month" keeps the day, or the month's last day when it is
/// shorter (31 Jan is 28 Feb); [`Found::clamped`] says when that happened. "end of the
/// week" is Friday or the last day of the week.
///
/// A date phrase can be followed by a time (#706): `EOD` or `COB` or `end of
/// day` (taken as 17:00, and marked [`Time::assumed`]), `noon`, or a clock
/// time such as `5pm`, `5 p.m.`, `9:30 am` or
/// `17:00`, with an optional `at` or `by`, and an optional comma after the date
/// (`tomorrow, 5pm`).
/// A time that could be read two ways ("at 5", or "midnight", which can mean
/// either end of the day) is left out and the date still
/// stands. `in N hours|minutes`, `N hours ago` and `N minutes from now` need
/// `now_minute`, the minutes since local midnight; without it they are not
/// resolved. An out-of-range `now_minute` counts as without.
pub fn find_at(
    text: &str,
    today: CivilDate,
    now_minute: Option<u16>,
    week: WeekStart,
) -> Vec<Found> {
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
        if let Some(p) = parse(&words, today_days, now_minute, week) {
            let mut last = i + p.consumed - 1;
            if !(p.weekday_phrase && near_written_date(&toks, i, last)) {
                // A time after the phrase, unless the phrase brought its own.
                let mut time = p.time;
                if time.is_none() {
                    if let Some((end, t)) = read_time(text, &toks, last + 1) {
                        last = end;
                        time = Some(t);
                    }
                }
                let mut phrase = String::new();
                for k in i..=last {
                    if k > i {
                        // Between words: a space, the colon in "5:30", the dot
                        // in "p.m", or the comma in "tomorrow, 5pm".
                        let gap = text.get(toks[k - 1].end..toks[k].start).unwrap_or(" ");
                        if gap == ":" || gap == "." {
                            phrase.push_str(gap);
                        } else if gap.contains(',') {
                            phrase.push(',');
                            if gap.ends_with(char::is_whitespace) {
                                phrase.push(' ');
                            }
                        } else {
                            phrase.push(' ');
                        }
                    }
                    phrase.push_str(&text[toks[k].start..toks[k].end]);
                }
                // The closing dot of "p.m.".
                if toks[last].text == "m"
                    && last > i
                    && text.get(toks[last - 1].end..toks[last].start) == Some(".")
                    && text
                        .get(toks[last].end..)
                        .is_some_and(|t| t.starts_with('.'))
                {
                    phrase.push('.');
                }
                if seen.insert(phrase.to_lowercase()) {
                    found.push(Found {
                        phrase,
                        resolution: p.resolution,
                        clamped: p.clamped,
                        time,
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

/// [`find_at`] without a clock: the date forms only.
#[cfg(test)]
pub fn find(text: &str, today: CivilDate, week: WeekStart) -> Vec<Found> {
    find_at(text, today, None, week)
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
    fn a_clamped_month_is_flagged_and_an_exact_one_is_not() {
        let wk = WeekStart::Monday;
        let jan31 = d(2027, 1, 31);
        let f = find("in 1 month", jan31, wk);
        assert_eq!(f[0].resolution, Resolution::One(d(2027, 2, 28)));
        assert!(f[0].clamped);
        assert!(find("1 month ago", d(2026, 3, 31), wk)[0].clamped);
        assert!(find("in 1 year", d(2028, 2, 29), wk)[0].clamped);
        assert!(!find("in 1 month", WED, wk)[0].clamped);
        assert!(!find("in 1 month", d(2026, 12, 31), wk)[0].clamped);
        assert!(
            !find("in 31 days", jan31, wk)[0].clamped,
            "days never clamp"
        );
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
    fn a_bare_weekday_shows_the_one_just_gone_and_the_one_coming() {
        assert_eq!(r("Friday", WED), either((2026, 9, 18), (2026, 9, 25)));
        assert_eq!(r("Tuesday", WED), either((2026, 9, 22), (2026, 9, 29)));
        assert_eq!(r("Sunday", WED), either((2026, 9, 20), (2026, 9, 27)));
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
        assert_eq!(week_start_from_locale(5), WeekStart::Saturday);
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
        // A clear time is part of the phrase (#706); "at 5" is not clear.
        assert_eq!(phrases("Friday 5pm", WED), ["Friday 5pm"]);
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
        assert_eq!(phrases("\u{130}\u{130} in 3 days", WED), ["in 3 days"]);
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
        // A phrase right at the start is found; a different one placed after
        // the scan limit is not, so the scan really stopped.
        let text = format!("tomorrow {}yesterday", "x ".repeat(MAX_SCAN_CHARS));
        assert_eq!(phrases(&text, WED), ["tomorrow"]);
        let repeated = "tomorrow ".repeat(200_000);
        assert_eq!(find(&repeated, WED, WeekStart::Monday).len(), 1);
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

    // -- Saturday-first weeks (#707) ------------------------------------------------------

    /// Saturday 26 September 2026: the first day of a Saturday-first week.
    const SAT: CivilDate = CivilDate {
        year: 2026,
        month: 9,
        day: 26,
    };

    fn rs(phrase: &str, today: CivilDate) -> Option<Resolution> {
        resolve(phrase, today, WeekStart::Saturday)
    }

    #[test]
    fn a_saturday_first_week_runs_saturday_to_friday() {
        assert_eq!(weekday_of(to_days(SAT)), 5, "2026-09-26 is a Saturday");
        // Monday-first and Sunday-first weeks both end on the 27th or 26th, so
        // "Friday this week" is the 25th there and the 2nd in a Saturday week.
        assert_eq!(rs("Friday this week", SAT), one(2026, 10, 2));
        assert_eq!(
            resolve("Friday this week", SAT, WeekStart::Monday),
            one(2026, 9, 25)
        );
        assert_eq!(
            resolve("Friday this week", SAT, WeekStart::Sunday),
            one(2026, 9, 25)
        );
        assert_eq!(rs("Friday next week", SAT), one(2026, 10, 9));
        assert_eq!(rs("Friday last week", SAT), one(2026, 9, 25));
        assert_eq!(rs("Saturday this week", SAT), one(2026, 9, 26));
        assert_eq!(rs("Monday next week", WED), one(2026, 9, 28));
    }

    #[test]
    fn this_weekday_follows_the_saturday_week() {
        let sun = d(2026, 9, 27);
        assert_eq!(rs("this Monday", sun), one(2026, 9, 28));
        assert_eq!(
            resolve("this Monday", sun, WeekStart::Monday),
            either((2026, 9, 21), (2026, 9, 28)),
            "a Monday-first week has already had its Monday"
        );
        assert_eq!(rs("this Friday", WED), one(2026, 9, 25));
    }

    #[test]
    fn end_of_a_saturday_week_is_one_day_the_friday() {
        assert_eq!(rs("end of the week", SAT), one(2026, 10, 2));
        assert_eq!(rs("end of next week", SAT), one(2026, 10, 9));
        // Other starts still show both readings.
        assert_eq!(
            resolve("end of the week", WED, WeekStart::Monday),
            either((2026, 9, 25), (2026, 9, 27))
        );
    }

    #[test]
    fn windows_week_start_five_is_saturday() {
        assert_eq!(week_start_from_locale(5), WeekStart::Saturday);
        assert_eq!(week_start_from_locale(6), WeekStart::Sunday);
        assert_eq!(week_start_from_locale(0), WeekStart::Monday);
        assert_eq!(week_start_from_locale(1), WeekStart::Monday);
        assert_eq!(week_start_from_locale(u32::MAX), WeekStart::Monday);
    }

    // -- times of day (#706) --------------------------------------------------------------

    /// 14:05, in minutes since midnight.
    const NOW: Option<u16> = Some(14 * 60 + 5);

    fn at(text: &str) -> Vec<Found> {
        find_at(text, WED, NOW, WeekStart::Monday)
    }

    fn time_of(text: &str) -> Option<(u16, bool)> {
        at(text)
            .first()
            .and_then(|f| f.time)
            .map(|t| (t.minute_of_day, t.assumed))
    }

    #[test]
    fn eod_is_five_in_the_afternoon_and_says_it_was_assumed() {
        let f = at("tomorrow EOD");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].phrase, "tomorrow EOD");
        assert_eq!(f[0].resolution, Resolution::One(d(2026, 9, 24)));
        assert_eq!(
            f[0].time.map(|t| (t.minute_of_day, t.assumed)),
            Some((17 * 60, true))
        );
        assert_eq!(time_of("tomorrow COB"), Some((17 * 60, true)));
        assert_eq!(time_of("tomorrow end of day"), Some((17 * 60, true)));
        assert_eq!(at("tomorrow by EOD")[0].phrase, "tomorrow by EOD");
    }

    #[test]
    fn a_written_clock_time_is_kept_as_written() {
        let f = at("Friday at 5pm");
        assert_eq!(f[0].phrase, "Friday at 5pm");
        assert_eq!(
            f[0].resolution,
            Resolution::Either(d(2026, 9, 18), d(2026, 9, 25))
        );
        assert_eq!(
            f[0].time.map(|t| (t.minute_of_day, t.assumed)),
            Some((17 * 60, false))
        );
        assert_eq!(time_of("tomorrow at 9:30am"), Some((9 * 60 + 30, false)));
        assert_eq!(time_of("tomorrow at 9:30 AM"), Some((9 * 60 + 30, false)));
        assert_eq!(time_of("tomorrow 5 pm"), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow 17:00"), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow at 09:15"), Some((9 * 60 + 15, false)));
        assert_eq!(time_of("tomorrow at noon"), Some((12 * 60, false)));
        assert_eq!(time_of("tomorrow at 12am"), Some((0, false)));
        assert_eq!(time_of("tomorrow at 12pm"), Some((12 * 60, false)));
        assert_eq!(time_of("tomorrow at 12:30pm"), Some((12 * 60 + 30, false)));
        assert_eq!(at("tomorrow at 5:30pm")[0].phrase, "tomorrow at 5:30pm");
    }

    #[test]
    fn a_comma_between_the_date_and_the_time_is_read() {
        let f = at("Tomorrow, 5pm");
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].phrase, "Tomorrow, 5pm");
        assert_eq!(f[0].resolution, Resolution::One(d(2026, 9, 24)));
        assert_eq!(time_of("tomorrow, 5pm"), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow,5pm"), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow, at 9:30 am"), Some((9 * 60 + 30, false)));
        assert_eq!(time_of("tomorrow, noon"), Some((12 * 60, false)));
        assert_eq!(time_of("tomorrow, EOD"), Some((17 * 60, true)));
        // Only one comma, and a time must follow it.
        for text in [
            "tomorrow,, 5pm",
            "tomorrow, , 5pm",
            "tomorrow,",
            "tomorrow, 5",
        ] {
            let f = at(text);
            assert_eq!(f.len(), 1, "{text}");
            assert_eq!(f[0].time, None, "{text}");
            assert_eq!(f[0].phrase, "tomorrow", "{text}");
        }
    }

    #[test]
    fn dotted_am_and_pm_are_read() {
        let f = at("tomorrow at 5 p.m.");
        assert_eq!(f[0].phrase, "tomorrow at 5 p.m.");
        assert_eq!(f[0].time.map(|t| t.minute_of_day), Some(17 * 60));
        assert_eq!(time_of("Tomorrow, 5 P.M."), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow 5p.m."), Some((17 * 60, false)));
        assert_eq!(time_of("tomorrow 5:30 p.m."), Some((17 * 60 + 30, false)));
        assert_eq!(time_of("tomorrow 5:30p.m."), Some((17 * 60 + 30, false)));
        assert_eq!(time_of("tomorrow 9 a.m."), Some((9 * 60, false)));
        assert_eq!(time_of("tomorrow 9 a.m"), Some((9 * 60, false)));
        assert_eq!(time_of("tomorrow 12 a.m."), Some((0, false)));
        assert_eq!(time_of("tomorrow 12 p.m."), Some((12 * 60, false)));
        assert_eq!(time_of("tomorrow, 12 a.m."), Some((0, false)));
        assert_eq!(time_of("tomorrow, 12 p.m."), Some((12 * 60, false)));
        // Malformed dots and hours are not guessed.
        for text in [
            "tomorrow 5 p. m.",
            "tomorrow 5 p..m.",
            "tomorrow 5 p",
            "tomorrow 5 x.m.",
            "tomorrow 13 p.m.",
            "tomorrow 0 a.m.",
            "tomorrow 5 pa.m.",
        ] {
            let f = at(text);
            assert_eq!(f.len(), 1, "{text}");
            assert_eq!(f[0].time, None, "{text}");
        }
    }

    #[test]
    fn midnight_is_not_guessed() {
        // "Friday midnight" usually means the end of Friday, not 00:00 at its
        // start, so no time is shown and the date stands.
        for text in [
            "Friday midnight",
            "tomorrow at midnight",
            "tomorrow, midnight",
        ] {
            let f = at(text);
            assert_eq!(f.len(), 1, "{text}");
            assert_eq!(f[0].time, None, "{text}");
        }
    }

    #[test]
    fn a_time_that_could_be_read_two_ways_is_not_guessed() {
        // No am or pm, and not written as 24-hour: no time, the date stays.
        for text in [
            "tomorrow at 5",
            "tomorrow at 5:30",
            "tomorrow at 13pm",
            "tomorrow at 0pm",
            "tomorrow at 9:75am",
            "tomorrow at 25:00",
            "tomorrow at 9:5pm",
            "tomorrow at 5 people",
            "tomorrow at",
            "tomorrow 123pm",
        ] {
            let f = at(text);
            assert_eq!(f.len(), 1, "{text}");
            assert_eq!(f[0].phrase, "tomorrow", "{text}");
            assert_eq!(f[0].time, None, "{text}");
        }
        assert_eq!(time_of("tomorrow"), None, "no time written, none shown");
    }

    #[test]
    fn hostile_digits_never_panic() {
        let long = format!("tomorrow at {}pm", "9".repeat(5000));
        assert_eq!(at(&long)[0].time, None);
        assert_eq!(at("tomorrow at \u{ff15}pm")[0].time, None);
        assert_eq!(at("tomorrow at 5:\u{ff10}\u{ff10}pm")[0].time, None);
        assert_eq!(at("tomorrow at 5:30\u{301}pm")[0].time, None);
        assert_eq!(at("tomorrow at :")[0].time, None);
    }

    #[test]
    fn hours_and_minutes_ahead_need_the_clock() {
        let f = at("in 2 hours");
        assert_eq!(f[0].phrase, "in 2 hours");
        assert_eq!(f[0].resolution, Resolution::One(d(2026, 9, 23)));
        assert_eq!(
            f[0].time.map(|t| (t.minute_of_day, t.assumed)),
            Some((16 * 60 + 5, false))
        );
        assert_eq!(time_of("in 90 minutes"), Some((15 * 60 + 35, false)));
        assert_eq!(time_of("in an hour"), Some((15 * 60 + 5, false)));
        assert_eq!(time_of("2 hours from now"), Some((16 * 60 + 5, false)));
        assert_eq!(time_of("3 hours ago"), Some((11 * 60 + 5, false)));
        // Without the clock there is nothing honest to show.
        assert!(find_at("in 2 hours", WED, None, WeekStart::Monday).is_empty());
    }

    #[test]
    fn hours_ahead_cross_midnight_and_a_year_end() {
        let f = at("in 10 hours");
        assert_eq!(f[0].resolution, Resolution::One(d(2026, 9, 24)));
        assert_eq!(f[0].time.map(|t| t.minute_of_day), Some(5));
        let late = find_at(
            "in 3 hours",
            d(2026, 12, 31),
            Some(23 * 60),
            WeekStart::Monday,
        );
        assert_eq!(late[0].resolution, Resolution::One(d(2027, 1, 1)));
        assert_eq!(late[0].time.map(|t| t.minute_of_day), Some(2 * 60));
        let early = find_at("2 hours ago", d(2027, 1, 1), Some(30), WeekStart::Monday);
        assert_eq!(early[0].resolution, Resolution::One(d(2026, 12, 31)));
        assert_eq!(early[0].time.map(|t| t.minute_of_day), Some(22 * 60 + 30));
    }

    #[test]
    fn a_clock_beyond_the_day_is_not_trusted() {
        assert!(find_at("in 2 hours", WED, Some(24 * 60), WeekStart::Monday).is_empty());
        assert!(find_at("in 2 hours", WED, Some(u16::MAX), WeekStart::Monday).is_empty());
    }

    #[test]
    fn the_time_label_follows_the_date() {
        let f = &at("Friday at 5pm")[0];
        assert_eq!(describe_found(f, WED), "Fri Sep 18 or Fri Sep 25, 17:00");
        let g = &at("tomorrow at 9:05am")[0];
        assert_eq!(describe_found(g, WED), "Thu Sep 24, 09:05");
        let h = &at("tomorrow")[0];
        assert_eq!(describe_found(h, WED), "Thu Sep 24");
    }

    #[test]
    fn find_without_a_clock_is_the_old_behaviour() {
        assert_eq!(phrases("tomorrow at 5pm", WED), ["tomorrow at 5pm"]);
        assert!(find("in 2 hours", WED, WeekStart::Monday).is_empty());
    }
}
