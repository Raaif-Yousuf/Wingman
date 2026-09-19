# Handoff: after the 2026-09-18/19 overnight fan-out

Written 2026-09-19. `master` = `327dbc6`. Re-check `git log -1` before trusting
any sha here. Nothing pushed; the owner pushes.

The run ended early: at roughly 12:05am the Anthropic account hit its monthly
spend limit and **ten agents were terminated mid-task in one burst**. Three had
already finished and merged. The rest left unfinished work, which is preserved
(see "Unfinished work" below) but is **not verified and may not compile**.

## What landed on master

| Commit | What |
|---|---|
| `b409114` | Completed the merge the previous session left half-done (`#213`, `#214`, `#207`, `#209`). `src/app.rs` was in an unmerged `UU` state with the conflict already resolved but never staged. |
| `e0a00d1` | Merged the `#221` DrawTextW empty-text fix (adds `src/ui/text.rs`). |
| `cef9945` | Codex mirrors (`AGENTS.md`, `.agents/skills/`, `.codex/agents/`) committed; shared-machine cargo rules scaled for a 15-agent run. |
| `98461ac` | `scripts/rename-repo-folder-to-wingman.ps1`. |
| `cebdbbb` | `scripts/merge-agent-branches.sh`. |
| `63f41fd` | **#220**: value-shape payment denylist at both fill_form layers. |
| `1d51811` | **#222**: router/chain provider-name matching consolidated into `config.rs`. |
| `327dbc6` | **#203** compile-fail gate for `Confirmed`, **#43** `docs/actions.md` + `docs/executors.md`. |

Gate at `327dbc6`: fmt, clippy, deny, hooks, Pester, PSScriptAnalyzer all
clean; **1485 unit tests + 1 trybuild + 7 no-em-dash, 0 failures**.

Issues closed this run: **#220, #222, #203, #43, #218, #205**. Open went from
105 to 127 because the auditors filed 28 new findings; the owner said the count
is irrelevant.

## Do this first

1. **Run the rename.** `scripts/rename-repo-folder-to-wingman.ps1`, from a
   PowerShell window rooted somewhere other than the repo, with no Claude Code
   session open and after the worktrees below are dealt with. `-WhatIf` first.
   It cannot be run from inside a session: Windows holds an open handle on a
   process's current directory. It also moves
   `~\.claude\projects\C--Users-raaif-copilot-ask` (which holds the memory
   files) and fixes the absolute `CARGO_TARGET_DIR` in both `filing-findings`
   SKILL.md copies. Never run, so read it before trusting it.
2. **Decide the three highest-severity findings.** They are listed below and
   none of them is fixed.
3. **Then either finish or discard the unfinished branches.** Do not merge one
   without finishing it and re-running `scripts/verify-all.sh`.

## The three findings that matter most

- **#242 (dead wire, and the worst one for a community project).** Any action
  from a user's `actions.toml`, including CONTRIBUTING.md's own "add an action
  in 20 minutes" tutorial example, appears in the palette and **silently does
  nothing** when clicked. `ui/palette_model.rs`'s `dispatch_target_for` is a
  closed 7-arm match with `_ => None`, and `app.rs`'s `dispatch_palette_action`
  no-ops on `None` with no card. `catalogue()` meanwhile adds every action
  unconditionally. A contributor following the docs gets a working-looking
  action that does nothing.
- **#225 (P1, breaks Confirm).** `Card::hide()` destroys an in-flight preview
  without posting `WM_APP_PREVIEW_DECIDED`, so `pending_review` /
  `pending_form_fill` is never cleared. Five call sites hide unconditionally.
  Two slots can then both be `Some`, and because `on_preview_decided` checks
  `pending_review` first, **pressing "Do it" on a visible form-fill preview can
  run a stale abandoned email replace-text instead**, dropping the confirmed
  action with no card. A fixer agent had written the red tests and was about to
  run them when it was killed; see `worktree-agent-a8b00fdc77b61f899`.
- **#236 / #237 (P1, local attack surface).** `WM_APP_ACTIVATE` is posted to a
  hardcoded public window class and handled with no sender authentication, so
  any process in the session can drive a screenshot plus a billed cloud request
  with no key press. And the single-instance mutex name is a fixed public
  string, so any process can pre-hold it and make Wingman exit silently forever,
  including at autostart. #237 also fires benignly: `app::run()` takes the mutex
  well before the owner window exists, so a slow duplicate launch hits the same
  silent exit with no adversary.

Also worth reading early: **#227** (no `catch_unwind` in any worker, and
`panic = "abort"` in release, so a worker panic kills the process with no card),
**#229** (desktop-sized GDI bitmap leaks on every region-overlay open),
**#245** (`units::convert` lets NaN/Infinity reach the card, and the module's
own doc comment falsely claims otherwise), **#247** (the calendar and fill_form
prompts open with "You are shown a screenshot", contradicting the non-vision
OCR preface on every local-model press), **#250** (README still documents the
pre-rename paths), **#252** (the `fill_form` real-window test is flaky under
parallel Win32 tests: a stray keystroke prepended an `l` to an email field, a
false-green risk on the only automated evidence the write path works).

## Unfinished work: nine WIP branches

Each was committed on its own branch as a single WIP commit so nothing is lost.
**None is verified. Several will not compile.** Each carries a commit message
saying so. Finish or discard; do not merge blind.

| Branch suffix | Issues | Where it stopped | New modules it adds |
|---|---|---|---|
| `a8b00fdc77b61f899` | #225 | red tests written, about to run them | -- |
| `ad2368162a616561d` | #236, #237, #238 | nothing on disk; had only read the code | -- |
| `ac4408ff852377671` | #229, #212 | mid-replacement of `build_background`/`free_background` with an RAII `Background` struct | -- |
| `a7930078964f7d912` | #224, #216 | DirectWrite palette written, waiting on its first `cargo test ui::palette` | -- |
| `ad7cea07323f6fcf0` | #219 | clippy clean, re-running tests after fmt | -- |
| `ac841e1594a42647f` | #109 | adding a `WM_DPICHANGED` test | `src/ui/placement.rs` |
| `a3881048d133bc867` | #111, #103 | post-fmt test re-run, then the live OCR measurements | `src/redact.rs` |
| `addbadcfdd437435f` | #21, #101 | updating call sites for a new budget argument | `src/usage.rs`, `src/cost.rs` |
| `a16117ca859523463` | #42, #202 | checking the test build compiles | -- |
| `ae8f016c17046c58e` | #105, #106 | testing `known_folder` and `config` | `src/egress.rs`, `src/known_folder.rs` |

The three merged branches' worktrees are clean and can be removed. All of them
must be removed and pruned before the folder rename, because a worktree's
`gitdir` file carries an absolute path.

## New tooling this run

- **`scripts/merge-agent-branches.sh`** -- merges agent branches one at a time,
  aborts (never resolves) a conflicting merge, rolls back a merge that does not
  build, and runs the full suite exactly once at the end. `--dry-run` lists each
  branch with the files it touches, which is how you plan the merge order.
  Already used for all three merges above.
- **`scripts/rename-repo-folder-to-wingman.ps1`** -- see "Do this first".
- **`.claude/skills/filing-findings/SKILL.md`** -- the shared-machine cargo
  rules now scale past five agents: `CARGO_BUILD_JOBS=1` from six agents up,
  plus `CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0`, because the
  linker is the RAM peak and debug info is most of what it holds.
- **`tests/compile_fail.rs`** -- `trybuild` gate for `Confirmed`'s privacy
  boundary. It does not link against the crate (impossible: bin-only, and
  `pub(crate)` is invisible from outside anyway). It regenerates a copy of
  `src/ui/confirm.rs` into a fixture and recreates the intra-crate relationship
  `src/executors/*.rs` has to it. Proven to fail when `value` is widened to
  `pub(crate)`. The old `compile_fail` doc example it replaced would never have
  failed: struct and fabricating fn shared one module.

## What the fan-out shape taught us

Ten build agents plus five read-only auditors on one 31 GB machine held fine
(low was 4.9 GB free, CPU ~52 percent). **The auditors were the better value**:
they cost no RAM, never touched the tree, and produced 28 findings including all
three P1s, while the build agents produced three merged issues in the same
window. On a fixed budget, weight the split further toward auditors. Distinct
per-agent insertion anchors in `config.rs` worked: zero conflicts across five
agents adding fields to one 3264-line file.

## Manual checks still owed

`#166` is the tracker and got several new entries this run. The #32 bullet's
stale "nothing calls `ui::confirm::confirm` for a real user click yet" claim was
corrected (issue #218): that flow has existed since #39/#38 and the box is
checkable today against a real Notepad field.

Still owed from before: the one Settings click that points the Copilot key at
the app (Settings > Bluetooth & devices > Keyboard > Customize Copilot key >
Custom). No script can do it; `HKCU\...\Shell\BrandedKey` is write-protected by
the shell even in HKCU. The low-level hook works regardless, so "the key
responds" does not mean the picker route is live.

## Owner decision owed

**#170**: whether to allow lossy JPEG/WebP screenshot encoding instead of
PNG-only. The issue's own Done-when says this is an owner decision, not a
technical one. Recommendation: keep PNG and do not add the option. Lossy
artifacts hurt exactly the small on-screen text the app exists to read, and the
pixel-budget downscaling already captures most of the payload win.
