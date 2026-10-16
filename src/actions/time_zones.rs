//! Time zone names and conversion for "Convert times to my time zone" (#315).
//!
//! The model only reads times off the screen as written. Everything here is
//! deterministic Rust: turning the zone text ("PT", "CET", "UTC+5:30") into a
//! zone, turning a time and a date into an instant, and turning that instant
//! into the user's local time. The model never does the arithmetic.
//!
//! No time zone database is added. Daylight saving rules come from Windows
//! itself, behind the [`TzProvider`] trait: [`WindowsZones`] asks the OS, and
//! the tests use a fake with hand-written transitions. The only question the
//! provider answers is "what is the local time in this zone at this UTC
//! instant", and [`local_to_utc`] builds the reverse on top of it, so the two
//! awkward days of the year (a skipped hour and a repeated hour) are handled
//! in one place, in pure code.
//!
//! Decisions, so nothing is guessed silently:
//! - A region abbreviation (PT, PST, PDT, ET, CET, CEST ...) means the region's
//!   clock, so the date decides whether daylight saving applies. "9:00 PST" in
//!   July is read as Pacific time that day (UTC-7). The card shows the offset
//!   that was used, so a mismatch with what the page meant is visible.
//! - An abbreviation with more than one common meaning (CST, IST, BST, AST) is
//!   never resolved: [`parse_zone`] returns every reading and the card shows
//!   each one.
//! - A zone the table does not know is reported as unknown, not converted.
//! - A date with no year is the next such date on or after today in the
//!   source zone. A weekday alone is the next such weekday. No date at all
//!   means today in the source zone.

use crate::connectors::civil_time::{
    civil_datetime_to_unix, days_from_civil, unix_to_civil_datetime, CivilDate, CivilDateTime,
};

/// A zone the converter can ask a [`TzProvider`] about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZoneId {
    /// A fixed offset from UTC, in minutes east (UTC+5:30 is 330).
    Fixed(i32),
    /// A Windows time zone key name ("Pacific Standard Time"); its daylight
    /// saving rules come from the provider.
    Windows(&'static str),
    /// The user's own zone.
    Local,
}

/// One possible meaning of a zone as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reading {
    /// Short name for the card ("India", "UTC+5:30").
    pub name: String,
    pub zone: ZoneId,
}

/// What a written zone turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ZoneParse {
    /// Nothing written.
    Missing,
    /// Not in the table and not an offset.
    Unknown,
    /// One reading, or several when the abbreviation is ambiguous.
    Readings(Vec<Reading>),
}

fn r(name: &str, zone: ZoneId) -> Reading {
    Reading {
        name: name.to_string(),
        zone,
    }
}

use ZoneId::{Fixed, Windows};

/// Abbreviation to readings. Upper case keys. One reading means the
/// abbreviation is unambiguous; more than one means the card shows each.
fn abbreviation(key: &str) -> Option<Vec<Reading>> {
    let pacific = || r("Pacific", Windows("Pacific Standard Time"));
    let mountain = || r("Mountain", Windows("Mountain Standard Time"));
    let central = || r("US Central", Windows("Central Standard Time"));
    let eastern = || r("Eastern", Windows("Eastern Standard Time"));
    let one = |x: Reading| Some(vec![x]);
    match key {
        "UTC" | "GMT" | "Z" | "UT" => one(r("UTC", Fixed(0))),
        "AOE" => one(r("Anywhere on Earth (UTC-12)", Fixed(-12 * 60))),
        "PT" | "PST" | "PDT" => one(pacific()),
        "MT" | "MST" | "MDT" => one(mountain()),
        "CT" | "CDT" => one(central()),
        "ET" | "EST" | "EDT" => one(eastern()),
        "AKST" | "AKDT" => one(r("Alaska", Windows("Alaskan Standard Time"))),
        "HST" | "HAST" => one(r("Hawaii (UTC-10)", Fixed(-10 * 60))),
        "NST" | "NDT" => one(r("Newfoundland", Windows("Newfoundland Standard Time"))),
        "CET" | "CEST" | "MET" | "MEST" => {
            one(r("Central Europe", Windows("W. Europe Standard Time")))
        }
        "EET" | "EEST" => one(r("Eastern Europe", Windows("FLE Standard Time"))),
        "WET" | "WEST" => one(r("Western Europe", Windows("GMT Standard Time"))),
        "MSK" => one(r("Moscow (UTC+3)", Fixed(3 * 60))),
        "SAST" => one(r("South Africa (UTC+2)", Fixed(2 * 60))),
        "CAT" => one(r("Central Africa (UTC+2)", Fixed(2 * 60))),
        "EAT" => one(r("East Africa (UTC+3)", Fixed(3 * 60))),
        "WAT" => one(r("West Africa (UTC+1)", Fixed(60))),
        "PKT" => one(r("Pakistan (UTC+5)", Fixed(5 * 60))),
        "SGT" => one(r("Singapore (UTC+8)", Fixed(8 * 60))),
        "HKT" => one(r("Hong Kong (UTC+8)", Fixed(8 * 60))),
        "AWST" => one(r("Western Australia (UTC+8)", Fixed(8 * 60))),
        "JST" => one(r("Japan (UTC+9)", Fixed(9 * 60))),
        "KST" => one(r("Korea (UTC+9)", Fixed(9 * 60))),
        "ACST" | "ACDT" => one(r("Adelaide", Windows("Cen. Australia Standard Time"))),
        "AEST" | "AEDT" => one(r("Sydney", Windows("AUS Eastern Standard Time"))),
        "NZST" | "NZDT" => one(r("New Zealand", Windows("New Zealand Standard Time"))),
        "BRT" => one(r("Brazil (UTC-3)", Fixed(-3 * 60))),
        // Ambiguous: never picked for the user.
        "CST" => Some(vec![
            central(),
            r("China (UTC+8)", Fixed(8 * 60)),
            r("Cuba", Windows("Cuba Standard Time")),
        ]),
        "IST" => Some(vec![
            r("India (UTC+5:30)", Fixed(5 * 60 + 30)),
            r("Ireland", Windows("GMT Standard Time")),
            r("Israel", Windows("Israel Standard Time")),
        ]),
        "BST" => Some(vec![
            r("British Summer Time", Windows("GMT Standard Time")),
            r("Bangladesh (UTC+6)", Fixed(6 * 60)),
        ]),
        "AST" => Some(vec![
            r("Atlantic", Windows("Atlantic Standard Time")),
            r("Arabia (UTC+3)", Fixed(3 * 60)),
        ]),
        "GST" => Some(vec![
            r("Gulf (UTC+4)", Fixed(4 * 60)),
            r("South Georgia (UTC-2)", Fixed(-2 * 60)),
        ]),
        _ => None,
    }
}

/// "+5:30", "-08", "+0530", "+5": sign, then hours and optional minutes.
/// `None` for anything else or an offset outside UTC-12:00 to UTC+14:00.
fn parse_offset(s: &str) -> Option<i32> {
    let mut chars = s.chars();
    let sign = match chars.next()? {
        '+' => 1,
        '-' | '\u{2212}' => -1,
        _ => return None,
    };
    let rest = chars.as_str();
    // ASCII digits and colons only, so the byte slices below are on
    // character boundaries, and short enough that nothing can overflow.
    if rest.is_empty() || rest.len() > 5 || !rest.bytes().all(|b| b.is_ascii_digit() || b == b':') {
        return None;
    }
    let (h, m): (i32, i32) = if let Some((h, m)) = rest.split_once(':') {
        if h.is_empty() || h.len() > 2 || m.len() != 2 {
            return None;
        }
        (h.parse().ok()?, m.parse().ok()?)
    } else if rest.len() == 4 {
        (rest[..2].parse().ok()?, rest[2..].parse().ok()?)
    } else if rest.len() <= 2 {
        (rest.parse().ok()?, 0)
    } else {
        return None;
    };
    if !(0..60).contains(&m) {
        return None;
    }
    let total = sign * (h * 60 + m);
    (-12 * 60..=14 * 60).contains(&total).then_some(total)
}

fn offset_name(minutes: i32) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let (h, m) = (minutes.abs() / 60, minutes.abs() % 60);
    if m == 0 {
        format!("UTC{sign}{h}")
    } else {
        format!("UTC{sign}{h}:{m:02}")
    }
}

/// Reads the zone text the model copied off the screen.
pub fn parse_zone(text: &str) -> ZoneParse {
    let t = text
        .trim()
        .trim_matches(|c| c == '(' || c == ')' || c == '.');
    if t.is_empty() {
        return ZoneParse::Missing;
    }
    let upper = t.to_uppercase().replace([' ', '.'], "");
    // "UTC+5:30", "GMT-8", or a bare "+05:30".
    let offset_part = upper
        .strip_prefix("UTC")
        .or_else(|| upper.strip_prefix("GMT"))
        .unwrap_or(&upper);
    if let Some(minutes) = parse_offset(offset_part) {
        return ZoneParse::Readings(vec![r(&offset_name(minutes), Fixed(minutes))]);
    }
    match abbreviation(&upper) {
        Some(readings) => ZoneParse::Readings(readings),
        None => ZoneParse::Unknown,
    }
}

/// Where the local time of a zone comes from.
pub trait TzProvider {
    /// The local civil time in `zone` at the UTC instant `utc_secs` (Unix
    /// seconds), daylight saving included. `None` when the zone is unknown to
    /// the provider.
    fn utc_to_local(&self, zone: &ZoneId, utc_secs: i64) -> Option<CivilDateTime>;
}

/// [`TzProvider::utc_to_local`] with the fixed offsets handled here.
fn to_local(p: &dyn TzProvider, zone: &ZoneId, utc: i64) -> Option<CivilDateTime> {
    match zone {
        ZoneId::Fixed(min) => Some(unix_to_civil_datetime(utc + *min as i64 * 60)),
        _ => p.utc_to_local(zone, utc),
    }
}

/// How a civil time sits against the zone's clock changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved {
    /// Exactly one instant.
    Instant(i64),
    /// The clocks repeat this hour; the first instant is returned.
    Repeated(i64),
    /// The clocks skip this hour: the time never happens on that date.
    Skipped,
}

/// The UTC instant of a civil time in `zone`. A transition is found by
/// asking the provider for the zone's offset a day either side and keeping
/// the offsets that map back to the same civil time: none means the time was
/// skipped, two means it happens twice.
pub fn local_to_utc(p: &dyn TzProvider, zone: &ZoneId, local: &CivilDateTime) -> Option<Resolved> {
    let ls = civil_datetime_to_unix(local);
    if let ZoneId::Fixed(min) = zone {
        return Some(Resolved::Instant(ls - *min as i64 * 60));
    }
    let mut offsets: Vec<i64> = Vec::new();
    for probe in [ls - 86_400, ls + 86_400] {
        let off = civil_datetime_to_unix(&to_local(p, zone, probe)?) - probe;
        if !offsets.contains(&off) {
            offsets.push(off);
        }
    }
    let mut valid: Vec<i64> = Vec::new();
    for off in offsets {
        let utc = ls - off;
        if civil_datetime_to_unix(&to_local(p, zone, utc)?) == ls {
            valid.push(utc);
        }
    }
    valid.sort_unstable();
    Some(match valid.as_slice() {
        [] => Resolved::Skipped,
        [one] => Resolved::Instant(*one),
        [first, ..] => Resolved::Repeated(*first),
    })
}

/// The local time in `zone` at `utc`, with its offset from UTC in minutes.
pub fn local_with_offset(
    p: &dyn TzProvider,
    zone: &ZoneId,
    utc: i64,
) -> Option<(CivilDateTime, i32)> {
    let local = to_local(p, zone, utc)?;
    Some((local, ((civil_datetime_to_unix(&local) - utc) / 60) as i32))
}

/// The offset name for the card ("UTC-7", "UTC+5:30").
pub fn format_offset(minutes: i32) -> String {
    offset_name(minutes)
}

// -- Time of day and date, as written -----------------------------------

/// A time of day as written, and whether it was written with am/pm.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeOfDay {
    pub hour: u8,
    pub minute: u8,
    pub twelve_hour: bool,
}

/// "9:00", "9:00 PM", "9pm", "21:00", "noon", "midnight". `None` otherwise.
pub fn parse_time(text: &str) -> Option<TimeOfDay> {
    let t = text.trim().to_lowercase().replace('.', "");
    match t.as_str() {
        "noon" => {
            return Some(TimeOfDay {
                hour: 12,
                minute: 0,
                twelve_hour: true,
            })
        }
        "midnight" => {
            return Some(TimeOfDay {
                hour: 0,
                minute: 0,
                twelve_hour: true,
            })
        }
        _ => {}
    }
    let (body, meridiem) = if let Some(b) = t.strip_suffix("am") {
        (b.trim(), Some(false))
    } else if let Some(b) = t.strip_suffix("pm") {
        (b.trim(), Some(true))
    } else {
        (t.as_str(), None)
    };
    let (h, m) = match body.split_once(':') {
        Some((h, m)) => (h, m.split(':').next()?),
        None if meridiem.is_some() => (body, "0"),
        None => return None,
    };
    if h.is_empty() || m.is_empty() || m.len() > 2 {
        return None;
    }
    let (mut hour, minute): (u8, u8) = (h.parse().ok()?, m.parse().ok()?);
    if minute > 59 {
        return None;
    }
    match meridiem {
        Some(pm) => {
            if !(1..=12).contains(&hour) {
                return None;
            }
            hour %= 12;
            if pm {
                hour += 12;
            }
        }
        None if hour > 23 => return None,
        None => {}
    }
    Some(TimeOfDay {
        hour,
        minute,
        twelve_hour: meridiem.is_some(),
    })
}

/// A date as written. Anything missing is filled in by [`resolve_date`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DateSpec {
    pub year: Option<i32>,
    pub month: Option<u8>,
    pub day: Option<u8>,
    /// 0 is Monday.
    pub weekday: Option<u8>,
    /// -1 yesterday, 0 today, 1 tomorrow, from a word.
    pub relative_days: Option<i8>,
}

const MONTHS: [&str; 12] = [
    "january",
    "february",
    "march",
    "april",
    "may",
    "june",
    "july",
    "august",
    "september",
    "october",
    "november",
    "december",
];
const WEEKDAYS: [&str; 7] = [
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
];

fn name_index(table: &[&str], token: &str) -> Option<u8> {
    if token.len() < 3 {
        return None;
    }
    table
        .iter()
        .position(|full| full.starts_with(token) || (token == "sept" && *full == "september"))
        .map(|i| i as u8)
}

/// Reads a date: "2026-10-22", "Oct 22", "22 October 2026", "Thursday",
/// "Thursday, October 22", "tomorrow", "10/22/2026". `Ok(None)` when no date
/// was written. `Err(())` when something was written that cannot be read
/// with confidence (for example "3/4", which is March 4 or April 3).
#[allow(clippy::result_unit_err)]
pub fn parse_date(text: &str) -> Result<Option<DateSpec>, ()> {
    let t = text.trim().to_lowercase();
    if t.is_empty() {
        return Ok(None);
    }
    let mut spec = DateSpec::default();
    for token in t.split(|c: char| c.is_whitespace() || c == ',') {
        let token = token.trim_matches('.');
        if token.is_empty() || matches!(token, "on" | "the" | "of") {
            continue;
        }
        let relative = match token {
            "today" => Some(0),
            "tomorrow" => Some(1),
            "yesterday" => Some(-1),
            _ => None,
        };
        if relative.is_some() {
            spec.relative_days = relative;
            continue;
        }
        if let Some((y, rest)) = token.split_once('-').filter(|(y, _)| y.len() == 4) {
            let (m, d) = rest.split_once('-').ok_or(())?;
            spec.year = Some(y.parse().map_err(|_| ())?);
            spec.month = Some(m.parse().map_err(|_| ())?);
            spec.day = Some(d.parse().map_err(|_| ())?);
            continue;
        }
        if token.contains('/') {
            let parts: Vec<&str> = token.split('/').collect();
            if !(2..=3).contains(&parts.len()) {
                return Err(());
            }
            let a: u8 = parts[0].parse().map_err(|_| ())?;
            let b: u8 = parts[1].parse().map_err(|_| ())?;
            let (month, day) = if a > 12 && b <= 12 {
                (b, a)
            } else if (b > 12 && a <= 12) || a == b {
                (a, b)
            } else {
                return Err(());
            };
            spec.month = Some(month);
            spec.day = Some(day);
            if let Some(y) = parts.get(2) {
                let y: i32 = y.parse().map_err(|_| ())?;
                spec.year = Some(if y < 100 { 2000 + y } else { y });
            }
            continue;
        }
        let digits = token.trim_end_matches(|c: char| c.is_ascii_alphabetic());
        let suffix = &token[digits.len()..];
        if !digits.is_empty()
            && digits.chars().all(|c| c.is_ascii_digit())
            && matches!(suffix, "" | "st" | "nd" | "rd" | "th")
        {
            let n: i32 = digits.parse().map_err(|_| ())?;
            if digits.len() == 4 {
                spec.year = Some(n);
            } else if (1..=31).contains(&n) {
                spec.day = Some(n as u8);
            } else {
                return Err(());
            }
            continue;
        }
        if let Some(m) = name_index(&MONTHS, token) {
            spec.month = Some(m + 1);
        } else if let Some(w) = name_index(&WEEKDAYS, token.trim_end_matches('s')) {
            spec.weekday = Some(w);
        } else {
            return Err(());
        }
    }
    if spec == DateSpec::default() {
        return Ok(None);
    }
    // The range Windows time structures can hold.
    if spec.year.is_some_and(|y| !(1601..=30827).contains(&y)) {
        return Err(());
    }
    // A month needs a day and a day needs a month.
    if spec.month.is_some() != spec.day.is_some() {
        return Err(());
    }
    Ok(Some(spec))
}

fn weekday_of(days: i64) -> u8 {
    // 1970-01-01 was a Thursday (3 with Monday as 0).
    (days + 3).rem_euclid(7) as u8
}

/// Weekday of a civil date, 0 is Monday.
pub fn weekday(date: &CivilDate) -> u8 {
    weekday_of(days_from_civil(
        date.year as i64,
        date.month as u32,
        date.day as u32,
    ))
}

fn valid_date(y: i32, m: u8, d: u8) -> bool {
    if !(1..=12).contains(&m) || d == 0 {
        return false;
    }
    let (ny, nm) = if m == 12 {
        (y + 1, 1)
    } else {
        (y, m as u32 + 1)
    };
    let len = days_from_civil(ny as i64, nm, 1) - days_from_civil(y as i64, m as u32, 1);
    (d as i64) <= len
}

/// The calendar date a [`DateSpec`] means, given `today` in the source zone.
/// `None` when no such date exists (30 February) or it is not found within
/// eight years (a Friday the 13th with a wrong weekday, say).
pub fn resolve_date(spec: Option<&DateSpec>, today: CivilDate) -> Option<CivilDate> {
    let today_days = days_from_civil(today.year as i64, today.month as u32, today.day as u32);
    let from_days = |days: i64| {
        let (y, m, d) = crate::connectors::civil_time::civil_from_days(days);
        CivilDate {
            year: y as i32,
            month: m as u8,
            day: d as u8,
        }
    };
    let Some(spec) = spec else {
        return Some(today);
    };
    if let Some(rel) = spec.relative_days {
        return Some(from_days(today_days + rel as i64));
    }
    match (spec.month, spec.day) {
        (Some(m), Some(d)) => {
            if let Some(y) = spec.year {
                return valid_date(y, m, d).then_some(CivilDate {
                    year: y,
                    month: m,
                    day: d,
                });
            }
            (today.year..=today.year + 8)
                .filter(|y| valid_date(*y, m, d))
                .map(|y| CivilDate {
                    year: y,
                    month: m,
                    day: d,
                })
                .find(|date| {
                    let days = days_from_civil(date.year as i64, m as u32, d as u32);
                    days >= today_days && spec.weekday.map_or(true, |w| weekday_of(days) == w)
                })
        }
        _ => {
            let w = spec.weekday?;
            let ahead = (w as i64 - weekday_of(today_days) as i64).rem_euclid(7);
            Some(from_days(today_days + ahead))
        }
    }
}

// -- The Windows provider --------------------------------------------------

/// Daylight saving rules from Windows' own time zone data.
pub struct WindowsZones {
    /// A Windows key name standing in for the user's zone, so tests are
    /// independent of the machine. `None` means the real local zone.
    local_override: Option<&'static str>,
    cache: std::sync::Mutex<
        std::collections::HashMap<
            &'static str,
            Option<windows::Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION>,
        >,
    >,
}

impl WindowsZones {
    pub fn new() -> Self {
        Self {
            local_override: None,
            cache: Default::default(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_local(key: &'static str) -> Self {
        Self {
            local_override: Some(key),
            cache: Default::default(),
        }
    }

    fn lookup(
        &self,
        key: &'static str,
    ) -> Option<windows::Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION> {
        use windows::Win32::System::Time::{
            EnumDynamicTimeZoneInformation, DYNAMIC_TIME_ZONE_INFORMATION,
        };
        let mut cache = self.cache.lock().ok()?;
        if let Some(found) = cache.get(key) {
            return *found;
        }
        let mut found = None;
        for index in 0.. {
            let mut info = DYNAMIC_TIME_ZONE_INFORMATION::default();
            // SAFETY: `info` is a valid, writable struct for the call.
            if unsafe { EnumDynamicTimeZoneInformation(index, &mut info) } != 0 {
                break;
            }
            let len = info
                .TimeZoneKeyName
                .iter()
                .position(|c| *c == 0)
                .unwrap_or(info.TimeZoneKeyName.len());
            if String::from_utf16_lossy(&info.TimeZoneKeyName[..len]) == key {
                found = Some(info);
                break;
            }
        }
        cache.insert(key, found);
        found
    }
}

impl Default for WindowsZones {
    fn default() -> Self {
        Self::new()
    }
}

impl TzProvider for WindowsZones {
    fn utc_to_local(&self, zone: &ZoneId, utc_secs: i64) -> Option<CivilDateTime> {
        use windows::Win32::Foundation::SYSTEMTIME;
        use windows::Win32::System::Time::{
            GetTimeZoneInformationForYear, SystemTimeToTzSpecificLocalTime, TIME_ZONE_INFORMATION,
        };
        let key = match zone {
            ZoneId::Windows(key) => Some(*key),
            ZoneId::Local => self.local_override,
            ZoneId::Fixed(_) => return None,
        };
        let info = match key {
            Some(key) => Some(self.lookup(key)?),
            None => None,
        };
        let c = unix_to_civil_datetime(utc_secs);
        if !(1601..=30827).contains(&c.date.year) {
            return None;
        }
        // The rules in force that year (they change: Israel, Brazil...), as a
        // plain TIME_ZONE_INFORMATION, then the plain conversion.
        let mut tzi = TIME_ZONE_INFORMATION::default();
        // SAFETY: `tzi` is a valid, writable struct; `info` is a valid zone or
        // `None` for the user's current zone.
        if unsafe {
            GetTimeZoneInformationForYear(
                c.date.year as u16,
                info.as_ref().map(|i| i as *const _),
                &mut tzi,
            )
        }
        .is_err()
        {
            return None;
        }
        let utc = SYSTEMTIME {
            wYear: c.date.year as u16,
            wMonth: c.date.month as u16,
            wDay: c.date.day as u16,
            wHour: c.hour as u16,
            wMinute: c.minute as u16,
            wSecond: c.second as u16,
            ..Default::default()
        };
        let mut local = SYSTEMTIME::default();
        // SAFETY: both SYSTEMTIMEs and `tzi` are valid for the call.
        unsafe { SystemTimeToTzSpecificLocalTime(Some(&tzi), &utc, &mut local) }.ok()?;
        Some(CivilDateTime {
            date: CivilDate {
                year: local.wYear as i32,
                month: local.wMonth as u8,
                day: local.wDay as u8,
            },
            hour: local.wHour as u8,
            minute: local.wMinute as u8,
            second: local.wSecond as u8,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-written rules for 2026 only: Pacific (UTC-8, UTC-7 from
    /// 2026-03-08 10:00 UTC to 2026-11-01 09:00 UTC), Central Europe (UTC+1,
    /// UTC+2 from 2026-03-29 01:00 UTC to 2026-10-25 01:00 UTC), and the
    /// local zone, which is Eastern (UTC-5, UTC-4 from 2026-03-08 07:00 UTC to
    /// 2026-11-01 06:00 UTC).
    struct Fake;

    fn utc(y: i32, mo: u8, d: u8, h: u8) -> i64 {
        civil_datetime_to_unix(&CivilDateTime {
            date: CivilDate {
                year: y,
                month: mo,
                day: d,
            },
            hour: h,
            minute: 0,
            second: 0,
        })
    }

    impl TzProvider for Fake {
        fn utc_to_local(&self, zone: &ZoneId, t: i64) -> Option<CivilDateTime> {
            let (std, start, end) = match zone {
                ZoneId::Windows("Pacific Standard Time") => {
                    (-480, utc(2026, 3, 8, 10), utc(2026, 11, 1, 9))
                }
                ZoneId::Windows("W. Europe Standard Time") => {
                    (60, utc(2026, 3, 29, 1), utc(2026, 10, 25, 1))
                }
                ZoneId::Local | ZoneId::Windows("Eastern Standard Time") => {
                    (-300, utc(2026, 3, 8, 7), utc(2026, 11, 1, 6))
                }
                _ => return None,
            };
            let dst = t >= start && t < end;
            Some(unix_to_civil_datetime(
                t + (std + if dst { 60 } else { 0 }) * 60,
            ))
        }
    }

    fn civil(y: i32, mo: u8, d: u8, h: u8, mi: u8) -> CivilDateTime {
        CivilDateTime {
            date: CivilDate {
                year: y,
                month: mo,
                day: d,
            },
            hour: h,
            minute: mi,
            second: 0,
        }
    }

    fn only(text: &str) -> ZoneId {
        match parse_zone(text) {
            ZoneParse::Readings(v) if v.len() == 1 => v[0].zone,
            other => panic!("{text}: {other:?}"),
        }
    }

    /// Local time (Eastern, in the fake) for a civil time written in `zone`.
    fn convert(zone: &str, local: CivilDateTime) -> CivilDateTime {
        let z = only(zone);
        let Some(Resolved::Instant(t)) = local_to_utc(&Fake, &z, &local) else {
            panic!("{zone}: not a plain instant");
        };
        local_with_offset(&Fake, &ZoneId::Local, t).unwrap().0
    }

    #[test]
    fn the_named_zones_convert_to_local_time() {
        // 2026-10-22 is a Thursday; Pacific and Europe are on daylight time,
        // Eastern too (UTC-4).
        assert_eq!(
            convert("PT", civil(2026, 10, 22, 9, 0)),
            civil(2026, 10, 22, 12, 0)
        );
        assert_eq!(
            convert("CET", civil(2026, 10, 22, 14, 0)),
            civil(2026, 10, 22, 8, 0)
        );
        assert_eq!(
            convert("UTC", civil(2026, 10, 22, 14, 0)),
            civil(2026, 10, 22, 10, 0)
        );
        assert_eq!(
            convert("ET", civil(2026, 10, 22, 9, 0)),
            civil(2026, 10, 22, 9, 0)
        );
        // Anywhere on Earth: 23:59 on the 22nd is 11:59 UTC on the 23rd.
        assert_eq!(
            convert("AoE", civil(2026, 10, 22, 23, 59)),
            civil(2026, 10, 23, 7, 59)
        );
    }

    #[test]
    fn the_day_changes_when_the_conversion_crosses_midnight() {
        assert_eq!(
            convert("PT", civil(2026, 10, 22, 22, 0)),
            civil(2026, 10, 23, 1, 0)
        );
        assert_eq!(
            convert("CET", civil(2026, 10, 22, 2, 0)),
            civil(2026, 10, 21, 20, 0)
        );
    }

    #[test]
    fn a_region_abbreviation_follows_daylight_saving_for_the_date() {
        // The same "9:00 PT" is UTC-8 in January and UTC-7 in July.
        let jan = convert("PT", civil(2026, 1, 15, 9, 0));
        let jul = convert("PT", civil(2026, 7, 15, 9, 0));
        assert_eq!(jan, civil(2026, 1, 15, 12, 0));
        assert_eq!(jul, civil(2026, 7, 15, 12, 0));
        // "PST" in July is still the Pacific clock (decision in the module doc).
        assert_eq!(convert("PST", civil(2026, 7, 15, 9, 0)), jul);
    }

    #[test]
    fn the_spring_forward_gap_has_no_instant() {
        // 2026-03-08 02:30 does not exist in Pacific time.
        let z = only("PT");
        assert_eq!(
            local_to_utc(&Fake, &z, &civil(2026, 3, 8, 2, 30)),
            Some(Resolved::Skipped)
        );
        // 03:30 does, and it is already UTC-7.
        assert_eq!(
            local_to_utc(&Fake, &z, &civil(2026, 3, 8, 3, 30)),
            Some(Resolved::Instant(utc(2026, 3, 8, 10) + 1800))
        );
    }

    #[test]
    fn the_fall_back_hour_happens_twice_and_the_first_is_used() {
        // 2026-11-01 01:30 Pacific is 08:30 UTC (daylight) and 09:30 UTC.
        let z = only("PT");
        assert_eq!(
            local_to_utc(&Fake, &z, &civil(2026, 11, 1, 1, 30)),
            Some(Resolved::Repeated(utc(2026, 11, 1, 8) + 1800))
        );
        // Either side is unambiguous.
        assert!(matches!(
            local_to_utc(&Fake, &z, &civil(2026, 11, 1, 0, 30)),
            Some(Resolved::Instant(_))
        ));
        assert!(matches!(
            local_to_utc(&Fake, &z, &civil(2026, 11, 1, 2, 30)),
            Some(Resolved::Instant(_))
        ));
    }

    #[test]
    fn the_us_and_europe_change_on_different_weekends() {
        // Between 8 and 29 March the gap between Pacific and Central Europe
        // is 8 hours, not the usual 9, and between 25 October and 1 November
        // it is 7 hours: the two zones' clocks change on different weekends.
        let gap = |m, d| {
            let t = utc(2026, m, d, 12);
            let pac = local_with_offset(&Fake, &only("PT"), t).unwrap().1;
            let eu = local_with_offset(&Fake, &only("CET"), t).unwrap().1;
            (eu - pac) / 60
        };
        assert_eq!(gap(3, 1), 9);
        assert_eq!(gap(3, 15), 8);
        assert_eq!(gap(4, 15), 9);
        assert_eq!(gap(10, 28), 8);
        assert_eq!(gap(11, 2), 9);
    }

    #[test]
    fn ambiguous_abbreviations_give_every_reading() {
        for (text, names) in [
            ("CST", vec!["US Central", "China"]),
            ("IST", vec!["India", "Ireland", "Israel"]),
            ("BST", vec!["British", "Bangladesh"]),
            ("AST", vec!["Atlantic", "Arabia"]),
        ] {
            let ZoneParse::Readings(rs) = parse_zone(text) else {
                panic!("{text}");
            };
            assert!(rs.len() >= 2, "{text} must stay ambiguous");
            for n in names {
                assert!(rs.iter().any(|r| r.name.contains(n)), "{text} lacks {n}");
            }
        }
    }

    #[test]
    fn unambiguous_abbreviations_have_one_reading() {
        for z in [
            "PT", "pst", "EDT", "CET", "CEST", "JST", "UTC", "GMT", "Z", "AoE", "A.O.E.",
        ] {
            assert!(
                matches!(parse_zone(z), ZoneParse::Readings(v) if v.len() == 1),
                "{z}"
            );
        }
    }

    #[test]
    fn offsets_are_parsed() {
        for (text, minutes) in [
            ("UTC+5:30", 330),
            ("UTC+05:30", 330),
            ("GMT-8", -480),
            ("UTC -8", -480),
            ("+0530", 330),
            ("UTC+14", 840),
            ("UTC\u{2212}3", -180),
        ] {
            assert_eq!(only(text), ZoneId::Fixed(minutes), "{text}");
        }
        assert_eq!(
            parse_zone("UTC+5:30"),
            ZoneParse::Readings(vec![r("UTC+5:30", Fixed(330))])
        );
    }

    #[test]
    fn bad_offsets_and_names_are_unknown() {
        for z in ["UTC+15", "UTC+5:75", "UTC+", "XYZ", "Thursday", "UTC+a"] {
            assert_eq!(parse_zone(z), ZoneParse::Unknown, "{z}");
        }
        assert_eq!(parse_zone("  "), ZoneParse::Missing);
    }

    #[test]
    fn times_are_parsed_in_both_styles() {
        let t = |s| parse_time(s).map(|t| (t.hour, t.minute, t.twelve_hour));
        assert_eq!(t("9:00"), Some((9, 0, false)));
        assert_eq!(t("21:05"), Some((21, 5, false)));
        assert_eq!(t("9:00 PM"), Some((21, 0, true)));
        assert_eq!(t("9pm"), Some((21, 0, true)));
        assert_eq!(t("12:30 am"), Some((0, 30, true)));
        assert_eq!(t("12 pm"), Some((12, 0, true)));
        assert_eq!(t("p.m."), None);
        assert_eq!(t("noon"), Some((12, 0, true)));
        for bad in ["", "25:00", "9:75", "13pm", "0am", "nine", "9"] {
            assert_eq!(t(bad), None, "{bad}");
        }
    }

    #[test]
    fn dates_are_parsed() {
        let d = |s: &str| parse_date(s).unwrap().unwrap();
        let iso = d("2026-10-22");
        assert_eq!(
            (iso.year, iso.month, iso.day),
            (Some(2026), Some(10), Some(22))
        );
        let md = d("Oct 22");
        assert_eq!((md.year, md.month, md.day), (None, Some(10), Some(22)));
        let dm = d("22nd October 2026");
        assert_eq!(
            (dm.year, dm.month, dm.day),
            (Some(2026), Some(10), Some(22))
        );
        let full = d("Thursday, October 22");
        assert_eq!(
            (full.weekday, full.month, full.day),
            (Some(3), Some(10), Some(22))
        );
        assert_eq!(d("Thu").weekday, Some(3));
        assert_eq!(d("tomorrow").relative_days, Some(1));
        let us = d("10/22/2026");
        assert_eq!(
            (us.month, us.day, us.year),
            (Some(10), Some(22), Some(2026))
        );
        let eu = d("22/10");
        assert_eq!((eu.month, eu.day), (Some(10), Some(22)));
        assert_eq!(parse_date("  ").unwrap(), None);
    }

    #[test]
    fn a_date_that_could_be_two_things_is_refused() {
        // March 4 or April 3: never guessed.
        assert!(parse_date("3/4").is_err());
        assert!(parse_date("someday").is_err());
        assert!(parse_date("Oct").is_err());
    }

    fn today() -> CivilDate {
        // 2026-10-15 is a Thursday.
        CivilDate {
            year: 2026,
            month: 10,
            day: 15,
        }
    }

    fn resolve(s: &str) -> Option<(i32, u8, u8)> {
        let spec = parse_date(s).unwrap();
        resolve_date(spec.as_ref(), today()).map(|d| (d.year, d.month, d.day))
    }

    #[test]
    fn weekday_math_is_right() {
        assert_eq!(weekday(&today()), 3);
        assert_eq!(
            weekday(&CivilDate {
                year: 2000,
                month: 1,
                day: 1
            }),
            5
        );
    }

    #[test]
    fn a_missing_year_means_the_next_such_date() {
        assert_eq!(resolve("Oct 22"), Some((2026, 10, 22)));
        // Already passed this year: next year.
        assert_eq!(resolve("Oct 10"), Some((2027, 10, 10)));
        // Today counts.
        assert_eq!(resolve("Oct 15"), Some((2026, 10, 15)));
        // A weekday alone: the next one, today included.
        assert_eq!(resolve("Thursday"), Some((2026, 10, 15)));
        assert_eq!(resolve("Friday"), Some((2026, 10, 16)));
        assert_eq!(resolve("Wednesday"), Some((2026, 10, 21)));
        // The weekday picks the year when the month and day alone are not enough.
        assert_eq!(resolve("Fri Nov 13"), Some((2026, 11, 13)));
        assert_eq!(resolve("Thu Nov 13"), Some((2031, 11, 13)));
        // No date is today; words are relative to it.
        assert_eq!(resolve(""), Some((2026, 10, 15)));
        assert_eq!(resolve("tomorrow"), Some((2026, 10, 16)));
        // Leap day waits for a leap year; 30 February never comes.
        assert_eq!(resolve("Feb 29"), Some((2028, 2, 29)));
        assert_eq!(resolve("Feb 30"), None);
        assert_eq!(resolve("2026-02-30"), None);
    }

    #[test]
    fn a_dst_boundary_day_without_a_year_converts_with_that_days_rules() {
        // "Nov 1 1:30 PT": today is 2026-10-15, so it is this year's fall-back.
        let date = resolve_date(parse_date("Nov 1").unwrap().as_ref(), today()).unwrap();
        let local = CivilDateTime {
            date,
            hour: 1,
            minute: 30,
            second: 0,
        };
        assert!(matches!(
            local_to_utc(&Fake, &only("PT"), &local),
            Some(Resolved::Repeated(_))
        ));
    }

    #[test]
    fn an_unknown_provider_zone_gives_none() {
        assert_eq!(
            local_to_utc(
                &Fake,
                &ZoneId::Windows("Nowhere Standard Time"),
                &civil(2026, 1, 1, 0, 0)
            ),
            None
        );
    }

    // The next tests ask the real Windows time zone data. The rules for
    // these zones in 2026 are the long-standing US and EU ones, so the
    // answers do not depend on the machine's own zone.

    #[test]
    fn windows_pacific_time_follows_the_us_rules() {
        let w = WindowsZones::with_local("Eastern Standard Time");
        let pac = ZoneId::Windows("Pacific Standard Time");
        // 2026-07-15 09:00 Pacific is 16:00 UTC; January's is 17:00 UTC.
        assert_eq!(
            local_to_utc(&w, &pac, &civil(2026, 7, 15, 9, 0)),
            Some(Resolved::Instant(utc(2026, 7, 15, 16)))
        );
        assert_eq!(
            local_to_utc(&w, &pac, &civil(2026, 1, 15, 9, 0)),
            Some(Resolved::Instant(utc(2026, 1, 15, 17)))
        );
        // The spring gap and the fall repeat, from the OS rules.
        assert_eq!(
            local_to_utc(&w, &pac, &civil(2026, 3, 8, 2, 30)),
            Some(Resolved::Skipped)
        );
        assert_eq!(
            local_to_utc(&w, &pac, &civil(2026, 11, 1, 1, 30)),
            Some(Resolved::Repeated(utc(2026, 11, 1, 8) + 1800))
        );
        // And into the (overridden) local zone.
        assert_eq!(
            local_with_offset(&w, &ZoneId::Local, utc(2026, 7, 15, 16)).unwrap(),
            (civil(2026, 7, 15, 12, 0), -240)
        );
    }

    #[test]
    fn windows_europe_changes_on_the_last_sunday_of_march_and_october() {
        let w = WindowsZones::with_local("Eastern Standard Time");
        let cet = ZoneId::Windows("W. Europe Standard Time");
        assert_eq!(
            local_with_offset(&w, &cet, utc(2026, 3, 29, 0)).unwrap().1,
            60
        );
        assert_eq!(
            local_with_offset(&w, &cet, utc(2026, 3, 29, 2)).unwrap().1,
            120
        );
        assert_eq!(
            local_with_offset(&w, &cet, utc(2026, 10, 25, 0)).unwrap().1,
            120
        );
        assert_eq!(
            local_with_offset(&w, &cet, utc(2026, 10, 25, 2)).unwrap().1,
            60
        );
    }

    #[test]
    fn odd_zone_text_never_panics() {
        let long = "9".repeat(5000);
        let odd = [
            "+1\u{20ac}",
            "-\u{20ac}",
            "\u{2212}",
            "+\u{1F600}\u{1F600}",
            "UTC+\u{20ac}30",
            "+12:\u{20ac}",
            "",
            " ",
            "+",
            "-",
            ":",
            "+:",
            "UTC+71582789",
            "+99999999999999999999",
            "+1:2:3",
            "\u{0}",
            long.as_str(),
        ];
        for z in odd {
            let _ = parse_zone(z);
            let _ = parse_zone(&format!("UTC+{z}"));
            let _ = parse_zone(&format!("{z}{z}"));
            let _ = parse_time(z);
            let _ = parse_date(z);
        }
        for t in ["\u{20ac}:\u{20ac}", "9:\u{20ac}", "\u{20ac}pm", &long] {
            assert_eq!(parse_time(t), None, "{t}");
        }
    }

    #[test]
    fn long_or_impossible_offsets_are_unknown() {
        for z in [
            "+71582789",
            "UTC+71582789",
            "+1260",
            "+1:60",
            "+15:00",
            "-12:01",
            "+123",
            "+1:2",
            "+1:234",
        ] {
            assert_eq!(parse_zone(z), ZoneParse::Unknown, "{z}");
        }
        assert_eq!(only("+1234"), ZoneId::Fixed(12 * 60 + 34));
        assert_eq!(only("-12:00"), ZoneId::Fixed(-720));
    }

    #[test]
    fn years_outside_the_windows_range_are_refused() {
        for d in [
            "0000-01-01",
            "1600-05-05",
            "30828-01-01",
            "99999 Oct 1",
            "1/2/70000",
        ] {
            assert!(parse_date(d).is_err(), "{d}");
        }
        assert!(parse_date("1601-01-01").unwrap().is_some());
        assert!(parse_date("2030-01-01").unwrap().is_some());
    }

    #[test]
    fn windows_applies_the_rules_of_the_year_asked_about() {
        let w = WindowsZones::new();
        let off = |key: &'static str, y, mo, d, h| {
            local_with_offset(&w, &ZoneId::Windows(key), utc(y, mo, d, h)).map(|x| x.1)
        };
        // Israel: daylight time starts on the Friday before the last Sunday
        // of March, so 2027-03-25 is still standard time (UTC+2).
        assert_eq!(off("Israel Standard Time", 2027, 3, 25, 7), Some(120));
        assert_eq!(off("Israel Standard Time", 2027, 3, 27, 12), Some(180));
        assert_eq!(off("Israel Standard Time", 2027, 10, 31, 12), Some(120));
        // Newfoundland (half hour), Sydney (southern summer) and Pacific.
        assert_eq!(
            off("Newfoundland Standard Time", 2027, 1, 15, 12),
            Some(-210)
        );
        assert_eq!(
            off("Newfoundland Standard Time", 2027, 7, 15, 12),
            Some(-150)
        );
        assert_eq!(off("AUS Eastern Standard Time", 2027, 1, 15, 12), Some(660));
        assert_eq!(off("AUS Eastern Standard Time", 2027, 7, 15, 12), Some(600));
        assert_eq!(off("Pacific Standard Time", 2031, 7, 15, 12), Some(-420));
    }

    #[test]
    fn every_windows_key_in_the_table_exists_on_this_machine() {
        let w = WindowsZones::new();
        let mut seen = 0;
        for abbr in [
            "PT", "MT", "CT", "ET", "AKST", "NST", "CET", "EET", "WET", "ACST", "AEST", "NZST",
            "CST", "IST", "BST", "AST",
        ] {
            let ZoneParse::Readings(rs) = parse_zone(abbr) else {
                panic!("{abbr}");
            };
            for reading in rs {
                if let ZoneId::Windows(key) = reading.zone {
                    seen += 1;
                    assert!(w.lookup(key).is_some(), "{abbr}: no Windows zone {key}");
                }
            }
        }
        assert!(seen > 10);
    }
}
