# Connectors

A connector is deterministic Rust that speaks to one external calendar/
mail/etc. surface on a typed domain value (`CalendarEvent` today). It is
what an executor calls to actually reach outside the machine, the same way
an executor is what a confirmed proposal calls to actually reach outside
the model. `ics` (issue #35, zero external account) is the only connector
that exists today; `google` and `microsoft` are planned (phase 3) but not
written.

Checked against the code, not the spec: `src/connectors/mod.rs` (the
`Connector` trait, `AuthKind`, `Capability`, `CalendarEvent`/`EventTime`),
`src/connectors/registry.rs`, `src/connectors/ics.rs`, and the one caller,
`src/executors/calendar_add.rs`.

## The connector contract

```rust
pub trait Connector: Send + Sync {
    fn id(&self) -> &'static str;
    fn auth_kind(&self) -> AuthKind;
    fn capabilities(&self) -> &'static [Capability];

    fn create_calendar_event(&self, _event: &CalendarEvent) -> anyhow::Result<CalendarEventResult> {
        anyhow::bail!("connector \"{}\" does not support calendar writes", self.id())
    }
}
```

- **`id`** is the name `src/connectors/registry.rs::resolve` and error/card
  text both use (today only `"ics"`).
- **`auth_kind`** reports how the connector authenticates. See `AuthKind`
  below for what exists and what is only planned.
- **`capabilities`** is the list of things this connector can do. Today
  there is exactly one, `Capability::CalendarWrite`; a connector that does
  not declare it gets `create_calendar_event`'s default body: a named
  `anyhow` error (rule 7: a card, never a panic), not a silent no-op. A
  caller (`calendar_add`) is expected to check `capabilities()` before
  calling; the default bail is the backstop if it does not.
- A capability method (`create_calendar_event` today) never takes raw
  `serde_json::Value`. The executor parses the confirmed JSON into the
  connector-level domain type once, on its own side of the boundary
  (`calendar_add::parse_calendar_event`), and a connector only ever sees the
  typed value. A future capability (Gmail drafts, say) adds a sibling
  variant to `Capability` plus a sibling default-bail method here, not a
  change to this one.

`Connector` is object-safe, stored as `Box<dyn Connector>` in the registry,
the same shape `executors::Executor` uses.

## `AuthKind`: what is implemented, what is not

```rust
pub enum AuthKind {
    None,
    ApiKey,
    OAuthPkce,
}
```

- **`None`** is implemented and in use: the `ics` connector has no account
  and no token, it writes a local file.
- **`ApiKey`** and **`OAuthPkce`** are named in the enum today so that
  `google`/`microsoft` do not need an `AuthKind` change to land later, but
  **no connector implements either one yet**. There is no API-key-auth
  connector in this tree, and the OAuth PKCE loopback flow described below
  is a design, not code: no `src/connectors/google.rs`, no
  `src/connectors/microsoft.rs`, no loopback HTTP listener exists.
  Issue #53 ("OAuth PKCE loopback flow shared by Google and Microsoft")
  tracks writing it; see the 2026-09-16 expansion plan's §10
  ("Connectors") table and §12 ("Secrets and config") in
  `docs/superpowers/specs/2026-09-16-expansion-plan-design.md` for the
  planned shape: a loopback redirect on `127.0.0.1:<random port>`, PKCE, the
  refresh token in Windows Credential Manager (via `src/secrets.rs`, which
  today only stores provider API keys, not connector tokens), silent
  refresh, and revoke. Do not write code against `AuthKind::ApiKey` or
  `AuthKind::OAuthPkce` expecting an existing pattern to copy; there isn't
  one yet.

## Capability

```rust
pub enum Capability {
    CalendarWrite,
}
```

One variant today. `ics` declares it; nothing else exists to declare
anything else.

## The domain type: `CalendarEvent` / `EventTime`

Lives in `connectors::mod`, not in the `ics` connector or the
`calendar_add` executor, because every future calendar-capable connector
(`google`, `microsoft`) consumes the same shape:

```rust
pub struct CalendarEvent {
    pub title: String,
    pub start: EventTime,
    pub end: Option<EventTime>,   // None: the connector applies a documented default duration
    pub location: Option<String>,
    pub description: Option<String>,
}

pub enum EventTime {
    AllDay(CivilDate),                          // ICS VALUE=DATE
    Utc(CivilDateTime),                         // ICS ...Z
    Local { at: CivilDateTime, tzid: String },   // a connector that can express a local time directly
}
```

`CivilDate`/`CivilDateTime` and the pure Gregorian arithmetic they need
(`days_from_civil`/`civil_from_days`, adding days/seconds) live in
`src/connectors/civil_time.rs`, with their own tests, independent of
`ics.rs`. No date/time crate dependency: this needs only "what UTC instant
is it" and "add N seconds/days to a civil date", not general calendar math
(rule 2: short dependency list by design).

`calendar_add`'s proposal parser can only ever produce `EventTime::AllDay`
or `EventTime::Utc` today: the landed `calendar_event` schema has no
timezone field for `EventTime::Local` to come from. `Local` exists for a
connector that can obtain a local time plus an IANA/Windows zone name
directly from its own API (`google`/`microsoft`, once they exist) and is
exercised today only by `ics.rs`'s own tests.

## Resolving a connector by name

```rust
// src/connectors/registry.rs
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Connector>> {
    match name {
        "ics" => Ok(Box::new(IcsConnector::default())),
        _ => anyhow::bail!("No connector named \"{name}\". Check the action's connector setting."),
    }
}
```

A plain `match`, same reasoning as `executors::registry::resolve`: with one
entry, a `HashMap`/`once_cell` lookup table buys nothing a `match` doesn't
already give for free. An unknown name is a named `anyhow` error, never a
panic. `google`/`microsoft` add arms here when their issues land.

## The `ics` connector, as a worked example

`src/connectors/ics.rs`. Builds an RFC 5545 `VCALENDAR`/`VEVENT`, writes it
to `%TEMP%\Wingman\<uid>.ics`, and hands the path to the OS default handler
(`ShellExecuteW("open", ...)`) behind an injectable `Opener` trait so no
test ever launches a real handler (rule 9). `temp_dir` is also injectable
(production: `%TEMP%\Wingman`; tests: their own tempdir), and
`LocalTimeConverter` is injectable the same way for the Win32 timezone
conversion `EventTime::Local` needs (see the file's own doc comments for
issue #211's upgrade-or-floating-fallback rule).

`create_calendar_event` writes the file and reports what actually happened
in `CalendarEventResult { path, opened }`: `path` is where the connector
wrote the event (what `calendar_add`'s `Undo` deletes); `opened` is whether
handing it to the shell succeeded. A failed open still leaves the written
file behind and is not propagated as an error: the write, which is the part
this connector fully controls, already succeeded (see `create_calendar_event`'s
own doc comment).

`ics.rs` also best-effort cleans up its own old output: every call to
`create_calendar_event` sweeps `.ics` files it previously wrote in the same
temp directory that are older than a short retention window, before writing
the new one. See `cleanup_stale_ics_files` and `MAX_ICS_FILE_AGE` in that
file for the exact rule; cleanup failures are always ignored; they never
fail the calendar-add action (issue #238).

## How to add a connector

1. **Decide the capability.** If it is calendar writes, implement
   `Connector::create_calendar_event` against the existing `CalendarEvent`/
   `EventTime` types; do not invent a parallel event shape. A genuinely new
   capability (Gmail drafts, say) adds a new `Capability` variant and a new
   sibling method on the trait with the same default-bail shape, in a
   design spec first (rule 12: a new module/trait method is architectural).
2. **Pick the real `AuthKind`.** `None` if there is no account (copy
   `ics.rs`'s shape: an injectable side effect trait like `Opener`, an
   injectable temp/working directory, tests that never touch a real OS
   resource). `ApiKey`/`OAuthPkce` have no implementation to copy yet; see
   "`AuthKind`: what is implemented, what is not" above and issue #53
   before starting one.
3. **Never let a token or key reach `config.toml` or a log.** Provider API
   keys already go through `src/secrets.rs` into Windows Credential Manager;
   a connector's OAuth refresh token is planned to do the same (expansion
   plan §12), under its own `Wingman/<connector>` credential name.
4. **Respect Offline mode.** See "Offline mode and connectors" below before
   this connector's `create_calendar_event` (or equivalent) ever opens a
   socket.
5. **Register it.** Add a `match` arm in `src/connectors/registry.rs::resolve`,
   with a test proving the name resolves and a test proving an unknown name
   still errors by name with no em dash (follow the existing tests in that
   file).
6. **Test the pure logic; check Win32/network by hand.** Anything that
   talks to a real OS resource (the shell, a socket, Credential Manager)
   goes behind an injectable trait the same way `Opener` and
   `LocalTimeConverter` do, so the connector's own logic is unit-tested
   against a fake; the real integration is a named manual check (rule 8),
   not a test that secretly does nothing.

See `CONTRIBUTING.md`'s "If the action needs a new connector" for the
contributor-facing summary of this list.

## Offline mode and connectors

Offline mode (`src/mode.rs`, `Mode::Offline`) is stricter for connectors
than for providers: a provider is merely restricted to loopback hosts while
Offline (the `offline_guard` in `src/provider/common.rs` refuses any
non-loopback socket before it opens), but the Modes table calls for
connectors to be **disabled outright** in Offline mode, not just
loopback-restricted. `ics` needs no network at all, so this does not apply
to it yet, but it is the rule any future networked connector (`google`,
`microsoft`) must follow.

This is written down today in two places that do not yet have code to
enforce, because nothing networked exists to enforce it on:

- `src/mode.rs`'s "CONNECTORS HOOK (not built yet)" doc comment: when
  `connectors/*.rs` gains a networked connector, it must call
  `provider::common`-style guarded transport, or, if its HTTP needs diverge
  enough to need its own send path, call `mode::is_offline_now` and
  `mode::classify_host` itself before opening a socket.
- `src/provider/common.rs`'s matching "CONNECTORS HOOK" comment next to
  `offline_guard`: the same obligation, stated at the one file every
  provider request already funnels through.

In short: a connector that ever makes a network request is not allowed to
reach the network at all while Offline mode is active, full stop, not
merely restricted to `127.0.0.1`. `docs/offline.md`'s "What it cannot
guarantee" section says the same thing from the offline-guard side.

## See also

- [`docs/executors.md`](executors.md): the other half of "Do" -- an
  executor is what calls a connector, after a proposal is confirmed.
- [2026-09-17 connector design](superpowers/specs/2026-09-17-connector-design.md):
  the full rationale for the trait shape, the `.ics` generation rules
  (folding, escaping, the missing-end default), and the timezone-handling
  decisions `ics.rs` implements.
- [2026-09-16 expansion plan](superpowers/specs/2026-09-16-expansion-plan-design.md)
  §10 ("Connectors") and §12 ("Secrets and config"): the planned
  `google`/`microsoft` connectors, their auth, and where their tokens go.
- [`docs/offline.md`](offline.md): the four modes exactly as `mode.rs`
  implements them, and what the Offline guard does and does not cover yet.
- Issue #53: the OAuth PKCE loopback flow shared by Google and Microsoft,
  not started.
