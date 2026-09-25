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

/// Converts a local wall-clock time to UTC, DST-correct. Abstracted so
/// tests never call the real Win32 timezone API (rule 9's failure shape:
/// same reasoning as [`Opener`]).
///
/// **Issue #211**: `TzSpecificLocalTimeToSystemTime(None, ...)` (the same
/// call `app.rs`'s `deadline_until_tomorrow` already uses for Pause) only
/// ever converts using the **machine's own currently active time zone**.
/// Win32 exposes no API this crate calls to convert an arbitrary *named*
/// zone (an IANA `tzid` such as `"America/New_York"`) without a zone
/// database this crate does not depend on (rule 2: the dependency list is
/// short by design, and no `chrono-tz`/`tzdata` crate is pulled in for
/// this). A production [`Win32LocalTimeConverter`] therefore converts
/// `at` as if it already were a wall-clock time in the machine's own zone,
/// regardless of what `tzid` names; [`render_ics`]'s caller
/// (`create_calendar_event`) only uses the result when this succeeds, and
/// falls back to a floating local time (see `format_event_time_property`)
/// otherwise -- see that function's doc comment for why a floating time,
/// not a bare `TZID` with no `VTIMEZONE`, is the fallback.
pub trait LocalTimeConverter: Send + Sync {
    /// `None` when the conversion cannot be performed (an invalid or
    /// ambiguous wall-clock time during a DST transition, or the
    /// underlying Win32 call failing for any other reason).
    fn to_utc(&self, at: &CivilDateTime) -> Option<CivilDateTime>;
}

/// Production [`LocalTimeConverter`]: `TzSpecificLocalTimeToSystemTime`
/// against the machine's own currently active zone. Win32, so checked by
/// hand per rule 8, not unit-tested directly -- the pure decision this
/// feeds (`resolve_local_times`, below) is what is actually tested, with a
/// scripted fake converter.
pub struct Win32LocalTimeConverter;

impl LocalTimeConverter for Win32LocalTimeConverter {
    fn to_utc(&self, at: &CivilDateTime) -> Option<CivilDateTime> {
        use windows::Win32::Foundation::SYSTEMTIME;
        use windows::Win32::System::Time::TzSpecificLocalTimeToSystemTime;

        let local = SYSTEMTIME {
            wYear: at.date.year as u16,
            wMonth: at.date.month as u16,
            wDay: at.date.day as u16,
            wHour: at.hour as u16,
            wMinute: at.minute as u16,
            wSecond: at.second as u16,
            wMilliseconds: 0,
            wDayOfWeek: 0,
        };
        let mut utc = SYSTEMTIME::default();
        // SAFETY: both SYSTEMTIME values are stack-local and valid for the
        // duration of this call; `None` for the zone parameter asks for
        // the machine's own currently active time zone, the same call
        // `app.rs`'s `deadline_until_tomorrow` already makes for Pause.
        unsafe { TzSpecificLocalTimeToSystemTime(None, &local, &mut utc) }.ok()?;

        Some(CivilDateTime {
            date: CivilDate {
                year: utc.wYear as i32,
                month: utc.wMonth as u8,
                day: utc.wDay as u8,
            },
            hour: utc.wHour as u8,
            minute: utc.wMinute as u8,
            second: utc.wSecond as u8,
        })
    }
}

/// The `ics` connector. `O: Opener = ShellOpener` mirrors
/// `ClipboardExecutor<C: ClipboardAccess = ArboardClipboard>`: a real
/// default type parameter for production, an explicit constructor for
/// tests, so no test ever calls the real shell or writes into the real
/// `%TEMP%\Wingman`. `local_time_converter` is a boxed trait object, not a
/// second generic parameter: Rust does not apply a struct's default type
/// parameters during ordinary call-site inference, so a second `<O, C =
/// Win32LocalTimeConverter>` parameter would break every existing
/// `IcsConnector::<SomeOpener>` call site that does not also name `C`
/// explicitly (including this file's and `executors::calendar_add`'s own
/// tests) -- a boxed trait object avoids that without touching any of
/// them.
pub struct IcsConnector<O: Opener = ShellOpener> {
    /// Injectable so tests use their own temp dir (rule 9): production is
    /// `%TEMP%\Wingman`, tests use `std::env::temp_dir()` joined with a
    /// per-test-process-unique subdirectory (see `mod tests`).
    temp_dir: PathBuf,
    opener: O,
    local_time_converter: Box<dyn LocalTimeConverter>,
}

impl IcsConnector<ShellOpener> {
    /// Production constructor: `%TEMP%\Wingman`, the real shell opener, the
    /// real Win32 timezone converter.
    pub fn new() -> Self {
        Self {
            temp_dir: std::env::temp_dir().join("Wingman"),
            opener: ShellOpener,
            local_time_converter: Box::new(Win32LocalTimeConverter),
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
    /// a test never touches `%TEMP%\Wingman` or the real shell. Still uses
    /// the real [`Win32LocalTimeConverter`] (harmless: nothing in this
    /// file's or `executors::calendar_add`'s existing tests constructs an
    /// `EventTime::Local`, so it is never actually called by them). Use
    /// [`IcsConnector::with_temp_dir_opener_and_converter`] to also inject
    /// a fake converter.
    #[allow(dead_code)]
    pub fn with_temp_dir_and_opener(temp_dir: PathBuf, opener: O) -> Self {
        Self {
            temp_dir,
            opener,
            local_time_converter: Box::new(Win32LocalTimeConverter),
        }
    }

    /// Same as [`IcsConnector::with_temp_dir_and_opener`], plus an
    /// explicit [`LocalTimeConverter`] -- what issue #211's tests use to
    /// prove the UTC-upgrade and floating-fallback paths without ever
    /// calling the real Win32 timezone API.
    #[allow(dead_code)]
    pub fn with_temp_dir_opener_and_converter(
        temp_dir: PathBuf,
        opener: O,
        local_time_converter: Box<dyn LocalTimeConverter>,
    ) -> Self {
        Self {
            temp_dir,
            opener,
            local_time_converter,
        }
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
        // #211: upgrade any `EventTime::Local` to `Utc` when the injected
        // converter can do so; `render_ics` renders whatever is left as a
        // `Local` at that point as a floating local time (never a bare
        // `TZID` with no `VTIMEZONE`) -- see `resolve_local_times` and
        // `format_event_time_property`'s doc comments.
        let resolved_event = resolve_local_times(event, self.local_time_converter.as_ref());
        let content = render_ics(&resolved_event, &uid, &dtstamp)?;

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

/// #276: RFC 5545 `DATE`/`DATE-TIME` values require exactly a 4-digit year
/// (§3.3.4/§3.3.5). `{:04}` is a minimum-width specifier, not a truncating
/// one, so a `CivilDate::year` that civil arithmetic (`add_days`/
/// `add_seconds`, both unguarded on purpose -- see `civil_time`'s doc
/// comment) has pushed past 9999 would otherwise silently widen to a 9-digit
/// year. Refused here as a named error instead of writing malformed content
/// (rule 7: every failure ends in a card, never a silent malformed write).
fn format_date(d: &CivilDate) -> Result<String> {
    if !(0..=9999).contains(&d.year) {
        return Err(anyhow!(
            "the event date {:04}-{:02}-{:02} is out of range for an ICS DATE value (year must be 0000-9999)",
            d.year,
            d.month,
            d.day
        ));
    }
    Ok(format!("{:04}{:02}{:02}", d.year, d.month, d.day))
}

fn format_datetime(dt: &CivilDateTime) -> Result<String> {
    Ok(format!(
        "{}T{:02}{:02}{:02}",
        format_date(&dt.date)?,
        dt.hour,
        dt.minute,
        dt.second
    ))
}

/// Formats one of `DTSTART`/`DTEND`/etc. with its value, per the connector
/// design doc's "DTSTART/DTEND" rule.
///
/// **Issue #211**: `EventTime::Local` no longer renders `;TZID=<tzid>:...`
/// with no accompanying `VTIMEZONE` component (RFC 5545 §3.6.5 says a
/// `VTIMEZONE` "MUST" be included for every `TZID` referenced, and this
/// connector never generates one). By the time `render_ics` runs, its
/// caller (`create_calendar_event`) has already tried to upgrade every
/// `Local` value to `Utc` via the injected `LocalTimeConverter`
/// (`resolve_local_times`); a `Local` value that reaches this function
/// unchanged is one that upgrade could not perform, so it is rendered as a
/// **floating** local time instead -- no `TZID` parameter, no trailing
/// `Z`, `tzid` dropped entirely. RFC 5545 §3.3.5 defines a floating
/// date-time as valid with no `VTIMEZONE` at all (the consuming calendar
/// app interprets it in whatever zone it is itself configured for), which
/// is the honest thing to emit when this connector cannot itself resolve
/// the zone -- unlike the old behaviour, it is never wrong about *which*
/// zone the time is in, only silent about naming one.
fn format_event_time_property(name: &str, t: &EventTime) -> Result<String> {
    Ok(match t {
        EventTime::AllDay(d) => format!("{name};VALUE=DATE:{}", format_date(d)?),
        EventTime::Utc(dt) => format!("{name}:{}Z", format_datetime(dt)?),
        EventTime::Local { at, .. } => format!("{name}:{}", format_datetime(at)?),
    })
}

/// Issue #211: upgrades every `EventTime::Local` in `event`'s `start`/`end`
/// to `EventTime::Utc` when `converter` can convert it (see
/// [`LocalTimeConverter`]'s doc comment for what "can" means); leaves it
/// as `Local` otherwise, which `format_event_time_property` then renders
/// as a floating local time. `AllDay`/`Utc` values pass through unchanged.
/// Pure given `converter` (a fake in every test below; the real
/// `Win32LocalTimeConverter` only in production), so this is what is
/// actually unit-tested, not the Win32 call itself.
fn resolve_local_times(event: &CalendarEvent, converter: &dyn LocalTimeConverter) -> CalendarEvent {
    CalendarEvent {
        title: event.title.clone(),
        start: resolve_one_time(&event.start, converter),
        end: event.end.as_ref().map(|t| resolve_one_time(t, converter)),
        location: event.location.clone(),
        description: event.description.clone(),
    }
}

fn resolve_one_time(t: &EventTime, converter: &dyn LocalTimeConverter) -> EventTime {
    match t {
        EventTime::Local { at, .. } => match converter.to_utc(at) {
            Some(utc) => EventTime::Utc(utc),
            None => t.clone(),
        },
        other => other.clone(),
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
fn render_ics(event: &CalendarEvent, uid: &str, dtstamp: &CivilDateTime) -> Result<String> {
    let end = event
        .end
        .clone()
        .unwrap_or_else(|| default_end(&event.start));
    // #249: RFC 5545's DTEND is exclusive for a VALUE=DATE (all-day) event,
    // so an explicit end that is the same day as start -- or, more broadly,
    // any day at or before start -- names a zero-or-negative-duration event
    // that no calendar app can render as the honestly-intended single-day
    // event. Treat it exactly like a missing end (default_end's own +1-day
    // rule) rather than writing it literally; a genuine multi-day end
    // (end > start) still renders unmodified.
    let end = match (&event.start, &end) {
        (EventTime::AllDay(s), EventTime::AllDay(e)) if e <= s => default_end(&event.start),
        _ => end,
    };

    let mut lines = vec![
        "BEGIN:VCALENDAR".to_string(),
        "VERSION:2.0".to_string(),
        "PRODID:-//Wingman//ics connector//EN".to_string(),
        "CALSCALE:GREGORIAN".to_string(),
        "BEGIN:VEVENT".to_string(),
        format!("UID:{uid}"),
        format!("DTSTAMP:{}Z", format_datetime(dtstamp)?),
        format_event_time_property("DTSTART", &event.start)?,
        format_event_time_property("DTEND", &end)?,
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
    Ok(out)
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
        let ics = render_ics(&sample_event(), "1234-0@wingman.local", &fixed_dtstamp()).unwrap();
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
        let ics = render_ics(&event, "9999-1@wingman.local", &fixed_dtstamp()).unwrap();
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
    fn all_day_event_with_an_explicit_same_day_end_still_gets_the_exclusive_next_day_dtend() {
        // #249: RFC 5545's DTEND is exclusive for a VALUE=DATE event, so an
        // explicit end equal to start must render exactly like the
        // missing-end default (start + 1 day), not literally.
        let event = CalendarEvent {
            title: "Conference".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 1,
            }),
            end: Some(EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 1,
            })),
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "9999-1@wingman.local", &fixed_dtstamp()).unwrap();
        assert!(
            ics.contains("DTEND;VALUE=DATE:20261002\r\n"),
            "same-day explicit end must render as start + 1 day: {ics}"
        );
    }

    #[test]
    fn all_day_event_with_an_end_before_start_also_gets_bumped_forward() {
        // Neighbouring case: an end that is literally before start is at
        // least as broken as an equal-day end, so it gets the same
        // missing-end-style treatment rather than being written verbatim.
        let event = CalendarEvent {
            title: "Oops".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 5,
            }),
            end: Some(EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 3,
            })),
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "9999-1@wingman.local", &fixed_dtstamp()).unwrap();
        assert!(
            ics.contains("DTEND;VALUE=DATE:20261006\r\n"),
            "an end before start must be treated like a missing end: {ics}"
        );
    }

    #[test]
    fn all_day_event_with_a_genuine_multi_day_end_is_left_untouched() {
        // Neighbouring case: a real multi-day all-day event must not be
        // altered by the same-day/before-start guard.
        let event = CalendarEvent {
            title: "Retreat".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 1,
            }),
            end: Some(EventTime::AllDay(CivilDate {
                year: 2026,
                month: 10,
                day: 4,
            })),
            location: None,
            description: None,
        };
        let ics = render_ics(&event, "9999-1@wingman.local", &fixed_dtstamp()).unwrap();
        assert!(
            ics.contains("DTSTART;VALUE=DATE:20261001\r\n"),
            "unaffected start: {ics}"
        );
        assert!(
            ics.contains("DTEND;VALUE=DATE:20261004\r\n"),
            "a genuine multi-day end must render literally, unmodified: {ics}"
        );
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
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp()).unwrap();
        assert!(ics.contains("DTSTART:20260920T140000Z\r\n"));
        assert!(ics.contains("DTEND:20260920T150000Z\r\n"));
    }

    fn sample_local_event() -> CalendarEvent {
        CalendarEvent {
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
        }
    }

    #[test]
    fn issue_211_local_event_time_renders_as_a_floating_time_not_a_bare_tzid() {
        // render_ics never sees a real Win32 conversion (it is pure) -- a
        // `Local` value passed straight to it is exactly the "could not
        // upgrade" case `resolve_local_times` leaves behind, and this is
        // the byte-exact floating-time rendering that must produce (#211:
        // never a `TZID` parameter with no accompanying `VTIMEZONE`).
        let event = sample_local_event();
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp()).unwrap();
        assert!(ics.contains("DTSTART:20260920T090000\r\n"));
        assert!(ics.contains("DTEND:20260920T100000\r\n"));
        assert!(
            !ics.contains("TZID"),
            "a floating time must never carry a TZID parameter: {ics}"
        );
    }

    // -- resolve_local_times / Win32-upgrade-or-floating-fallback (#211) ---

    struct FakeConverter {
        /// If `Some`, every call succeeds and returns this fixed UTC value
        /// (real conversion logic is Win32's job, not this fake's -- it
        /// only needs to prove which branch `resolve_local_times` took).
        result: Option<CivilDateTime>,
    }

    impl LocalTimeConverter for FakeConverter {
        fn to_utc(&self, _at: &CivilDateTime) -> Option<CivilDateTime> {
            self.result
        }
    }

    fn utc_noon() -> CivilDateTime {
        CivilDateTime {
            date: CivilDate {
                year: 2026,
                month: 9,
                day: 20,
            },
            hour: 12,
            minute: 0,
            second: 0,
        }
    }

    #[test]
    fn resolve_local_times_upgrades_local_to_utc_when_the_converter_succeeds() {
        let event = sample_local_event();
        let converter = FakeConverter {
            result: Some(utc_noon()),
        };
        let resolved = resolve_local_times(&event, &converter);
        assert_eq!(resolved.start, EventTime::Utc(utc_noon()));
    }

    #[test]
    fn resolve_local_times_leaves_local_unchanged_when_the_converter_fails() {
        let event = sample_local_event();
        let converter = FakeConverter { result: None };
        let resolved = resolve_local_times(&event, &converter);
        assert_eq!(resolved.start, event.start);
    }

    #[test]
    fn resolve_local_times_passes_all_day_and_utc_through_unchanged() {
        let converter = FakeConverter {
            result: Some(utc_noon()),
        };
        let all_day = CalendarEvent {
            title: "x".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 2026,
                month: 9,
                day: 20,
            }),
            end: None,
            location: None,
            description: None,
        };
        let resolved = resolve_local_times(&all_day, &converter);
        assert_eq!(resolved.start, all_day.start);

        let utc_event = sample_event(); // start/end are both EventTime::Utc
        let resolved = resolve_local_times(&utc_event, &converter);
        assert_eq!(resolved.start, utc_event.start);
        assert_eq!(resolved.end, utc_event.end);
    }

    #[test]
    fn resolve_local_times_upgrades_both_start_and_end_independently() {
        let mut event = sample_local_event();
        event.end = Some(EventTime::Local {
            at: CivilDateTime {
                date: CivilDate {
                    year: 2026,
                    month: 9,
                    day: 20,
                },
                hour: 10,
                minute: 0,
                second: 0,
            },
            tzid: "America/New_York".to_string(),
        });
        let converter = FakeConverter {
            result: Some(utc_noon()),
        };
        let resolved = resolve_local_times(&event, &converter);
        assert_eq!(resolved.start, EventTime::Utc(utc_noon()));
        assert_eq!(resolved.end, Some(EventTime::Utc(utc_noon())));
    }

    #[test]
    fn create_calendar_event_writes_utc_when_the_converter_succeeds() {
        let dir = test_temp_dir("local-upgrade");
        let connector = IcsConnector::with_temp_dir_opener_and_converter(
            dir.clone(),
            RecordingOpener::default(),
            Box::new(FakeConverter {
                result: Some(utc_noon()),
            }),
        );
        let result = connector
            .create_calendar_event(&sample_local_event())
            .unwrap();
        let written = std::fs::read_to_string(&result.path).unwrap();
        assert!(written.contains("DTSTART:20260920T120000Z\r\n"));
        assert!(!written.contains("TZID"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn create_calendar_event_writes_a_floating_time_when_the_converter_fails() {
        let dir = test_temp_dir("local-fallback");
        let connector = IcsConnector::with_temp_dir_opener_and_converter(
            dir.clone(),
            RecordingOpener::default(),
            Box::new(FakeConverter { result: None }),
        );
        let result = connector
            .create_calendar_event(&sample_local_event())
            .unwrap();
        let written = std::fs::read_to_string(&result.path).unwrap();
        assert!(written.contains("DTSTART:20260920T090000\r\n"));
        assert!(
            !written.contains("TZID"),
            "the RFC 5545 violation #211 reports must never be written: {written}"
        );
        std::fs::remove_dir_all(&dir).ok();
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

    // -- out-of-range years (#276) ------------------------------------------

    #[test]
    fn all_day_event_starting_on_year_9999_12_31_with_no_end_errors_instead_of_writing_a_9_digit_year(
    ) {
        // #276: default_end's +1-day rule on the very last representable
        // 4-digit-year date rolls the year over to 10000. format_date must
        // refuse this instead of emitting a malformed 9-digit DTEND.
        let event = CalendarEvent {
            title: "Boundary".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 9999,
                month: 12,
                day: 31,
            }),
            end: None,
            location: None,
            description: None,
        };
        let err = render_ics(&event, "u@wingman.local", &fixed_dtstamp())
            .expect_err("a year past 9999 must be a named error, not malformed output");
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn all_day_event_with_an_explicit_end_past_year_9999_also_errors() {
        let event = CalendarEvent {
            title: "Boundary".to_string(),
            start: EventTime::AllDay(CivilDate {
                year: 9999,
                month: 12,
                day: 30,
            }),
            end: Some(EventTime::AllDay(CivilDate {
                year: 10000,
                month: 1,
                day: 1,
            })),
            location: None,
            description: None,
        };
        assert!(render_ics(&event, "u@wingman.local", &fixed_dtstamp()).is_err());
    }

    #[test]
    fn a_normal_timed_event_still_renders_successfully_far_from_the_year_boundary() {
        // Neighbouring case: the new fallible signature must not regress an
        // ordinary in-range event.
        let ics = render_ics(&sample_event(), "1234-0@wingman.local", &fixed_dtstamp());
        assert!(ics.is_ok());
    }

    #[test]
    fn format_date_rejects_a_year_below_zero_and_at_or_above_10000() {
        assert!(format_date(&CivilDate {
            year: 9999,
            month: 12,
            day: 31
        })
        .is_ok());
        assert!(format_date(&CivilDate {
            year: 10000,
            month: 1,
            day: 1
        })
        .is_err());
        assert!(format_date(&CivilDate {
            year: -1,
            month: 1,
            day: 1
        })
        .is_err());
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
        let ics = render_ics(&event, "u@wingman.local", &fixed_dtstamp()).unwrap();
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
