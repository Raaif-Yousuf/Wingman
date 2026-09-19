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

## Do these next

Ranked. The first three are the ones a user would notice.

1. **#271 (P1), the region overlay's click-select-window path is
   unconditionally broken.** The overlay is opaque and topmost with no
   hit-test exemption, so `WindowFromPoint` can only ever return the overlay
   itself. Every such click stages the whole desktop. Advertised behaviour,
   never worked, no test inspects the staged rect's value.
2. **#262 (P1), a failed clipboard restore permanently disables the retry.**
   `restore_now` sets `done = true` whether or not the restore succeeded, so
   `Drop`'s safety net is disabled at exactly the moment it is needed and
   the user loses their clipboard. Proven by execution.
3. **#266 and #267 (both P1), the executors.** `fill_form` writes a field
   with no staleness check, so it can silently overwrite something changed
   after the preview was shown and report success; `docs/executors.md`
   claims it does check. `replace_text` has no payment check at all, so
   "never touches payment data, no exceptions" currently has zero
   enforcement on it.
4. **#279 (P1), the cargo-test hook is bypassable** by any command prefix or
   nested shell. It is the only thing stopping a fan-out agent from
   exhausting this machine's RAM, and `time cargo test`, `cargo nextest run`
   and `bash -c "cargo test"` all walk straight through.
5. **#283 and #269 (P1, P2), `apply_config` forgets subsystems.** The mode
   mirror the Offline guard reads is never re-synced on Reload, and neither
   is the palette chord. The seam audit wrote the whole `Config` field
   propagation table into #283; only those two rows were wrong, but nothing
   stops a fourth.
6. **#257 (P1), the config ACL.** Applied after the write rather than
   before, swallowed on failure, and never applied at all by the
   `copilot-ask` migration, which copies a live key into a file with
   inherited permissions.
7. **#294 (P1)**, the main Ask path's own prompt says "You are shown a
   screenshot" on the non-vision path. #247 named this gap and declined to
   file it because the constant was outside its scope.

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

`#166` is the tracker. Tonight's verifier named four findings that cannot be
closed without a live desktop, and they should go on it: #271 needs a real
click on a second window under the overlay; #261 needs a genuinely hung UIA
provider; #263 needs an injected failing `IsPassword` call; #255 needs a
real focused control destroyed with `DestroyWindow` to see what
`WM_KILLFOCUS` Windows actually delivers.

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
