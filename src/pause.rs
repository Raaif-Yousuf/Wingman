//! Pause state (issue #20): pure logic plus the one piece of process-wide
//! state the `WH_KEYBOARD_LL` hook in `hotkey.rs` needs to read on its hot
//! path.
//!
//! See `docs/superpowers/specs/2026-09-16-expansion-plan-design.md`, section
//! "Pause / off": while paused, the hook passes every chord through
//! unchanged (including the Copilot key: no swallowing, no Ctrl tap), no
//! capture runs, no network request can start, and the tray icon is greyed.
//! Awareness does not exist yet in this crate, so there is nothing here to
//! stop for it; a future awareness watcher must consult [`is_paused_now`]
//! the same way `hotkey.rs` and `app.rs`'s `App::ask` do.
//!
//! # Not persisted across restart
//!
//! `PauseState` lives only on `App` (in `app.rs`) and in this module's
//! atomic; nothing writes it to `config.toml`. A restart is an explicit
//! resume -- this is deliberate (see the design doc's "Decisions still
//! owed" / the issue's own scope), not an oversight.
//!
//! # The hook's fast path
//!
//! [`is_paused_now`] is called from `hotkey.rs`'s `hook_proc` on every
//! keydown. It must never allocate and never block: it does one `Relaxed`
//! atomic load plus one `SystemTime::now()` call (a cheap syscall, not a
//! lock), matching the lock-free pattern `dismiss.rs`'s `ARMED` flag already
//! uses for the same reason (see that module's doc comment).

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The three ways to pause, plus the fixed one-hour duration `OneHour` uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseChoice {
    OneHour,
    UntilTomorrow,
    UntilResumed,
}

/// Whether the app is currently paused, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PauseState {
    #[default]
    Running,
    /// `until: None` only for [`PauseChoice::UntilResumed`] -- no deadline,
    /// cleared only by an explicit Resume. `Some(t)` is a wall-clock instant
    /// this module never mutates itself; `app.rs` owns the transition back
    /// to `Running` once it observes [`PauseState::is_paused`] go false.
    Paused {
        choice: PauseChoice,
        until: Option<SystemTime>,
    },
}

impl PauseState {
    /// Whether callers should treat the app as paused right now. A `Paused`
    /// state whose deadline has passed reports `false` here -- this is a
    /// pure predicate, it never mutates `self`; the caller (`app.rs`) is
    /// responsible for actually transitioning to `Running` when this flips.
    pub fn is_paused(&self, now: SystemTime) -> bool {
        match self {
            PauseState::Running => false,
            PauseState::Paused { until: None, .. } => true,
            PauseState::Paused { until: Some(t), .. } => now < *t,
        }
    }
}

/// The fixed duration [`PauseChoice::OneHour`] pauses for.
pub const ONE_HOUR: Duration = Duration::from_secs(3600);

/// Deadline for [`PauseChoice::OneHour`]: exactly one hour from `now`.
/// [`PauseChoice::UntilTomorrow`]'s deadline needs the local calendar date
/// and timezone (DST-aware), which this module has no access to -- see
/// `app.rs`'s `deadline_until_tomorrow`, which uses [`next_day`] for the
/// pure calendar part and Win32's `TzSpecificLocalTimeToSystemTime` for the
/// timezone-aware conversion back to a wall-clock instant.
pub fn deadline_one_hour(now: SystemTime) -> SystemTime {
    now + ONE_HOUR
}

/// A calendar date, independent of any Win32 type, so the day-rollover
/// arithmetic (month lengths, leap years) is unit-tested without a real
/// clock or timezone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LocalDate {
    pub year: i32,
    pub month: u8,
    pub day: u8,
}

pub fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

pub fn days_in_month(year: i32, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        // Defensive: `month` is always 1..=12 for every caller in this
        // crate (it comes straight from `GetLocalTime`'s `wMonth`).
        _ => 30,
    }
}

/// The calendar day after `d`, handling month and year rollover (including
/// the December 31 -> January 1 case and leap-year February).
pub fn next_day(d: LocalDate) -> LocalDate {
    let last = days_in_month(d.year, d.month);
    if d.day < last {
        LocalDate {
            day: d.day + 1,
            ..d
        }
    } else if d.month < 12 {
        LocalDate {
            year: d.year,
            month: d.month + 1,
            day: 1,
        }
    } else {
        LocalDate {
            year: d.year + 1,
            month: 1,
            day: 1,
        }
    }
}

/// Tray tooltip text for a paused state (rule 11: no em dashes). `hour_min`
/// is the deadline's local hour/minute and is only meaningful (required to
/// get a real time in the text) for [`PauseChoice::OneHour`]; the caller
/// resolves it via the OS's local-time conversion since this module has no
/// timezone access of its own -- see `app.rs`'s `local_hour_min`.
pub fn tooltip_text(choice: PauseChoice, hour_min: Option<(u8, u8)>) -> String {
    match choice {
        PauseChoice::OneHour => match hour_min {
            Some((h, m)) => format!("Paused until {h:02}:{m:02}"),
            None => "Paused".to_string(),
        },
        PauseChoice::UntilTomorrow => "Paused until tomorrow".to_string(),
        PauseChoice::UntilResumed => "Paused".to_string(),
    }
}

/// Lock-free pause deadline the hook callback reads on every keydown (see
/// the module doc's "hook's fast path" section).
///
/// `0` = running (not paused). `u64::MAX` = paused with no deadline
/// ([`PauseChoice::UntilResumed`]). Any other value is the deadline as Unix
/// epoch seconds.
static PAUSE_DEADLINE: AtomicU64 = AtomicU64::new(0);

/// Pure decision: given the raw atomic value and the current time (Unix
/// epoch seconds), should the caller treat itself as paused? This is the
/// hook's "pass through vs handle" decision (`hotkey.rs`) and `App::ask`'s
/// network guard (`app.rs`), extracted so it is testable without touching
/// the atomic, Win32, or the system clock.
pub fn raw_should_pass_through(raw: u64, now_epoch_secs: u64) -> bool {
    raw != 0 && (raw == u64::MAX || now_epoch_secs < raw)
}

/// True if paused right now. Allocation-free, lock-free; safe to call from
/// the `WH_KEYBOARD_LL` hot path.
pub fn is_paused_now() -> bool {
    let raw = PAUSE_DEADLINE.load(Ordering::Relaxed);
    if raw == 0 {
        return false;
    }
    let now_epoch = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    raw_should_pass_through(raw, now_epoch)
}

/// Clear the shared pause deadline (Running). Called by `App::resume`.
pub fn set_running() {
    PAUSE_DEADLINE.store(0, Ordering::Relaxed);
}

/// Set the shared pause deadline. `None` marks "paused with no deadline"
/// ([`PauseChoice::UntilResumed`]); `Some(t)` stores `t`. Called by
/// `App::pause_for`.
pub fn set_paused(until: Option<SystemTime>) {
    let epoch = match until {
        None => u64::MAX,
        Some(t) => t
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(u64::MAX),
    };
    PAUSE_DEADLINE.store(epoch, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- calendar rollover (next_day / is_leap_year / days_in_month) -------

    #[test]
    fn next_day_within_month() {
        assert_eq!(
            next_day(LocalDate {
                year: 2026,
                month: 9,
                day: 16
            }),
            LocalDate {
                year: 2026,
                month: 9,
                day: 17
            }
        );
    }

    #[test]
    fn next_day_rolls_month() {
        assert_eq!(
            next_day(LocalDate {
                year: 2026,
                month: 9,
                day: 30
            }),
            LocalDate {
                year: 2026,
                month: 10,
                day: 1
            }
        );
    }

    #[test]
    fn next_day_rolls_year() {
        assert_eq!(
            next_day(LocalDate {
                year: 2026,
                month: 12,
                day: 31
            }),
            LocalDate {
                year: 2027,
                month: 1,
                day: 1
            }
        );
    }

    #[test]
    fn next_day_leap_february() {
        // 2028 is a leap year: Feb 28 -> Feb 29, not straight to March.
        assert!(is_leap_year(2028));
        assert_eq!(
            next_day(LocalDate {
                year: 2028,
                month: 2,
                day: 28
            }),
            LocalDate {
                year: 2028,
                month: 2,
                day: 29
            }
        );
    }

    #[test]
    fn next_day_non_leap_february() {
        assert!(!is_leap_year(2026));
        assert_eq!(
            next_day(LocalDate {
                year: 2026,
                month: 2,
                day: 28
            }),
            LocalDate {
                year: 2026,
                month: 3,
                day: 1
            }
        );
    }

    #[test]
    fn leap_year_rule_handles_century_years() {
        // The century-but-not-400 exception: 1900 and 2100 are NOT leap,
        // 2000 is (also divisible by 400).
        assert!(!is_leap_year(1900));
        assert!(is_leap_year(2000));
        assert!(!is_leap_year(2100));
        assert!(is_leap_year(2024));
        assert!(!is_leap_year(2023));
    }

    #[test]
    fn next_day_leap_day_rolls_to_march() {
        assert_eq!(
            next_day(LocalDate {
                year: 2028,
                month: 2,
                day: 29
            }),
            LocalDate {
                year: 2028,
                month: 3,
                day: 1
            }
        );
    }

    // -- PauseState::is_paused ---------------------------------------------

    #[test]
    fn running_is_never_paused() {
        assert!(!PauseState::Running.is_paused(SystemTime::now()));
    }

    #[test]
    fn until_resumed_is_always_paused() {
        let s = PauseState::Paused {
            choice: PauseChoice::UntilResumed,
            until: None,
        };
        assert!(s.is_paused(SystemTime::now()));
        assert!(s.is_paused(SystemTime::now() + Duration::from_secs(1_000_000)));
    }

    #[test]
    fn deadline_in_future_is_paused() {
        let now = SystemTime::now();
        let s = PauseState::Paused {
            choice: PauseChoice::OneHour,
            until: Some(now + Duration::from_secs(60)),
        };
        assert!(s.is_paused(now));
    }

    #[test]
    fn deadline_in_past_is_not_paused() {
        let now = SystemTime::now();
        let s = PauseState::Paused {
            choice: PauseChoice::OneHour,
            until: Some(now - Duration::from_secs(1)),
        };
        assert!(!s.is_paused(now));
    }

    #[test]
    fn deadline_exactly_now_is_not_paused() {
        // Strict `<`: the boundary instant itself counts as expired.
        let now = SystemTime::now();
        let s = PauseState::Paused {
            choice: PauseChoice::OneHour,
            until: Some(now),
        };
        assert!(!s.is_paused(now));
    }

    // -- deadline_one_hour ---------------------------------------------------

    #[test]
    fn one_hour_adds_exactly_3600_seconds() {
        let now = UNIX_EPOCH + Duration::from_secs(1_000_000);
        assert_eq!(deadline_one_hour(now), now + Duration::from_secs(3600));
    }

    // -- tooltip_text (rule 11: no em dashes) --------------------------------

    #[test]
    fn tooltip_one_hour_formats_hh_mm() {
        assert_eq!(
            tooltip_text(PauseChoice::OneHour, Some((14, 30))),
            "Paused until 14:30"
        );
    }

    #[test]
    fn tooltip_one_hour_pads_single_digits() {
        assert_eq!(
            tooltip_text(PauseChoice::OneHour, Some((9, 5))),
            "Paused until 09:05"
        );
    }

    #[test]
    fn tooltip_until_tomorrow() {
        assert_eq!(
            tooltip_text(PauseChoice::UntilTomorrow, None),
            "Paused until tomorrow"
        );
    }

    #[test]
    fn tooltip_until_resumed() {
        assert_eq!(tooltip_text(PauseChoice::UntilResumed, None), "Paused");
    }

    #[test]
    fn tooltip_one_hour_without_hour_min_degrades_to_plain_paused() {
        // Best-effort: if the local-time conversion failed, still show
        // something rather than nothing (rule 7's spirit -- never silent).
        assert_eq!(tooltip_text(PauseChoice::OneHour, None), "Paused");
    }

    #[test]
    fn no_tooltip_text_contains_an_em_dash() {
        for (choice, hhmm) in [
            (PauseChoice::OneHour, Some((0, 0))),
            (PauseChoice::UntilTomorrow, None),
            (PauseChoice::UntilResumed, None),
        ] {
            assert!(!tooltip_text(choice, hhmm).contains('\u{2014}'));
        }
    }

    // -- the hook's "pass through vs handle" decision ------------------------

    #[test]
    fn zero_means_running_never_passes_through() {
        assert!(!raw_should_pass_through(0, 1_000));
        assert!(!raw_should_pass_through(0, 0));
    }

    #[test]
    fn max_means_indefinite_always_passes_through() {
        assert!(raw_should_pass_through(u64::MAX, 0));
        assert!(raw_should_pass_through(u64::MAX, u64::MAX - 1));
    }

    #[test]
    fn before_deadline_passes_through() {
        assert!(raw_should_pass_through(1_000, 500));
    }

    #[test]
    fn at_or_after_deadline_does_not_pass_through() {
        assert!(!raw_should_pass_through(1_000, 1_000));
        assert!(!raw_should_pass_through(1_000, 1_001));
    }

    // -- the shared atomic, exercised through its real accessors -------------
    //
    // These three tests are the only ones in the crate that touch
    // PAUSE_DEADLINE; each restores Running before returning so it can never
    // leak into another test run in the same process.

    #[test]
    fn set_paused_until_resumed_then_set_running_round_trips() {
        set_paused(None);
        assert!(is_paused_now());
        set_running();
        assert!(!is_paused_now());
    }

    #[test]
    fn set_paused_with_future_deadline_reports_paused() {
        set_paused(Some(SystemTime::now() + Duration::from_secs(120)));
        assert!(is_paused_now());
        set_running();
        assert!(!is_paused_now());
    }

    #[test]
    fn set_paused_with_past_deadline_reports_not_paused() {
        set_paused(Some(SystemTime::now() - Duration::from_secs(5)));
        assert!(!is_paused_now());
        set_running();
    }
}
