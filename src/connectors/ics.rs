//! The zero-auth `ics` connector (#35): writes an RFC 5545
//! `VCALENDAR`/`VEVENT` to a temp file and hands it to the default calendar
//! handler (`ShellExecuteW("open", ...)`, behind the injectable [`Opener`]
//! trait so tests never launch a real handler). See the connector design
//! doc's ".ics generation rules" for the folding/escaping/timezone rules
//! this file implements.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};

use super::civil_time::{self, CivilDate, CivilDateTime};
use super::{AuthKind, CalendarEvent, CalendarEventResult, Capability, Connector, EventTime};

/// Hands a generated file to the OS's default handler for it. Abstracted so
/// tests never call the real shell (rule 9's failure shape: "touches a
/// real shared OS resource from a test" is the same problem a named kernel
/// object is).
pub trait Opener: Send + Sync {
    fn open(&self, path: &Path) -> Result<()>;
}

/// `ShellExecuteW("open", <path>, ...)`, the same call `app.rs`'s
/// `edit_settings` already makes to open `config.toml` in its default
/// handler.
pub struct ShellOpener;

impl Opener for ShellOpener {
    fn open(&self, path: &Path) -> Result<()> {
        use std::os::windows::ffi::OsStrExt;

        use windows::core::{w, PCWSTR};
        use windows::Win32::UI::Shell::ShellExecuteW;
        use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        let wide: Vec<u16> = path
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer kept alive for
        // the whole call; every other argument is `None`/a static verb
        // string, the same shape `app.rs::edit_settings` already uses.
        let code = unsafe {
            ShellExecuteW(
                None,
                w!("open"),
                PCWSTR(wide.as_ptr()),
                None,
                None,
                SW_SHOWNORMAL,
            )
        };
        // MSDN: a return value greater than 32 indicates success; anything
        // else is an error code (`SE_ERR_*` or a Win32 error code).
        if (code.0 as isize) > 32 {
            Ok(())
        } else {
            Err(anyhow!(
                "ShellExecuteW(\"open\") failed with code {}",
                code.0 as isize
            ))
        }
    }
}

/// The `ics` connector. `O: Opener = ShellOpener` mirrors
/// `ClipboardExecutor<C: ClipboardAccess = ArboardClipboard>`: a real
/// default type parameter for production, an explicit constructor for
/// tests, so no test ever calls the real shell or writes into the real
/// `%TEMP%\Wingman`.
pub struct IcsConnector<O: Opener = ShellOpener> {
    /// Injectable so tests use their own temp dir (rule 9): production is
    /// `%TEMP%\Wingman`, tests use `std::env::temp_dir()` joined with a
    /// per-test-process-unique subdirectory (see `mod tests`).
    temp_dir: PathBuf,
    opener: O,
}

impl IcsConnector<ShellOpener> {
    /// Production constructor: `%TEMP%\Wingman`, the real shell opener.
    pub fn new() -> Self {
        Self {
            temp_dir: std::env::temp_dir().join("Wingman"),
            opener: ShellOpener,
        }
    }
}

impl Default for IcsConnector<ShellOpener> {
    fn default() -> Self {
        Self::new()
    }
}

impl<O: Opener> IcsConnector<O> {
    /// Test/forward-wiring constructor: an explicit temp dir and opener, so
    /// a test never touches `%TEMP%\Wingman` or the real shell. Unused
    /// outside tests (this file's and `executors::calendar_add`'s) until a
    /// non-test caller injects a non-default opener.
    #[allow(dead_code)]
    pub fn with_temp_dir_and_opener(temp_dir: PathBuf, opener: O) -> Self {
        Self { temp_dir, opener }
    }
}

impl<O: Opener> Connector for IcsConnector<O> {
    fn id(&self) -> &'static str {
        "ics"
    }

    fn auth_kind(&self) -> AuthKind {
        AuthKind::None
    }

    fn capabilities(&self) -> &'static [Capability] {
        &[Capability::CalendarWrite]
    }

    fn create_calendar_event(&self, event: &CalendarEvent) -> Result<CalendarEventResult> {
        let uid = generate_uid();
        let dtstamp = civil_time::now_utc();
        let content = render_ics(event, &uid, &dtstamp);

        std::fs::create_dir_all(&self.temp_dir)
            .with_context(|| format!("could not create {}", self.temp_dir.display()))?;
        let path = self
            .temp_dir
            .join(format!("{}.ics", sanitize_filename(&uid)));
        std::fs::write(&path, content.as_bytes())
            .with_context(|| format!("could not write {}", path.display()))?;

        // A failed open still leaves a written file behind: the confirm
        // card can name the path for the user to open by hand (see
        // `CalendarEventResult::opened`'s doc comment). Not propagated as
        // `Err` -- the write, which is the part this connector fully
        // controls, already succeeded.
        let opened = self.opener.open(&path).is_ok();

        Ok(CalendarEventResult { path, opened })
    }
}

/// `<millis-since-epoch>-<per-process counter>@wingman.local`. Uniqueness
/// only needs to hold within one machine's generated files (RFC 5545's UID
/// only needs to be unique within the producing identifier's namespace,
/// here `wingman.local`), which wall-clock milliseconds plus a monotonic
/// counter already gives -- no `uuid` dependency needed.
fn generate_uid() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{millis}-{counter}@wingman.local")
}

/// A UID is already filesystem-safe (digits, one `-`, one `@`, `wingman`,
/// one `.`, `local`) except for `@`, which some tools mis-handle in a
/// filename; replaced with `_` for the on-disk name only. The UID inside
/// the file content is untouched.
fn sanitize_filename(uid: &str) -> String {
    uid.replace('@', "_")
}

fn format_date(d: &CivilDate) -> String {
    format!("{:04}{:02}{:02}", d.year, d.month, d.day)
}

fn format_datetime(dt: &CivilDateTime) -> String {
    format!(
        "{}T{:02}{:02}{:02}",
        format_date(&dt.date),
        dt.hour,
        dt.minute,
        dt.second
    )
}

/// Formats one of `DTSTART`/`DTEND`/etc. with its value, per the connector
/// design doc's "DTSTART/DTEND" rule.
fn format_event_time_property(name: &str, t: &EventTime) -> String {
    match t {
        EventTime::AllDay(d) => format!("{name};VALUE=DATE:{}", format_date(d)),
        EventTime::Utc(dt) => format!("{name}:{}Z", format_datetime(dt)),
        EventTime::Local { at, tzid } => format!("{name};TZID={tzid}:{}", format_datetime(at)),
    }
}

/// The "missing end" default duration (connector design doc): one day for
/// an all-day start, one hour otherwise -- matching RFC 5545's own
/// single-all-day-event convention and most calendar UIs' own default for
/// a start-only event, respectively.
fn default_end(start: &EventTime) -> EventTime {
    match start {
        EventTime::AllDay(d) => EventTime::AllDay(civil_time::add_days(d, 1)),
        EventTime::Utc(dt) => EventTime::Utc(civil_time::add_seconds(dt, 3_600)),
        EventTime::Local { at, tzid } => EventTime::Local {
            at: civil_time::add_seconds(at, 3_600),
            tzid: tzid.clone(),
        },
    }
}

/// Escapes RFC 5545 `TEXT` value special characters (§3.3.11): backslash,
/// comma, semicolon, and a line break to the literal two-character `\n`.
/// Applied before folding (folding operates on the already-escaped octet
/// stream, per the connector design doc). A bare CR (no accompanying LF) is
/// dropped rather than escaped: a JSON string value (the wire shape every
/// caller of this connector goes through) uses LF line breaks, not
/// old-Mac-style bare CR, so this is a deliberate scope limitation, not an
/// unmeasured guess.
fn escape_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            ',' => out.push_str("\\,"),
            ';' => out.push_str("\\;"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            other => out.push(other),
        }
    }
    out
}

/// Folds one unfolded content line to RFC 5545's 75-octet limit (§3.1):
/// the first physical line carries up to 75 octets, each continuation line
/// carries a single leading space (counted toward its own 75-octet budget)
/// plus up to 74 more octets of content, joined by CRLF. Never splits a
/// multi-byte UTF-8 sequence: the cut point backs up to the nearest earlier
/// character boundary when the exact octet limit would land inside one.
fn fold_line(line: &str) -> String {
    const FIRST_LIMIT: usize = 75;
    const CONT_LIMIT: usize = 74;

    if line.len() <= FIRST_LIMIT {
        return line.to_string();
    }

    let mut out = String::new();
    let mut rest = line;
    let mut first = true;
    loop {
        let limit = if first { FIRST_LIMIT } else { CONT_LIMIT };
        if rest.len() <= limit {
            if !first {
                out.push(' ');
            }
            out.push_str(rest);
            break;
        }
        let mut cut = limit;
        while !rest.is_char_boundary(cut) {
            cut -= 1;
        }
        if !first {
            out.push(' ');
        }
        out.push_str(&rest[..cut]);
        out.push_str("\r\n");
        rest = &rest[cut..];
        first = false;
    }
    out
}

/// Pure and independently testable with a fixed `uid`/`dtstamp`: the
/// connector's `create_calendar_event` is the only caller that generates
/// fresh (non-deterministic) values for those two fields.
fn render_ics(event: &CalendarEvent, uid: &str, dtstamp: &CivilDateTime) -> String {
    let end = event
        .end
        .clone()
        .unwrap_or_else(|| default_end(&event.start));

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Wingman//ics connector//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}Z", format_datetime(dtstamp)),
        format_event_time_property("DTSTART", &event.start),
        format_event_time_property("DTEND", &end),
        format!("SUMMARY:{}", escape_text(&event.title)),
    ];
    if let Some(location) = event.location.as_ref().filter(|s| !s.is_empty()) {
        lines.push(format!("LOCATION:{}", escape_text(location)));
    }
    if let Some(description) = event.description.as_ref().filter(|s| !s.is_empty()) {
        lines.push(format!("DESCRIPTION:{}", escape_text(description)));
    }
    lines.push("END:VEVENT".to_string());
    lines.push("END:VCALENDAR".to_string());

    let mut out = String::new();
    for line in &lines {
        out.push_str(&fold_line(line));
        out.push_str("\r\n");
    }
    out
}

/// Parses a `calendar_event` proposal's `start`/`end` string into an
/// [`EventTime`]. Accepts `YYYY-MM-DD` (-> [`EventTime::AllDay`]) or
/// `YYYY-MM-DDTHH:MM[:SS](Z|+HH:MM|-HH:MM)` (-> [`EventTime::Utc`], offset
/// converted). See the connector design doc's "Timezone handling" section
/// for why a date-time with no offset is a hard error rather than a
/// floating-time guess.
pub fn parse_event_time(input: &str) -> Result<EventTime> {
    let trimmed = input.trim();
    if let Some(date) = try_parse_date_only(trimmed) {
        return Ok(EventTime::AllDay(date));
    }
    parse_datetime_with_offset(trimmed).ok_or_else(|| {
        anyhow!(
            "could not parse \"{trimmed}\" as a date (YYYY-MM-DD) or a date-time with a UTC offset (YYYY-MM-DDTHH:MM:SSZ or +HH:MM)"
        )
    })
}

fn try_parse_date_only(s: &str) -> Option<CivilDate> {
    let parts: Vec<&str> = s.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return None;
    }
    let year: i32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let day: u32 = parts[2].parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    // A hand-rolled Gregorian formula does not itself know how many days
    // each month has; round-tripping through the same day-count functions
    // `civil_time`'s own tests verify is what catches an impossible date
    // like 2026-02-30 (see the connector design doc's "Verification note").
    let days = civil_time::days_from_civil(year as i64, month, day);
    let (ry, rm, rd) = civil_time::civil_from_days(days);
    if ry == year as i64 && rm == month && rd == day {
        Some(CivilDate {
            year,
            month: month as u8,
            day: day as u8,
        })
    } else {
        None
    }
}

fn parse_datetime_with_offset(s: &str) -> Option<EventTime> {
    let (date_part, rest) = s.split_once('T')?;
    let date = try_parse_date_only(date_part)?;

    let (time_part, offset_seconds) = if let Some(stripped) = rest.strip_suffix('Z') {
        (stripped, 0i64)
    } else {
        let pos = rest.rfind(['+', '-'])?;
        (&rest[..pos], parse_offset(&rest[pos..])?)
    };

    let time_parts: Vec<&str> = time_part.split(':').collect();
    if time_parts.len() < 2 || time_parts.len() > 3 {
        return None;
    }
    let hour: u32 = time_parts[0].parse().ok()?;
    let minute: u32 = time_parts[1].parse().ok()?;
    let second: u32 = if time_parts.len() == 3 {
        time_parts[2].parse().ok()?
    } else {
        0
    };
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    let local_dt = CivilDateTime {
        date,
        hour: hour as u8,
        minute: minute as u8,
        second: second as u8,
    };
    let utc_secs = civil_time::civil_datetime_to_unix(&local_dt) - offset_seconds;
    Some(EventTime::Utc(civil_time::unix_to_civil_datetime(utc_secs)))
}

/// Parses a `+HH:MM`/`-HH:MM` UTC offset into signed seconds east of UTC.
fn parse_offset(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() != 6 || bytes[3] != b':' {
        return None;
    }
    let sign: i64 = match bytes[0] {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let hh: i64 = s[1..3].parse().ok()?;
    let mm: i64 = s[4..6].parse().ok()?;
    if hh > 23 || mm > 59 {
        return None;
    }
    Some(sign * (hh * 3_600 + mm * 60))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    // -- test doubles --------------------------------------------------

    /// Records every path it was asked to open; never touches a real shell
    /// (rule 9's kernel-object failure shape, applied to "open a file with
    /// the OS default handler").
    #[derive(Default)]
    struct RecordingOpener {
        opened: RefCell<Vec<PathBuf>>,
        fail: bool,
    }

    // `RefCell` is not `Sync`; each test constructs and uses its own
    // instance on one thread, never sharing it across threads (see
    // `clipboard.rs`'s `FakeClipboard` for the same reasoning).
    unsafe impl Sync for RecordingOpener {}

    impl Opener for RecordingOpener {
        fn open(&self, path: &Path) -> Result<()> {
            self.opened.borrow_mut().push(path.to_path_buf());
            if self.fail {
                Err(anyhow!("fake opener configured to fail"))
            } else {
                Ok(())
            }
        }
    }

    fn test_temp_dir(label: &str) -> PathBuf {
        // A per-test, per-process-unique subdirectory under the real OS
        // temp dir -- never `%TEMP%\Wingman` (rule 9: tests never touch
        // production names/paths). `std::process::id()` plus a counter
        // keeps parallel test threads from colliding.
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "wingman-ics-test-{}-{}-{label}",
            std::process::id(),
            n
        ))
    }

    fn sample_event() -> CalendarEvent {
        CalendarEvent {
            title: "Standup".to_string(),
            start: EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20,
                },
                hour: 14,
                minute: 0,
                second: 0,
            }),
            end: Some(EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20,
                },
                hour: 14,
                minute: 30,
                second: 0,
            })),
            location: Some("Room 4B".to_string()),
            description: Some("Daily sync".to_string()),
        }
    }

    fn fixed_dtstamp() -> CivilDateTime {
        CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 17,
            },
            hour: 9,
            minute: 0,
            second: 0,
        }
    }

    // -- golden .ics output ---------------------------------------------

    #[test]
    fn golden_ics_for_a_sample_event() {
        let ics = render_ics(&sample_event(), "1234-0@wingman.local", &fixed_dtstamp());
        let expected = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Wingman//ics connector//EN\r\n\
CALSCALE:GREGORIAN\r\n\
BEGIN:VEVENT\r\n\
UID:1234-0@wingman.local\r\n\
DTSTAMP:20260917T090000Z\r\n\
DTSTART:20260920T140000Z\r\n\
DTEND:20260920T143000Z\r\n\
SUMMARY:Standup\r\n\
LOCATION:Room 4B\r\n\
DESCRIPTION:Daily sync\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert_eq!(ics, expected);
    }

    #[test]
    fn golden_ics_for_an_all_day_event_with_no_location_or_description() {
        let event = CalendarEvent {
            title: "Conference".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 1,
            }),
            end: None,
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "9999-1@wingman.local", &fixed_dtstamp());
        let expected = "BEGIN:VCALENDAR\r\n\
VERSION:2.0\r\n\
PRODID:-//Wingman//ics connector//EN\r\n\
CALSCALE:GREGORIAN\r\n\
BEGIN:VEVENT\r\n\
UID:9999-1@wingman.local\r\n\
DTSTAMP:20260917T090000Z\r\n\
DTSTART;VALUE=DATE:20261001\r\n\
DTEND;VALUE=DATE:20261002\r\n\
SUMMARY:Conference\r\n\
END:VEVENT\r\n\
END:VCALENDAR\r\n";
        assert_eq!(ics, expected, "all-day missing-end default is +1 day");
    }

    #[test]
    fn missing_end_on_a_timed_event_defaults_to_one_hour_later() {
        let event = CalendarEvent {
            title: "Quick call".to_string(),
            start: EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20,
                },
                hour: 14,
                minute: 0,
                second: 0,
            }),
            end: None,
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp());
        assert!(ics.contains("DTSTART:20260920T140000Z\r\n"));
        assert!(ics.contains("DTEND:20260920T150000Z\r\n"));
    }

    #[test]
    fn local_timezone_event_renders_a_tzid_parameter() {
        let event = CalendarEvent {
            title: "Local meeting".to_string(),
            start: EventTime::Local {
                at: CivilDateTime {
                    date: CivilDate {
                        year: 2026,
                        month: 9,
                        day: 20,
                    },
                    hour: 9,
                    minute: 0,
                    second: 0,
                },
                tzid: "America/New_York".to_string(),
            },
            end: None,
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp());
        assert!(ics.contains("DTSTART;TZID=America/New_York:20260920T090000\r\n"));
        assert!(ics.contains("DTEND;TZID=America/New_York:20260920T100000\r\n"));
    }

    // -- folding at 75 octets --------------------------------------------

    #[test]
    fn a_short_line_is_not_folded() {
        let line = "SUMMARY:short";
        assert_eq!(fold_line(line), line);
    }

    #[test]
    fn a_line_over_75_octets_is_folded_with_crlf_and_a_leading_space() {
        // "SUMMARY:" (8) + 70 'a's = 78 octets, 3 over the limit.
        let value = "a".repeat(70);
        let line = format!("SUMMARY:{value}");
        assert_eq!(line.len(), 78);

        let folded = fold_line(&line);
        let parts: Vec<&str> = folded.split("\r\n").collect();
        assert_eq!(parts.len(), 2, "expected exactly one fold: {folded:?}");
        assert_eq!(parts[0].len(), 75, "first physical line must be 75 octets");
        assert!(
            parts[1].starts_with(' '),
            "continuation line must start with a single leading space"
        );
        // Rejoining (stripping the CRLF and the one leading space per
        // continuation line) must reproduce the original content exactly.
        assert_eq!(format!("{}{}", parts[0], &parts[1][1..]), line);
    }

    #[test]
    fn folding_does_not_split_a_multi_byte_utf8_character() {
        // 'é' is 2 octets in UTF-8. Force the naive 75th-octet cut to land
        // inside one: 74 ASCII octets of prefix plus a multi-byte char
        // straddling the boundary.
        let prefix = "a".repeat(74);
        let line = format!("{prefix}éé more text to push this line past 75 octets total");
        let folded = fold_line(&line);
        assert!(
            folded.is_char_boundary(0),
            "sanity: folded text must itself be valid UTF-8 (would already panic otherwise)"
        );
        // Every fragment between fold points must be valid UTF-8 on its
        // own -- `String`'s own invariants guarantee this already, but the
        // real assertion is that no byte was dropped or duplicated.
        let rejoined: String = folded
            .split("\r\n")
            .enumerate()
            .map(|(i, part)| if i == 0 { part } else { &part[1..] })
            .collect();
        assert_eq!(rejoined, line);
    }

    #[test]
    fn a_line_needing_two_folds_produces_three_physical_lines() {
        // "SUMMARY:" (8) + 200 'b's = 208 octets: first line 75, then two
        // more continuation chunks of up to 74 octets each covering the
        // remaining 133 octets (75 + 74 + 59 -- wait, compute from the
        // function itself via the round-trip check below rather than
        // hand-deriving the exact split).
        let value = "b".repeat(200);
        let line = format!("SUMMARY:{value}");
        let folded = fold_line(&line);
        let parts: Vec<&str> = folded.split("\r\n").collect();
        assert!(
            parts.len() >= 3,
            "expected multiple folds: got {} lines",
            parts.len()
        );
        for (i, part) in parts.iter().enumerate() {
            // Every physical line -- the first and every continuation line,
            // the latter's leading space included -- is at most 75 octets.
            assert!(
                part.len() <= 75,
                "line {i} exceeds 75 octets: {} -> {part:?}",
                part.len()
            );
        }
        let rejoined: String = parts
            .iter()
            .enumerate()
            .map(|(i, part)| if i == 0 { *part } else { &part[1..] })
            .collect();
        assert_eq!(rejoined, line);
    }

    // -- escaping ----------------------------------------------------------

    #[test]
    fn escapes_backslash_comma_semicolon_and_newline() {
        let cases: &[(&str, &str)] = &[
            ("a,b", "a\\,b"),
            ("a;b", "a\\;b"),
            ("a\\b", "a\\\\b"),
            ("a\nb", "a\\nb"),
            ("a\r\nb", "a\\nb"),
            ("plain text", "plain text"),
            (
                "comma, semi; back\\slash\nline",
                "comma\\, semi\\; back\\\\slash\\nline",
            ),
        ];
        for (input, expected) in cases {
            assert_eq!(escape_text(input), *expected, "failed for {input:?}");
        }
    }

    #[test]
    fn golden_ics_escapes_special_characters_in_summary_and_description() {
        let event = CalendarEvent {
            title: "Team, sync; notes\\here".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 9,
                day: 20,
            }),
            end: None,
            location: None,
            description: Some("line one\nline two".to_string()),
        };
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp());
        assert!(ics.contains("SUMMARY:Team\\, sync\\; notes\\\\here\r\n"));
        assert!(ics.contains("DESCRIPTION:line one\\nline two\r\n"));
    }

    // -- parse_event_time ---------------------------------------------------

    #[test]
    fn parses_a_date_only_string_as_all_day() {
        let t = parse_event_time("2026-09-20").unwrap();
        assert_eq!(
            t,
            EventTime::AllDay(CivilDate {
                year: 2026,
                month: 9,
                day: 20
            })
        );
    }

    #[test]
    fn parses_a_utc_datetime_with_z_suffix() {
        let t = parse_event_time("2026-09-20T14:00:00Z").unwrap();
        assert_eq!(
            t,
            EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20
                },
                hour: 14,
                minute: 0,
                second: 0
            })
        );
    }

    #[test]
    fn parses_a_datetime_with_a_positive_offset_and_converts_to_utc() {
        // 14:00 at +02:00 is 12:00 UTC.
        let t = parse_event_time("2026-09-20T14:00:00+02:00").unwrap();
        assert_eq!(
            t,
            EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20
                },
                hour: 12,
                minute: 0,
                second: 0
            })
        );
    }

    #[test]
    fn parses_a_datetime_with_a_negative_offset_and_converts_to_utc_across_a_day_boundary() {
        // 23:00 at -07:00 is 06:00 UTC the next day.
        let t = parse_event_time("2026-09-20T23:00:00-07:00").unwrap();
        assert_eq!(
            t,
            EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 21
                },
                hour: 6,
                minute: 0,
                second: 0
            })
        );
    }

    #[test]
    fn parses_a_datetime_with_no_seconds() {
        let t = parse_event_time("2026-09-20T14:00Z").unwrap();
        assert_eq!(
            t,
            EventTime::Utc(CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20
                },
                hour: 14,
                minute: 0,
                second: 0
            })
        );
    }

    #[test]
    fn rejects_a_datetime_with_no_offset() {
        // THEORY (unverified, connector design doc): a floating local time
        // with no offset is a hard error, not a UTC/local-machine guess.
        let err = parse_event_time("2026-09-20T14:00:00")
            .expect_err("a date-time with no offset must be rejected");
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn rejects_an_impossible_calendar_date() {
        assert!(parse_event_time("2026-02-30").is_err());
        assert!(parse_event_time("2026-13-01").is_err());
    }

    #[test]
    fn rejects_garbage_input_with_a_named_error() {
        let err = parse_event_time("next tuesday afternoon")
            .expect_err("free text must not silently parse");
        assert!(err.to_string().contains("next tuesday afternoon"));
    }

    // -- IcsConnector: writes a file and calls the injected opener ----------

    #[test]
    fn ics_connector_is_zero_auth_and_declares_calendar_write() {
        let connector = IcsConnector::with_temp_dir_and_opener(
            test_temp_dir("caps"),
            RecordingOpener::default(),
        );
        assert_eq!(connector.id(), "ics");
        assert_eq!(connector.auth_kind(), AuthKind::None);
        assert!(connector
            .capabilities()
            .contains(&Capability::CalendarWrite));
    }

    #[test]
    fn create_calendar_event_writes_a_file_and_calls_the_opener_exactly_once() {
        let dir = test_temp_dir("writes");
        let opener = RecordingOpener::default();
        let connector = IcsConnector::with_temp_dir_and_opener(dir.clone(), opener);

        let result = connector.create_calendar_event(&sample_event()).unwrap();

        assert!(result.opened);
        assert!(result.path.starts_with(&dir));
        assert!(result.path.extension().is_some_and(|e| e == "ics"));
        let written = std::fs::read_to_string(&result.path).unwrap();
        assert!(written.contains("SUMMARY:Standup"));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_calendar_event_reports_opened_false_when_the_opener_fails_but_still_writes_the_file()
    {
        let dir = test_temp_dir("open-fails");
        let opener = RecordingOpener {
            opened: RefCell::new(Vec::new()),
            fail: true,
        };
        let connector = IcsConnector::with_temp_dir_and_opener(dir.clone(), opener);

        let result = connector.create_calendar_event(&sample_event()).unwrap();

        assert!(!result.opened);
        assert!(result.path.exists(), "the .ics file must still be written");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_calendar_event_never_calls_the_real_shell() {
        // The whole point of `Opener`: this test constructs and drives a
        // real `IcsConnector::create_calendar_event` call end to end
        // without ever invoking `ShellExecuteW` or opening any real file
        // handler window. If this test's process ever pops a calendar app,
        // that is the bug this test exists to catch.
        let dir = test_temp_dir("no-real-shell");
        let opener = RecordingOpener::default();
        let connector = IcsConnector::with_temp_dir_and_opener(dir.clone(), opener);
        connector.create_calendar_event(&sample_event()).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }
}
