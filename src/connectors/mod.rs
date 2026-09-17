//! Connectors (#35): the boundary between a deterministic executor and an
//! external calendar/mail/etc. service. See
//! `docs/superpowers/specs/2026-09-17-connector-design.md` for the type
//! system this module implements and why (the executor design doc does not
//! cover connectors; this is the short rule-12 addendum for them).
//!
//! `Connector` is object-safe (stored as `Box<dyn Connector>` in
//! [`registry::resolve`]), the same shape `executors::Executor` uses.

pub mod civil_time;
mod ics;
pub mod registry;

// `Opener` is only named by `executors::calendar_add`'s tests today (to
// construct an `IcsConnector` wired to a fake opener) -- the same
// forward-wiring status the rest of this module has until `app.rs`'s
// worker calls a resolved executor for real (executor design doc's "Out of
// scope").
#[allow(unused_imports)]
pub use ics::{parse_event_time, IcsConnector, Opener};

use civil_time::{CivilDate, CivilDateTime};

/// How a connector authenticates. `None` (the `ics` connector: no account,
/// no token, writes a local file) is what #35 needs today; `ApiKey` and
/// `OAuthPkce` are named now so `google`/`microsoft` (phase 3, expansion
/// plan §10) do not need an enum change to land.
// `ApiKey`/`OAuthPkce` are unused outside their own construction until a
// connector that needs them exists -- the same forward-wiring status
// `executors::Effect::Writes` had before `calendar_add` used it.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    None,
    ApiKey,
    OAuthPkce,
}

/// What a connector can do. One variant today (`CalendarWrite`); a future
/// capability (Gmail drafts, say) adds a sibling variant plus a sibling
/// default-bail method on [`Connector`], not a change to this one.
// Unused outside tests and `ics::IcsConnector::capabilities` until
// `calendar_add` checks `capabilities()` before calling from a live code
// path -- see the module doc comment's forward-wiring status.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    CalendarWrite,
}

/// A calendar event in the shape every calendar-capable connector consumes
/// (`ics` today; `google`/`microsoft` later, expansion plan §10). Lives here
/// rather than in `executors::calendar_add`: the domain type belongs to the
/// capability, not to one connector or one executor, so a future
/// `google`/`microsoft` connector has a type to implement
/// [`Connector::create_calendar_event`] against without inventing its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub title: String,
    pub start: EventTime,
    /// `None` per the "missing end" rule (connector design doc): the
    /// connector applies a documented default duration rather than writing
    /// no end at all.
    pub end: Option<EventTime>,
    pub location: Option<String>,
    pub description: Option<String>,
}

/// A point in time as a calendar-event boundary can express it. See the
/// connector design doc's "Timezone handling" section for which variants
/// `calendar_add`'s proposal parser can actually produce today (`AllDay`
/// and `Utc` only -- the landed `calendar_event` schema has no timezone
/// field) versus which exist for a future connector to construct directly
/// (`Local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventTime {
    /// A whole-day event with no time component (ICS `VALUE=DATE`).
    AllDay(CivilDate),
    /// An instant expressed in UTC (ICS `...Z`).
    Utc(CivilDateTime),
    /// An instant expressed as a local wall-clock time plus an IANA/Windows
    /// zone name (ICS `;TZID=...`). Not reachable from the schema-driven
    /// `calendar_add` parser today (see the connector design doc); exists
    /// for a connector that can express one directly, and is exercised by
    /// `ics.rs`'s own tests.
    #[allow(dead_code)]
    Local { at: CivilDateTime, tzid: String },
}

/// What creating a calendar event actually did, returned to the caller so
/// the confirm/result card can say what happened (expansion plan §6, "Say
/// what happened, not what was intended") instead of assuming success from
/// a bare `Ok(())`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEventResult {
    /// Where the connector wrote the event before handing it to the OS
    /// (`ics`: the generated `.ics` file). `calendar_add`'s [`crate::executors::Undo`]
    /// removes this path; it is not the calendar app's own event id, which
    /// no zero-auth connector can obtain (nothing reports one back).
    pub path: std::path::PathBuf,
    /// Whether the connector successfully handed the file to a handler
    /// (`ics`: `ShellExecuteW("open", ...)` succeeded). `false` still means
    /// the file was written -- the confirm/result card can name the path
    /// for the user to open by hand.
    pub opened: bool,
}

/// Deterministic Rust that speaks to one external calendar/mail/etc.
/// surface on a connector-typed domain value. Never called on unvalidated
/// JSON: every method here takes the connector-level domain type
/// ([`CalendarEvent`] today), never `serde_json::Value` -- the raw-to-typed
/// conversion happens once, in the calling executor
/// (`executors::calendar_add::parse_calendar_event`), not here.
// `auth_kind`/`capabilities` are unused outside tests until a caller reads
// them from a live code path (the same forward-wiring status
// `executors::Executor`'s own methods have per that module's doc comment).
#[allow(dead_code)]
pub trait Connector: Send + Sync {
    fn id(&self) -> &'static str;
    fn auth_kind(&self) -> AuthKind;
    fn capabilities(&self) -> &'static [Capability];

    /// Default bail (rule 7: a named error, not a panic) for a connector
    /// that does not declare [`Capability::CalendarWrite`]. `calendar_add`
    /// checks `capabilities()` before calling this; the bail is the
    /// backstop, not the primary guard.
    fn create_calendar_event(&self, _event: &CalendarEvent) -> anyhow::Result<CalendarEventResult> {
        anyhow::bail!(
            "connector \"{}\" does not support calendar writes",
            self.id()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal connector that declares no capabilities, to prove the
    // default-bail backstop actually bails rather than silently succeeding
    // (the `wired-to-nothing` shape: a default method with no body would be
    // a silent no-op that returns `Ok`).
    struct NoCapabilityConnector;
    impl Connector for NoCapabilityConnector {
        fn id(&self) -> &'static str {
            "no-capability"
        }
        fn auth_kind(&self) -> AuthKind {
            AuthKind::None
        }
        fn capabilities(&self) -> &'static [Capability] {
            &[]
        }
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
            end: None,
            location: None,
            description: None,
        }
    }

    #[test]
    fn a_connector_with_no_calendar_capability_errors_instead_of_silently_succeeding() {
        let err = NoCapabilityConnector
            .create_calendar_event(&sample_event())
            .expect_err("a connector with no CalendarWrite capability must error, not succeed");
        let msg = err.to_string();
        assert!(
            msg.contains("no-capability"),
            "error should name the connector: {msg}"
        );
        assert!(
            !msg.contains('\u{2014}'),
            "no em dashes in card-facing text (rule 11): {msg}"
        );
    }
}
