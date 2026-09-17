# The `Connector` trait and the zero-auth `ics` connector

Status: **approved** (owner asleep; written against issues #35 and #34,
already scoped by the owner in the issue bodies, the 2026-09-16 expansion
plan §4 (`connectors/` row), §6 ("The first four actions", "Add to
calendar") and §10 ("Connectors"). Treated as pre-approved per the
overnight-agent instructions; the owner reviews on waking.)

Scope: the `Connector` trait, `AuthKind`, `Capability`, a name-based
registry, the domain `CalendarEvent`/`EventTime` types connectors speak, the
zero-auth `ics` connector (RFC 5545 `.ics` writer plus an injectable
`Opener`), and the `calendar_add` executor that drives it. Out of scope:
`google.rs`/`microsoft.rs` (OAuth PKCE, phase 3 per §10's table) and
`mcp.rs` (phase 5). Those get their own design section when their issue
lands; nothing here blocks that.

## Why a design doc (rule 12)

`connectors/` is a new module the executor design doc (2026-09-17) does not
mention -- that doc's scope is explicitly "`\"none\"` and `\"clipboard\"`",
both connector-free. The expansion plan pins the trait's *fields* ("id, auth
kind, capabilities") but not its *methods*: an object-safe `Box<dyn
Connector>` stored in a registry (mirroring `executors::registry`) needs a
concrete callable surface, not just descriptive metadata, or nothing could
ever call a connector. This is architectural per rule 12 for the same reason
the executor design doc gives: a new module whose shape nothing upstream has
pinned.

## `Connector` trait

`src/connectors/mod.rs`. Object-safe, stored as `Box<dyn Connector>` in the
registry, same pattern as `executors::Executor`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind { None, ApiKey, OAuthPkce }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability { CalendarWrite }

pub trait Connector: Send + Sync {
    fn id(&self) -> &'static str;
    fn auth_kind(&self) -> AuthKind;
    fn capabilities(&self) -> &'static [Capability];

    /// Default bail, not a required method: today only `CalendarWrite`
    /// exists and only `ics` implements it. A future capability (Gmail
    /// drafts, say) gets its own sibling method with the same default-bail
    /// shape, so a connector that only supports `CalendarWrite` need not
    /// stub out methods for capabilities it does not have -- the same
    /// "unimplemented arm is untestable dead code" reasoning
    /// `actions::schema::schema_for` already documents for proposal kinds.
    fn create_calendar_event(&self, _event: &CalendarEvent) -> anyhow::Result<CalendarEventResult> {
        anyhow::bail!("connector \"{}\" does not support calendar writes", self.id())
    }
}
```

A connector that calls a method for a capability it does not declare is a
caller bug the default bail turns into a named error (rule 7), not a panic;
`calendar_add` (below) is expected to check `capabilities()` before calling,
but the bail is the backstop if it does not.

## Registry

`src/connectors/registry.rs`, a `match` like `executors::registry::resolve`
(one entry today, `HashMap`/`once_cell` buys nothing):

```rust
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Connector>> {
    match name {
        "ics" => Ok(Box::new(IcsConnector::default())),
        _ => anyhow::bail!("No connector named \"{name}\". Check the action's connector setting."),
    }
}
```

`google`/`microsoft` add arms here when their issues land; nothing about
this shape changes.

## The domain type: `CalendarEvent` / `EventTime`

Lives in `connectors::mod` (not `executors::calendar_add`) because every
future calendar-capable connector (`google`, `microsoft`) consumes the same
shape -- the domain type belongs to the capability, not to one connector or
one executor.

```rust
pub struct CalendarEvent {
    pub title: String,
    pub start: EventTime,
    pub end: Option<EventTime>,   // None per the "missing end" rule below
    pub location: Option<String>,
    pub description: Option<String>,
}

pub struct CivilDate { pub year: i32, pub month: u8, pub day: u8 }
pub struct CivilDateTime { pub date: CivilDate, pub hour: u8, pub minute: u8, pub second: u8 }

pub enum EventTime {
    AllDay(CivilDate),                          // ICS VALUE=DATE
    Utc(CivilDateTime),                         // ICS ...Z
    Local { at: CivilDateTime, tzid: String },   // ICS ;TZID=...
}
```

`CivilDate`/`CivilDateTime` are hand-rolled (no `chrono`/`time` dependency:
the crate's dependency list is short by design and this task's scope is a
few date/time conversions, not general calendar math) using the
Howard Hinnant `days_from_civil`/`civil_from_days` algorithm (public domain,
the same algorithm `libc++`'s `<chrono>` and most from-scratch Gregorian
converters use) for the two things actually needed: today's UTC instant
(`DTSTAMP`) and adding a default duration to a start time (the "missing end"
rule). `src/connectors/civil_time.rs` carries the pure functions and their
own tests, independent of `ics.rs`.

### Timezone handling -- what is reachable today, and what is `THEORY (unverified)`

The landed `calendar_event` proposal schema (#26, `actions/schema.rs`) has
`start`/`end` as plain, format-less JSON-Schema strings -- no `tz` field
exists in the schema that ships today (the expansion plan §6 worked example
names a `tz` field in a comment, but the schema that actually landed with
#26 does not have one). `calendar_add`'s proposal parser (below) therefore
only ever produces `EventTime::AllDay` or `EventTime::Utc`, never `Local`:
there is no timezone name in the wire shape for it to read. `EventTime::Local`
exists in the type and is exercised directly by `ics.rs`'s own tests (a
connector-level capability, per the paragraph above, not dead code) so that
`google`/`microsoft` -- which *can* express a local time plus an IANA/Windows
zone name from their own APIs -- have a variant to construct without a type
change.

`THEORY (unverified)`: whether a provider reliably emits an ISO 8601
date-time string with an explicit UTC offset or `Z` suffix (which
`calendar_add`'s parser requires to build `EventTime::Utc`) versus a
floating local time with no offset is unmeasured -- no live provider call is
in this task's scope (build rules: no live API calls). The parser treats a
date-time string with no offset as a hard parse error (rule 7: an error
card, not a silent wrong-timezone guess) rather than assuming UTC or the
machine's local zone. If this proves too strict against a real model's
output, loosening it is a follow-up with its own measurement, not a
guess made here.

**Resolved (#211, 2026-09-17):** `ics.rs` no longer emits a bare
`DTSTART;TZID=...:` line at all, so the RFC 5545 §3.6.5 question above is
moot rather than answered -- the chosen fix avoids the violation instead of
adding a `VTIMEZONE` block. The robust option, in order:

1. **Upgrade to UTC via Win32**, the same `TzSpecificLocalTimeToSystemTime`
   call `app.rs`'s `deadline_until_tomorrow` already uses for Pause
   (`None` zone parameter = "the machine's own currently active zone",
   DST-correct). This is the only zone conversion Win32 offers without a
   separate IANA/Windows zone database this crate does not depend on (rule
   2's "dependency list is short by design"), so a `Local { at, tzid }`
   value is converted as if `at` were already a wall-clock time in the
   machine's own zone, regardless of what `tzid` names. `EventTime::Local`
   is still unreachable from the schema-driven `calendar_add` parser today
   (unchanged from the paragraph above), so this path is exercised only by
   `ics.rs`'s own tests and by a future zone-aware connector
   (`google`/`microsoft`) until one exists.
2. **Fall back to a floating local time** (RFC 5545 §3.3.5: no `TZID`
   parameter, no trailing `Z`, no `VTIMEZONE` needed) when the Win32
   conversion cannot be performed (an invalid/ambiguous wall-clock time
   during a DST transition, or any other Win32 failure). `tzid` is dropped
   silently in this case -- there is nothing else to do with a zone name
   this connector cannot resolve, and floating is still RFC-valid, unlike
   the old bare-`TZID` output.

Implementation: `ics::LocalTimeConverter` (an injectable trait, same
reasoning as `Opener` -- rule 9, tests never call the real Win32 API) plus
`Win32LocalTimeConverter` (production); `create_calendar_event` runs every
`Local` value in a `CalendarEvent` through `resolve_local_times` before
rendering, and `format_event_time_property`'s `Local` arm renders whatever
is left (i.e. a value the converter could not upgrade) as floating. Golden
tests updated in `connectors::ics::tests` (`issue_211_local_event_time_...`
and the `resolve_local_times_*`/`create_calendar_event_writes_*` tests) to
assert the new byte-exact output for both the upgrade and fallback paths,
with a scripted `FakeConverter` -- no real Win32 call and no real file
handler in any test.

The manual check in issue #166 (open a real generated `.ics` in the
default calendar app) still cannot run from this task (no exe launch); it
now verifies the UTC/floating output reads correctly, not the removed
bare-`TZID` behaviour.

## The `ics` connector

`src/connectors/ics.rs`. Builds an RFC 5545 `VCALENDAR`/`VEVENT`, writes it
to a temp file, hands it to the default calendar handler:

```rust
pub trait Opener: Send + Sync {
    fn open(&self, path: &std::path::Path) -> anyhow::Result<()>;
}
struct ShellOpener;   // ShellExecuteW("open", path, ...) behind cfg(windows)

pub struct IcsConnector<O: Opener = ShellOpener> {
    temp_dir: std::path::PathBuf,   // injectable: production is %TEMP%\Wingman, tests use their own tempdir (rule 9)
    opener: O,
}
```

Same injectable-dependency shape `ClipboardExecutor<C: ClipboardAccess>`
already uses: a real default type parameter for production, `with_opener`/an
explicit constructor for tests, so no test ever calls `ShellExecuteW` or
writes into the real `%TEMP%\Wingman`.

`.ics` generation rules (RFC 5545, §3.1 "Content Lines" for folding, §3.3.11
for the `TEXT` escaping):

- **CRLF** line endings throughout, including the final line -- RFC 5545
  requires it regardless of the platform building the file.
- **Line folding** at 75 octets: any content line whose UTF-8 byte length
  (not char count -- folding is defined in octets) exceeds 75 is broken
  after the 75th octet, followed by CRLF and a single leading space, with no
  fold splitting a multi-byte UTF-8 sequence (fold at the nearest earlier
  character boundary instead of exactly 75 when the 75th octet falls inside
  one).
- **Escaping** (`SUMMARY`/`LOCATION`/`DESCRIPTION`, RFC 5545 §3.3.11):
  backslash to `\\`, comma to `\,`, semicolon to `\;`, and a newline to the
  literal two-character sequence `\n` -- applied before folding, since
  folding operates on the already-escaped octet stream.
- **UID**: `<millis-since-epoch>-<per-process counter>@wingman.local`, built
  from `SystemTime::now()` and an `AtomicU64`, not a `uuid` dependency --
  uniqueness only needs to hold within one machine's generated files, which
  a monotonic counter plus wall-clock milliseconds already gives.
- **DTSTAMP**: `SystemTime::now()` converted to UTC civil time via
  `civil_time`, formatted `YYYYMMDDTHHMMSSZ`.
- **DTSTART/DTEND**: `EventTime::AllDay` writes `;VALUE=DATE:YYYYMMDD` (no
  time component, no `Z`); `EventTime::Utc` writes `:YYYYMMDDTHHMMSSZ`;
  `EventTime::Local` writes `;TZID=<tzid>:YYYYMMDDTHHMMSS` (see the
  `VTIMEZONE` gap, #211, above).
- **Missing end** (`CalendarEvent.end: None`): documented default duration,
  not an omitted `DTEND` (an event with no end is a worse proposal-review
  experience than a documented guess the user can edit before confirming --
  but see `calendar_add` below, which is also where the *editable* preview
  actually lives). All-day default: one day (`DTEND` = start + 1 day,
  RFC 5545's own convention for a single all-day event, exclusive end
  date). Timed default: one hour (`DTEND` = start + 1 hour, matching most
  calendar UIs' own default when a user creates an event with only a start
  time).

## `calendar_add` executor

`src/executors/calendar_add.rs`. `Effect::Writes` (the executor design doc's
existing `Effect` enum has exactly `ReadOnly`/`Writes` -- the task brief's
"`Effect::External`" names a variant that does not exist in the already
*approved* executor design; `Writes` already means "not read-only, always
requires the preview confirmation" per that doc's own `Effect` doc comment,
so adding a third variant for the same distinction would fork one boolean
into two enums for no new information. Using the existing variant is the
minimal-divergence reading of rule 12: the executor design doc is
authoritative for what `Effect` already covers).

`Executor::execute` is fixed by the approved executor design as
`fn execute(&self, confirmed: Confirmed<serde_json::Value>) -> Result<Undo>`
(object-safe, one common JSON currency across every executor). The task
brief's "consumes `Confirmed<CalendarEvent>` only" is satisfied by
construction rather than by widening that signature: `execute` immediately
calls a private `parse_calendar_event(&Value) -> Result<CalendarEvent>` on
the confirmed value and never touches the JSON again -- there is no code
path in this executor that acts on an unconfirmed `Proposal`, and no code
path that acts on the raw `Value` past the first line of `execute`. Keeping
`Connector::create_calendar_event` and the parser both typed on
`CalendarEvent` (not `Value`) is what actually gives the task brief's
guarantee: a bug in `execute` cannot accidentally hand a connector raw,
unvalidated JSON.

`parse_calendar_event` reads the five `calendar_event` schema fields
(`title`, `start`, `end`, `location`, `notes` -- `notes` maps to
`CalendarEvent.description`) and calls a shared `parse_event_time(&str) ->
Result<EventTime>` (in `connectors::ics`, reused by both `start` and `end`)
that accepts `YYYY-MM-DD` (-> `AllDay`) or `YYYY-MM-DDTHH:MM:SS(Z|+HH:MM|
-HH:MM)` (-> `Utc`, offset converted) and errors otherwise (see the timezone
`THEORY` above). An empty `end` string is `None` (missing end, the
documented default duration applies); a present-but-unparseable `end` is a
hard error, same as an unparseable `start` -- a proposal claiming an end
time and getting the format wrong is a bug to surface, not silently drop.

`execute` resolves the connector by name via `connectors::registry::resolve`
(today the executor is not itself configurable per-instance; a fixed
`"ics"` name until settings gains a "calendar connector" choice, the same
forward-wiring status `ClipboardExecutor` has for `text_answer`), calls
`create_calendar_event`, and wraps the result in an honest `Undo`:

```rust
Undo::recording(
    format!("added \"{title}\" to your calendar via {}", result.path.display()),
    move || {
        std::fs::remove_file(&result.path).context("could not remove the generated .ics file")
        // Deliberately does NOT attempt to remove the event from the
        // calendar app: Wingman only ever opened the .ics file with the
        // default handler (rule: "never the final button" -- Wingman does
        // not import/save on the user's behalf either); it has no API
        // handle to the event the calendar app created from it, so there
        // is nothing to call. The summary names this so `Undo::undo`'s
        // caller does not believe more happened than actually can.
    },
)
```

`Undo.summary` after `undo()` runs is expected to additionally surface
"remove the event in your calendar app yourself" as a manual step -- modeled
by `execute`'s returned `Undo.summary` naming the file location up front
(so the confirm/result card's undo affordance can say what it does and does
not do), rather than inventing a second field on `Undo` for one executor;
`Undo`'s shape is shared crate-wide and already shipped (executor design
doc) with exactly `summary` plus the restore closure.

Registered in `executors::registry::resolve`: `"calendar_add" =>
Ok(Box::new(CalendarAddExecutor::new()))`.

## Verification note

`civil_time`'s date arithmetic (`days_from_civil`/`civil_from_days`, adding
a duration) is proven by its own unit tests, including the well-known
round-trip property (every day in a multi-century range converts to a day
count and back to the same date) rather than a handful of spot values,
because a hand-rolled calendar algorithm is exactly the kind of code a
narrow example set gives false confidence about. `ics.rs`'s folding and
escaping are proven against literal expected byte strings (not just
"contains" checks), since an off-by-one in a 75-octet fold is invisible to
a substring assertion. Everything else is proven by the tests in
`src/connectors/*.rs` and `src/executors/calendar_add.rs` listed in the
commit this design doc ships with.
