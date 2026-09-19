# Handoff: 2026-09-18/19 fan-out, and the salvage after it

Written 2026-09-19. Re-check `git log -1` before trusting any sha here.
Nothing pushed; the owner pushes.

## Read this first: the folder rename is ready to run

`scripts/rename-repo-folder-to-wingman.ps1`. Its preconditions are met as of
this commit: **one worktree, clean tree, no stray directories under
`.claude/worktrees/`**. Run it from a PowerShell window rooted somewhere other
than the repo, with no Claude Code session open and nothing building.
`-WhatIf` first.

It cannot be run from inside a session: Windows keeps an open handle on a
process's current directory. It also moves
`~\.claude\projects\C--Users-raaif-Wingman` (which holds the memory files),
fixes the absolute `CARGO_TARGET_DIR` in both `filing-findings` SKILL.md
copies, and updates the `Repo:` line in CLAUDE.md and AGENTS.md. Never run, so
read it before trusting it.

Five unmerged branches still exist (below). Branches do not block the rename;
only worktrees do, and there are none.

## What the run delivered

The fan-out was cut short: at about 12:05am the account hit its monthly spend
limit and ten agents were killed mid-task in one burst. Anything below marked
"finished at merge time" was salvaged afterwards from a WIP commit.

| Issue | State |
|---|---|
| #220 payment value-shape denylist at both fill_form layers | merged |
| #222 provider-name matching consolidated into config.rs | merged |
| #203 compile-fail gate for `Confirmed` | merged |
| #43 `docs/actions.md`, `docs/executors.md` | merged |
| #219 Selection-sourced "Do it" for Review this email | merged, the agent had finished it |
| #224 palette uses the shared `draw_text_line` | merged |
| #216 DirectWrite palette | merged; measurement and fallback observable added at merge time |
| #106 egress log | merged; read-back surface and config publish wired at merge time |
| #234 tray command id exhaustiveness | merged; three guards, mutation-checked |
| #225 stale-preview hijack (P1) | merged; fixed at merge time |
| #218, #205, #170 | closed without code (stale note, accepted, owner decision) |
| #105 "Show me what you're sending" | scaffolding merged, deliberately left unwired, see below |

Gate on master: fmt, clippy, deny, hooks, Pester, PSScriptAnalyzer all clean;
**1551 unit tests + 1 trybuild + 7 no-em-dash, 0 failures.**

## Three things a future session should know

**#105 is sequenced after #225 on purpose, and #225 is now done.** Its
structural gate (`ui::confirm::SendToken` / `SendAuthorized`, unforgeable in
the same shape as `Confirmed<P>`) is in place and fails safe: with the toggle
on and nothing authorized, every request is refused. What remains is `App::ask`
showing `ui::preview::RequestPreview` through `Card::show_preview` and calling
`user_confirmed_send` on Send. It was held back because it meant adding a third
pending kind to a preview state machine that had a live P1. It no longer does.
The unwired items carry `#[allow(dead_code)]` with that reason at each site.

**The #225 fix needed a third change nobody had predicted.** Making
`leave_preview_if_active` report abandonment, on its own, would have swapped
one wrong-action bug for another: `WM_APP_PREVIEW_DECIDED` is a `PostMessageW`,
so replacing a live preview posts the old one's abandonment and then arms the
new one in the same turn, and the stale notification would later clear the
state of the preview now on screen. Hence the preview generation carried in the
message's `WPARAM`. If you touch this area, keep the generation.

**The suite has one known flake, and its cause is not what the issue first
said.** #252: `executors::fill_form`'s real-window test can pick up a stray
keystroke, because the typed-input fallback calls `element.SetFocus()` and then
`SendInput` (`src/executors/target.rs:637`). The owner identified the actual
cause: a human typing at the keyboard while the suite runs. A cross-test mutex
would not have helped, and was nearly built before that correction. The
interesting fix is option 3 on the issue: have `fill_form` verify what actually
landed in the field. That is a real robustness gap for a user who keeps typing
during a fill, and the flake would disappear as a side effect.

A second flake,
`pause::tests::set_paused_until_resumed_then_set_running_round_trips`, **was** a
genuine test-versus-test race on the shared `PAUSE_DEADLINE` atomic, and is
fixed with a poison-tolerant test lock.

## Five unmerged branches

Each holds one WIP commit, clearly labelled, committed only so the work is not
lost. **None is verified. Four conflict with master; one does not compile.**
Finish or discard; do not merge blind.

| Branch suffix | Issues | Merge state | Where it stopped | New modules |
|---|---|---|---|---|
| `a16117ca859523463` | #42, #202 | conflicts | checking the test build compiles | -- |
| `ac841e1594a42647f` | #109 | conflicts | adding a `WM_DPICHANGED` test | `src/ui/placement.rs` |
| `a3881048d133bc867` | #111, #103 | conflicts | post-fmt test re-run, then live OCR measurements | `src/redact.rs` |
| `addbadcfdd437435f` | #21, #101 | conflicts | updating call sites for a new budget argument | `src/usage.rs`, `src/cost.rs` |
| `ac4408ff852377671` | #229, #212 | 3 compile errors | mid-replacement of `build_background`/`free_background` with an RAII guard | -- |

The conflicts are mostly against work that has since landed on master, so a
rebase is probably cheaper than a merge for all four.

## Highest-value open issues

- **#242**: any action from a user's `actions.toml`, including CONTRIBUTING.md's
  own tutorial example, appears in the palette and silently does nothing when
  clicked. The worst one open for a project whose pitch is that actions are the
  contribution surface.
- **#236 / #237** (both P1): any local process can post `WM_APP_ACTIVATE` and
  trigger a billed screenshot plus cloud request with no key press, or squat the
  fixed single-instance mutex name and make the app exit silently forever,
  including at autostart. #237 also fires benignly: `app::run()` takes the mutex
  before the owner window exists, so a slow duplicate launch hits the same silent
  exit with no adversary. The agent assigned these was killed before writing any
  code.
- **#227**: no `catch_unwind` in any worker, and `panic = "abort"` in release, so
  a worker panic kills the process with no card.
- **#229**: desktop-sized GDI bitmap leaks on every region-overlay open.
- **#245**: `units::convert` lets NaN and Infinity reach the card, and the
  module's own doc comment falsely claims otherwise.
- **#247**: the calendar and fill_form prompts open with "You are shown a
  screenshot", contradicting the non-vision OCR preface on every local-model
  press.
- **#250**: README still documents the pre-rename `copilot-ask` paths.

## Tooling added this run

- `scripts/rename-repo-folder-to-wingman.ps1` (see the top of this file).
- `scripts/merge-agent-branches.sh` -- merges agent branches one at a time,
  aborts rather than resolves a conflicting merge, rolls back a merge that does
  not build, runs the full suite once at the end. `--dry-run` lists each branch
  with the files it touches, which is how to plan merge order.
- `tests/compile_fail.rs` -- a `trybuild` gate for `Confirmed`'s privacy
  boundary. It does not link against the crate (impossible: bin-only, and
  `pub(crate)` is invisible from outside anyway); it regenerates a copy of
  `src/ui/confirm.rs` into a fixture and recreates the intra-crate relationship
  `src/executors/*.rs` has to it.
- Three tray-command guards (#234), including a source-scanning test that every
  `cmd::` id has a `WM_COMMAND` arm in `wnd_proc`.
- `.claude/skills/filing-findings/SKILL.md` -- the shared-machine cargo rules now
  scale past five agents: `CARGO_BUILD_JOBS=1` from six up, plus
  `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`.

## On running a fan-out again

Ten build agents plus five read-only auditors on one 31 GB machine held fine
(low 4.9 GB free, CPU around 52 percent). **The auditors were the better value
by a wide margin**: no RAM, no tree writes, and 28 findings including every P1
found that night, against three merged issues from the ten build agents in the
same window. Weight the split further toward auditors. Distinct per-agent
insertion anchors in `config.rs` worked: five agents adding fields to one
3264-line file produced zero conflicts.

The binding constraint was budget, not the machine. Size the fan-out to
remaining spend, and when a run dies, commit each worktree's WIP as one clearly
labelled unverified commit rather than leaving it loose.

## Manual checks owed

`#166` is the tracker and gained three entries this run: #216's DirectWrite
appearance, #225's end-to-end preview sequencing, and the #236/#237 checks.

Still owed from before: the one Settings click that points the Copilot key at
the app (Settings > Bluetooth & devices > Keyboard > Customize Copilot key >
Custom). No script can do it; `HKCU\...\Shell\BrandedKey` is write-protected by
the shell even in HKCU. The low-level hook works regardless, so "the key
responds" does not mean the picker route is live.
