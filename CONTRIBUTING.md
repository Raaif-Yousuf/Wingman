# Contributing to Wingman

Wingman is meant to grow as a community project: the loop (Look, Propose,
Confirm, Do) and the executor set are the framework, and most contributions
are expected to be **actions**, a small, reviewable amount of data and prompt
text that reuses an existing executor. This document covers both that
future path and how to contribute to the app as it exists today.

## Where the project actually is

Read this before writing code: today's app is one hotkey, one screenshot,
one cloud model call, one read-only card (`README.md` has the details). The
**actions framework described below in "Add an action in 20 minutes" does
not exist in the code yet.** It is the target shape from the [expansion
plan](docs/superpowers/specs/2026-09-16-expansion-plan-design.md) §6, written
here so the format is settled before anyone builds the framework or the
first catalogue action against it. If you want to contribute to the actions
framework itself (`actions/`, the proposal schema registry, the intent
router, the palette), that is Phase 2 work; open an issue or comment on an
existing one before starting, per the spec-first rule below.

Contributions that work against the app as it exists today (the hotkey
path, capture, the settings window, the card, the tray, provider code) are
welcome right now and do not need any of the actions-framework scaffolding.

## Prerequisites

- Rust 1.80 or newer (`rust-version` in `Cargo.toml`), stable toolchain.
- Windows 11. This is a Win32 application; it will not build meaningfully on
  another OS, though `cargo check` may get partway on one.
- An OpenAI or Anthropic API key to exercise the running app end to end.
  Not required to build or run the unit tests.
- Optional: [Ollama](https://ollama.com) running locally, only relevant once
  local-model support (Phase 1 of the plan) exists; nothing in the current
  codebase talks to it yet.

## Before you write code: spec-first for anything architectural

CLAUDE.md rule 12: a change that adds a module, changes a thread's
responsibilities, changes a stored file's format, or otherwise affects
architecture gets a short design spec in
`docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md`, written and settled
before the implementation plan. A new action that only adds a TOML entry and
reuses an existing executor does not need one; a new executor, connector or
provider does.

## Build, test, lint

```powershell
cargo build
cargo test
cargo clippy -D warnings
```

Run the tests you touched while iterating (`cargo test config::tests::` for
everything in `src/config.rs`'s test module, for example); run the full
`cargo test` before opening a pull request. Every module that has logic
worth testing keeps its tests in a `#[cfg(test)] mod tests { ... }` block in
the same file (see `src/config.rs`, `src/capture.rs`, `src/autostart.rs`),
which is the pattern to follow for new pure-logic code.

`cargo clippy -D warnings` must be clean. Once `deny.toml` exists
(tracked separately), `cargo deny check` joins this list; until then, keep
new dependencies permissively licensed by hand (MIT, Apache-2.0, BSD, ISC,
Zlib, Unlicense: CLAUDE.md rule 2) and add anything new to
`THIRD_PARTY_NOTICES.md` in the same pull request.

Win32 code (anything touching a window, a hook, the tray, GDI) is not
meaningfully unit-testable. CLAUDE.md rule 8 applies: state the one
observable that would differ if the change were wired to nothing, and check
it by hand before calling the change done. The `wired-to-nothing` skill in
this repo exists for exactly this.

## Commit sign-off (DCO)

Every commit needs a Developer Certificate of Origin sign-off, certifying
you wrote it or otherwise have the right to submit it under the project's
MIT license:

```powershell
git commit -s -m "your message"
```

That appends a `Signed-off-by: Your Name <you@example.com>` trailer using
your configured `git config user.name` / `user.email`. Pull requests without
it will be asked to amend (`git commit --amend -s` for the last commit, or
`git rebase --exec 'git commit --amend --no-edit -s' -i <base>` for several).

## Add an action in 20 minutes (target format, Phase 2)

This walks through adding **Translate selection**, the catalogue action the
expansion plan names as the worked example (§6), because it needs no new
executor: it reads the current text selection and hands back translated text
on a read-only card. Every step below describes the framework as designed;
none of it can be run yet.

### 1. Add the action to `actions.toml`

```toml
[[actions]]
id       = "translate-selection"
name     = "Translate selection"
inputs   = ["selection", "screen"]      # falls back to a screen region if nothing is selected
proposal = "text_answer"                # built-in schema: { headline, detail }
executor = "none"                       # read-only: no executor, no confirm step
confirm  = false
prompt   = """
Translate the selected text to the user's preferred language (see the
`preferred_language` setting; default English). Return the translation as
`detail` and a one-line "Translated from <language>" as `headline`.
"""
prefer   = { mode = "auto" }
```

That is the entire contribution for a read-only action that reuses an
existing proposal schema and needs no executor: one TOML block. It is
reviewable in minutes because it cannot do anything beyond what `text_answer`
and "no executor" already allow.

### 2. If the action needs a new proposal shape

Only needed when no existing schema fits (`text_answer`, `verdict`,
`calendar_event`, `form_fill`, `text_review` cover most read-and-propose
cases). Add it to the proposal schema registry in `actions/` with its
`serde` struct, in **property-declaration order**, because that order is
sent as the JSON schema and the model answers in that order (CLAUDE.md rule
3: this is why `serde_json` keeps `preserve_order`). Put any field the model
should commit to last (like a verdict) after the fields that justify it
(like the reasoning), the same way the existing `verdict` schema puts
`headline` after `detail`.

### 3. If the action needs a new executor

Only needed when the action writes something (fills a form, sends a
calendar event, replaces text). An executor is one file in `executors/`
that:

- takes a `Confirmed<Proposal>` (the type system enforces that it cannot run
  on an unconfirmed proposal),
- does exactly what the confirmed preview showed, nothing re-generated or
  re-interpreted at execution time,
- returns an `Outcome` describing what it actually did (not what it
  intended), with an undo closure where the platform allows one,
- never presses Send, Submit, Buy or Pay, and never touches payment data.

These five rules (plan §6, "The rules every executor obeys") are non-
negotiable for anything merged as an executor; a pull request that adds an
executor is reviewed against them explicitly.

### 4. If the action needs a new connector

Only for actions that talk to an external service (a calendar, an email
provider). Implement the `Connector` trait in `connectors/`: an id, an auth
kind (OAuth PKCE loopback, API key, or none), and its capabilities. Tokens go
through the secrets module into Windows Credential Manager, never into
`config.toml` and never logged.

### 5. Test it

- A unit test for any new pure logic (schema construction, prompt
  templating, executor logic that does not touch Win32).
- A manual check for anything Win32: state the observable (per rule 8),
  trigger the action for real, confirm the card shows what is expected.
- If the action is read-only and needs no executor, that manual check is
  usually: press the hotkey with the action selected, confirm the card shows
  a translated result, done.

### 6. Open the pull request

Reference the catalogue entry (or open one) from the plan's §6 "Action
catalogue" if it is not already an issue. Use the `good first issue` label
for actions like this one, that need no new executor.

## Adding something outside the actions framework

Bug fixes, performance work, and changes to the parts of the app that exist
today (capture, hotkeys, the card, settings, provider code) do not need any
of the above. Open an issue first if the change is architectural (rule 12);
otherwise, a focused pull request with tests for the logic it touches is
enough. Search existing issues before filing a new one; this repo's
`filing-findings` skill has the format used for bug reports if you are
filing one.

## Code of Conduct

Participation in this project is governed by
[`CODE_OF_CONDUCT.md`](CODE_OF_CONDUCT.md).
