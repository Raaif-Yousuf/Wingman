---
name: auditing-a-module
description: Use when auditing a module, subsystem or file set in this repo for defects worth filing, and when dispatched as a read-only auditor in a fan-out. Covers what counts as a finding, the Win32/Rust defect classes that actually recur here, the dedupe gate, and the batching rule that keeps low-value noise out of the tracker.
---

# Auditing a module

A read-only auditor is the highest-value agent shape this repo has. MEASURED
2026-09-18: five auditors produced 28 findings, including every P1 found that
night, against three merged issues from ten build agents in the same window.
They cost no RAM and touch no tree. Stay in that shape.

**Read [`filing-findings`](../filing-findings/SKILL.md) first.** It owns the
issue format, the label taxonomy, the dedupe command and the cargo RAM rules.
This skill owns *what to look for* and *what is worth filing*.

## Rules of the shape

1. **Never edit a source file.** Not a typo, not a doc comment, not a format
   nit. You file; someone else fixes. The one exception is your own scratch
   notes under the scratchpad directory.
2. **Never run bare `cargo test`.** `scripts/hooks/block_unfiltered_cargo_test.py`
   denies it. Run `cargo test <filter>` and only to confirm or refute one
   specific suspicion. Reading is cheaper than building: a build you did not
   need costs the orchestrator's whole machine for a minute.
3. **Read the whole file before judging any part of it.** Most false findings
   in this repo come from a guard that exists forty lines above or below the
   line that looked wrong.
4. **Report to the orchestrator, not just to GitHub.** Your final message is
   the only thing that survives; it should list every issue you filed by
   number and every suspicion you rejected with the reason.

## What counts as a finding

In descending value. Spend your budget top-down; do not start at the bottom
because it is easy.

| Rank | Class | Why it ranks here |
|---|---|---|
| 1 | **Wired to nothing** | Code that compiles, tests green, and does nothing at runtime. Eighteen recorded instances in the sibling repo, none caught by a test. See the `wired-to-nothing` skill for the checks. |
| 2 | **Confirm-boundary holes** | Anything that lets an executor act without a `Confirmed<P>`, or lets a proposal change between the card and the act. The product promise is Look, Propose, Confirm, Do. |
| 3 | **Leaks and lifetime bugs** | GDI/USER handles, COM interfaces, `HGLOBAL`, DIB sections, thread handles. A tray app that leaks runs for weeks. |
| 4 | **Idle work** | Any timer, poll, spin or wake while the app is doing nothing. CLAUDE.md rule 5; the owner audits this machine to the tenth of a watt. |
| 5 | **Unsound error paths** | A failure that ends in something other than a card, a swallowed `HRESULT`, a `?` that converts a recoverable error into an exit, an `unwrap` on the main thread. |
| 6 | **Secret exposure** | A key reachable through `Debug`, a log line, a panic message, an error string, a serialized struct, a diagnostics dump. |
| 7 | **Wrong results** | Arithmetic, unit conversion, time zones, encoding, surrogate pairs, off-by-one in slicing a `&str` by byte index. |
| 8 | **Missing tests on pure logic** | Only where the logic is pure and the absence is load-bearing. "This has no test" alone is not a finding. |
| 9 | **Structure and duplication** | Hand-mirrored match arms, a third copy of a table, a module that should be split. File one issue per *pattern*, not per site. |
| 10 | **Stale comments and docs** | Real but nearly worthless one at a time. **Batch them**: one issue per module group listing every site. Never file these individually. |

## Defect classes that actually recur in this code

Check these by name; each has bitten this repo or its sibling at least once.

**Win32 and GDI**
- A `CreateCompatibleBitmap`/`CreateDIBSection`/`CreateFontIndirect` without a
  matching `DeleteObject` on every path, including the early returns.
- A `SelectObject` whose previous handle is never restored before the DC dies.
- `GetDC` without `ReleaseDC`; `BeginPaint` without `EndPaint`.
- A hook installed on a thread that has no message loop (it will never fire).
- `PostMessage` to a window handle captured before the window existed, or
  after it was destroyed. Check the ordering in `app::run`.
- A `WM_COMMAND` id with no arm, or an arm for an id nothing sends.
- Missing `WM_DPICHANGED`, or fonts/metrics computed once at create time.
- `SendInput` and focus: what has focus when the input is synthesized, and
  what happens if the user is typing at the same moment.
- A message posted with `PostMessageW` that carries state which may be stale
  by the time it is received. This repo has already been bitten: see the
  preview generation in `WM_APP_PREVIEW_DECIDED` (#225).

**Threading**
- A worker closure with no `catch_unwind` under `panic = "abort"`.
- A `static` `AtomicX` shared between tests (a real race, not a flake).
- Anything the worker touches that is not `Send`, or a COM pointer crossing
  threads without the apartment being right.

**Strings and text**
- Slicing a `&str` or `OsString` by a byte index that may land inside a
  UTF-8 sequence or split a UTF-16 surrogate pair. Tray tooltips, card text
  and truncation helpers are the usual sites.
- Em dashes in user-facing strings (CLAUDE.md rule 11). `tests/no_em_dash.rs`
  gates this; a new user-facing surface not covered by it is a finding.

**Config, secrets, providers**
- A struct holding a key that derives plain `Debug`.
- An env override that reads differently from the file value.
- A provider error body echoed into a card verbatim.
- `serde_json` `preserve_order` assumptions: schema property order is
  load-bearing (CLAUDE.md rule 3). A schema built in the wrong order is a
  correctness bug, not style.
- Ollama: `localhost` instead of `127.0.0.1`, `think` left unset, `keep_alive`
  nested inside `options` (CLAUDE.md rule 6).

**Prompts and schemas**
- A prompt that describes an input the model is not actually given (the
  vision/OCR mismatch, #247).
- A schema property treated as a closed set in code but left open in the
  schema.
- Wording that reads as reasoning extraction: Anthropic's classifier returns
  `stop_reason: "refusal"` (MEASURED 2026-09-15).

## The dedupe gate

121 issues were open on 2026-09-19 and the last three audit rounds already
covered `app.rs`, `mode.rs`, `config.rs`'s provider arms, `ui/region`,
`ui/tray`, `ui/settings`, `connectors/ics`, `calc/units`, `single_instance`
and the executors' uia guard. Before filing anything:

```
gh issue list --state all --search "<symptom words>" --limit 20
gh issue list --state all --search "<function_name>" --limit 20
```

Search twice: once by symptom, once by identifier. A hit means comment with
your new evidence, not a new issue. Say in your report which numbers you
found and chose not to duplicate.

## Calibration

Filing is cheap and the tracker is allowed to be large, but each issue still
has to earn a stranger's minute. Before you file, answer in one line: *what
would a user or a maintainer do differently because this exists?* If the
answer is "nothing", it is a batch-into-one-issue item or nothing at all.

Mark every unobserved claim `THEORY (unverified):` and add `needs-repro`
(CLAUDE.md rule 10). An auditor who dresses a reading as an observation
costs more than one who files nothing.
