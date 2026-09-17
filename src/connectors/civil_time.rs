//! Pure Gregorian civil-calendar arithmetic (#35), used by `ics.rs` for
//! `DTSTAMP` (now, in UTC) and the "missing end" default-duration rule. No
//! `chrono`/`time` dependency: the crate's dependency list is short by
//! design (Cargo.toml has no date/time crate today) and everything needed
//! here is "what UTC instant is it" plus "add N seconds/days to a civil
//! date", not general calendar math.
//!
//! `days_from_civil`/`civil_from_days` are Howard Hinnant's algorithm
//! (public domain; the same one `libc++`'s `<chrono>` and most
//! from-scratch Gregorian converters use), valid across the proleptic
//! Gregorian calendar with no explicit range limit other than `i64`
//! overflow, which is not a practical concern for calendar-event dates.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CivilDateTime {
    pub date: CivilDate,
    pub hour: u8,
    pub minute: u8,
    pub second: u8,
}

/// Days since the Unix epoch (1970-01-01 = 0) for a proleptic-Gregorian
/// civil date. `m`/`d` are not range-checked here -- an out-of-range month
/// or day is a caller bug the parser (`ics::parse_event_time`) is
/// responsible for rejecting before this function ever sees it.
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400; // [0, 399]
    let m = m as i64;
    let d = d as i64;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1; // [0, 365]
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy; // [0, 146096]
    era * 146097 + doe - 719468
}

/// Inverse of [`days_from_civil`].
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Seconds since the Unix epoch for a UTC civil date-time.
pub fn civil_datetime_to_unix(dt: &CivilDateTime) -> i64 {
    days_from_civil(
        dt.date.year as i64,
        dt.date.month as u32,
        dt.date.day as u32,
    ) * 86_400
        + dt.hour as i64 * 3_600
        + dt.minute as i64 * 60
        + dt.second as i64
}

/// Inverse of [`civil_datetime_to_unix`].
pub fn unix_to_civil_datetime(secs: i64) -> CivilDateTime {
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    CivilDateTime {
        date: CivilDate {
            year: y as i32,
            month: m as u8,
            day: d as u8,
        },
        hour: (sod / 3_600) as u8,
        minute: ((sod % 3_600) / 60) as u8,
        second: (sod % 60) as u8,
    }
}

/// Adds (or subtracts, for a negative `delta`) whole seconds to a UTC civil
/// date-time, carrying across day/month/year boundaries.
pub fn add_seconds(dt: &CivilDateTime, delta: i64) -> CivilDateTime {
    unix_to_civil_datetime(civil_datetime_to_unix(dt) + delta)
}

/// Adds (or subtracts) whole days to a civil date, carrying across
/// month/year boundaries.
pub fn add_days(date: &CivilDate, delta: i64) -> CivilDate {
    let days = days_from_civil(date.year as i64, date.month as u32, date.day as u32) + delta;
    let (y, m, d) = civil_from_days(days);
    CivilDate {
        year: y as i32,
        month: m as u8,
        day: d as u8,
    }
}

/// The current instant, in UTC civil time. The one place this module reads
/// the system clock; every other function is pure and independently
/// testable without it.
pub fn now_utc() -> CivilDateTime {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    unix_to_civil_datetime(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_is_day_zero() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn known_reference_dates_match_their_known_day_counts() {
        // 2000-03-01 is a well-known reference point for this algorithm
        // (the start of a 400-year era boundary check): day 11017.
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_from_days(11_017), (2000, 3, 1));

        // A pre-epoch date: 1969-12-31 is day -1.
        assert_eq!(days_from_civil(1969, 12, 31), -1);
        assert_eq!(civil_from_days(-1), (1969, 12, 31));

        // A far pre-epoch date, to exercise the negative-era branch.
        assert_eq!(civil_from_days(days_from_civil(1600, 1, 1)), (1600, 1, 1));
    }

    #[test]
    fn leap_year_rules_are_gregorian_not_julian() {
        // 2000 is divisible by 400: a leap year. Feb has 29 days.
        assert_eq!(
            days_from_civil(2000, 2, 29) + 1,
            days_from_civil(2000, 3, 1)
        );
        // 1900 is divisible by 100 but not 400: NOT a leap year. Feb 29
        // does not exist, so Feb 28 and Mar 1 are consecutive days (a gap
        // of 1, not 2) -- unlike the leap-year cases above and below,
        // where Feb 29 sits between them and the gap is 2.
        assert_eq!(
            days_from_civil(1900, 3, 1) - days_from_civil(1900, 2, 28),
            1
        );
        // 2004 is divisible by 4 but not 100: a leap year.
        assert_eq!(
            days_from_civil(2004, 2, 29) + 1,
            days_from_civil(2004, 3, 1)
        );
    }

    #[test]
    fn round_trips_every_day_across_a_multi_century_range() {
        // Not a handful of spot values: a hand-rolled calendar algorithm is
        // exactly the kind of code a narrow example set gives false
        // confidence about (design doc's "Verification note"). Covers
        // 1900-01-01 through 2100-12-31 inclusive, well past any date a
        // calendar event proposal will plausibly name.
        let start = days_from_civil(1900, 1, 1);
        let end = days_from_civil(2100, 12, 31);
        for day in start..=end {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(
                days_from_civil(y, m, d),
                day,
                "round trip failed for day {day} -> {y:04}-{m:02}-{d:02}"
            );
        }
    }

    #[test]
    fn civil_datetime_round_trips_through_unix_seconds() {
        let dt = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 20,
            },
            hour: 14,
            minute: 30,
            second: 15,
        };
        let secs = civil_datetime_to_unix(&dt);
        assert_eq!(unix_to_civil_datetime(secs), dt);
    }

    #[test]
    fn add_seconds_carries_across_a_day_boundary() {
        let dt = CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 20,
            },
            hour: 23,
            minute: 30,
            second: 0,
        };
        let plus_one_hour = add_seconds(&dt, 3_600);
        assert_eq!(
            plus_one_hour,
            CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 21,
                },
                hour: 0,
                minute: 30,
                second: 0,
            }
        );
    }

    #[test]
    fn add_days_carries_across_a_month_and_year_boundary() {
        let dec_31 = CivilDate {
            year: 2026,
            month: 12,
            day: 31,
        };
        assert_eq!(
            add_days(&dec_31, 1),
            CivilDate {
                year: 2027,
                month: 1,
                day: 1
            }
        );

        let jan_31 = CivilDate {
            year: 2026,
            month: 1,
            day: 31,
        };
        assert_eq!(
            add_days(&jan_31, 1),
            CivilDate {
                year: 2026,
                month: 2,
                day: 1
            }
        );
    }

    #[test]
    fn now_utc_returns_a_plausible_recent_year() {
        // Not a golden value (the clock moves) -- just a sanity bound that
        // catches a badly wired epoch/scale bug (e.g. using millis where
        // seconds were expected would land far outside this range).
        let now = now_utc();
        assert!(
            (2020..=2100).contains(&now.date.year),
            "now_utc produced an implausible year: {}",
            now.date.year
        );
    }
}
