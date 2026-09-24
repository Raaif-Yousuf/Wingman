# Handoff: the 2026-09-19 audit round

Written 2026-09-19. Re-check `git log -1` before trusting any sha here.
Nothing pushed; the owner pushes.

## Read this first

**The folder rename is done.** `C:\Users\raaif\copilot-ask` is now
`C:\Users\raaif\Wingman`, renamed by hand rather than by
`scripts/rename-repo-folder-to-wingman.ps1`, so the four dependent rewrites
the script would have done did not happen automatically. All four are now
handled: the Claude Code project state and its memory directory were copied
across, there were no worktrees to orphan, both `filing-findings` SKILL.md
copies had their absolute `CARGO_TARGET_DIR` fixed, and the `Repo:` line in
CLAUDE.md and AGENTS.md was corrected. The script itself is now only a
checklist, and it targets lowercase `wingman`; do not run it.

**There is a draft spec awaiting your approval.**
`docs/superpowers/specs/2026-09-19-activation-trust-design.md` covers #236
and #237, both P1, and its § 7 lists four decisions only you can make. No
code was written for either issue, per rule 12. Its central finding is worth
reading even if you reject the rest: sender authentication cannot close
either hole against a same-user, same-integrity process, and the named-pipe
approach both issues suggest moves the attacker from one `PostMessageW` to
one `CreateProcess` without changing what the user loses.

## What this round was

Twelve read-only auditors, then one adversarial verifier, then six fix
agents in worktrees. The auditors filed **issues #253 to #297**; the
verifier re-checked the thirteen most serious and **confirmed all thirteen,
refuting none**, three of them by copying the function body into a scratch
program and running it. The tracker is now 166 open, 43 of them P1.

Coverage is recorded in [`docs/audit-coverage.md`](docs/audit-coverage.md),
which is new: the tracker records findings but cannot tell you where nobody
has looked, and an unaudited 2000-line module looks exactly like a clean one
from the issue list. Every area of the tree now has a date against it.

The auditor brief is now a skill,
[`.claude/skills/auditing-a-module`](.claude/skills/auditing-a-module/SKILL.md),
so the next round shares one brief instead of twelve hand-written ones.

## Landed on master

| Commit | What |
|---|---|
| `a55ab42` | the repo's own paths pointed at the renamed folder |
| `ef367de` | the `auditing-a-module` skill, `docs/audit-coverage.md`, the activation-trust spec |
| `91699a3` | `every_wm_app_id_has_a_wnd_proc_arm` and `every_wm_app_id_is_posted_somewhere`, both mutation-checked |
| `6ef81f7` | #253, #273, #260 and #281: provider error text redacted, the scrubber widened, the provider test race closed |
| `7817f0f` | the documentation truth pass, #250 and #286 to #293 |
| `8898a47` | the folder-rename claim the docs pass left contradicting itself |
| `98ee978` | #279 the bypassable cargo-test hook, #257 the config ACL window |
| `e605cc1` | #271 the region click-select, #272 the overlay stranded on Alt-Tab |
| `27c755c` | #262 the clipboard restore, #263 IsPassword fail-open, #265 CF_DIBV5 |
| `6188cb0` | #266 fill_form staleness, #267 replace_text payment checks |
| `7b48a2d` | #269, #283, #268, plus a guard for the apply_config-forgets-a-mirror class |
| `ff30d24` | the two ways the merged suite did not pass, both introduced tonight |

### The two findings worth carrying forward

**A guard test that cannot fail is worse than no guard.** Two of tonight's
scanning tests were vacuous when written, in two different ways: one was
anchored on `\n` while `include_str!` hands back CRLF on this working copy,
and one filtered line by line for a construct this file's own rustfmt'd
style always splits across lines (#281). Both reported green while checking
nothing. Every source-scanning test now asserts its candidate set is
non-empty first, with a message saying the scanner has broken, and every new
one is mutation-checked with both outputs recorded in the commit message.

**The redaction hole was the shape of the alphabet, not the threshold**
(#273). The scrubber's token alphabet was base64url and nothing else, so a
credential in standard base64 or split by a vendor's punctuation was seen as
several short fragments, each under the 20-character threshold, and every
one survived intact. Those characters cannot simply join the alphabet,
because a URL and a filesystem path are long runs over exactly them. A wide
run is now scrubbed whole only when it also carries upper case, lower case
and a digit.

**The gate on master is green:** fmt, clippy, deny, hooks, Pester and
PSScriptAnalyzer all clean, **1611 unit tests + 1 trybuild + 7 no-em-dash, 0
failures**. Thirteen of tonight's issues are fixed and merged; the rest are
filed.

### What the merge gate caught that no agent's own run did

Worth knowing before the next fan-out, because both were invisible to the
filtered runs each agent was told to use.

**The suite hung, and the cause was mine.** Rewriting `provider::common`'s
tests onto one `network_guard()` left one test taking it twice, because that
test had had both a mode lock and an egress lock and two separate rewrites
each matched it. `std::sync::Mutex` is not reentrant, so it deadlocked and
took the whole run with it. Found by running the binary with
`--test-threads=1` and reading which test the log stopped after.

**Two `ui::region` tests were racy, and fixing them improved the product
code.** The overlay under test is a real top-level window, and the rest of
the suite creates and destroys real windows constantly, so genuine
`WA_INACTIVE` messages arrive mid-test and any "the overlay was NOT
cancelled" assertion fails whenever one does. `on_activate` now cancels only
once the overlay has actually been activated, which is also the more correct
production behaviour, and the decision moved into a pure function tested
exhaustively.

## Do these next

Ranked. Seven of the original list are now done; what remains is below.

1. **#294 (P1)**, the main Ask path's own prompt says "You are shown a
   screenshot" on the non-vision path. #247 named this gap and declined to
   file it because the constant was outside its scope, so it has been known
   and unowned for two rounds.
2. **#261 (P1), UIA calls have no timeout**, so a hung foreground app wedges
   `self.busy` forever and the app stops responding to the key with no card.
   Needs a decision about what a timeout does to a half-read element, which
   is why it was not handed to a fix agent tonight.
3. **#263's live check.** The fix landed, but it is fail-closed now: a field
   whose password status cannot be read is treated as a password. Confirm on
   a real desktop that this does not make ordinary fields unusable.
4. **#236 and #237 (both P1)**, blocked on the spec above. Nothing should be
   written for them until you have answered its § 7.
5. **#298 (P2), a new consequence of #257's fix.** `Config::save()` can now
   return `Err` on an ACL failure where it previously could not, and no
   caller turns that into a card. Rule 7 says it must.
6. **#242 (P2), custom actions.toml actions do nothing when clicked.** Still
   the worst one open for a project whose pitch is that actions are the
   contribution surface.
7. **#227 (P2), no `catch_unwind` in any worker**, with `panic = "abort"` in
   release, so a worker panic kills the process with no card.

The full ranked list is the tracker: 43 issues carry P1, and
`docs/audit-coverage.md` says which parts of the tree the round actually
read.

## The five older unmerged branches

Triaged in **#285**. The recommendation is **rebase and finish, all five**:
none is redundant with what landed on master and none is a design that does
not work. Two things that table did not say before:

- `addbadcfdd437435f` (#21, #101) has an unflagged compile error of its own,
  not just conflicts: about twelve test call sites are missing the argument
  the branch itself added.
- Master's #214 refactor collapsed the four `readiness_gate` call sites that
  branch patches into one, which makes finishing it **cheaper** than its own
  original approach, provided whoever does it reads #214 first instead of
  fighting a blind rebase.

`ac4408ff852377671` (#229) does not touch #271's code path, so those two can
proceed independently.

## Manual checks owed

`#166` is the tracker, and `OWNER_TODO.md` carries the same list. Tonight's
verifier named four findings that cannot be closed without a live desktop,
and two of them are now checks against fixes that have already landed:

- **#271**, fixed: open the overlay over two overlapping real windows and
  click without dragging on the front one; the size label must match that
  window, not the whole desktop. Also click bare desktop background: the
  fixing agent flagged, as an unverified theory, that Progman or WorkerW may
  appear in the window snapshot with a full-monitor rect.
- **#272**, fixed: open the overlay, Alt-Tab away, and it must disappear
  rather than stranding unresponsive to Escape.
- **#263**, fixed: the `IsPassword` read now fails closed, so confirm a real
  form still fills normally and only a genuinely unreadable field is
  skipped.
- **#261**, not fixed: needs a genuinely hung UIA provider to observe `busy`
  wedging.
- **#255**, not fixed: needs a real focused preview control destroyed with
  `DestroyWindow`, to see what `WM_KILLFOCUS` Windows actually delivers.

Still owed from before: the one Settings click that points the Copilot key
at the app. No script can do it.

## On running a fan-out again

- **Read-only auditors remain the best value by a wide margin**, now twice
  measured. Twelve of them wrote nothing to the tree, produced 45 issues and
  conflicted with nobody.
- **Add a verifier pass.** One agent whose only job was to attack the
  round's P1s turned a pile of `THEORY (unverified)` into something safe to
  hand to fixers, and would have been worth it even if it had refuted
  nothing.
- **An auditor scoped to "the seams between modules" finds what per-module
  agents structurally cannot.** Both of its findings were about a value
  crossing a boundary.
- **The orchestrator needs its own `CARGO_TARGET_DIR` too.** MEASURED
  2026-09-19: with agents running filtered tests in the main checkout,
  orchestrator builds failed repeatedly with `LNK1104`, because cargo's file
  lock serializes compilation but not the linker's output path.
- Six building agents took this 31 GB machine down to 1.6 GB free. Four is
  the comfortable number with the orchestrator also building.
