# Audit coverage ledger

Which parts of the tree have been read by a dedicated auditor, when, and what
came out. The tracker records findings; this file records *where we have
looked*, which the tracker cannot tell you. An unaudited 2000-line module and
a clean one look identical from the issue list.

Kept by the orchestrator of each fan-out, not by the auditors themselves
(auditors never write to the tree; see
[`auditing-a-module`](../.claude/skills/auditing-a-module/SKILL.md)).

**How to use it.** Before dispatching auditors, sort by "last audited" and
send them at the oldest and the never. Re-audit a module when it has changed
substantially since its last pass, not on a clock.

## Coverage

| Area | Files | Last audited | Round | Outcome |
|---|---|---|---|---|
| Providers | `src/provider/*` | 2026-09-19 | 4 | #253, #254 |
| Card and confirm chain | `src/ui/card.rs`, `preview.rs`, `confirm.rs`, `text.rs`, `src/dismiss.rs` | 2026-09-19 | 4 | #255, #256, #284 |
| Config and secrets | `src/config.rs`, `secrets.rs`, `dpapi.rs`, `known_folder.rs`, `src/profile/*` | 2026-09-19 | 4 | #257, #258, #259 |
| Inputs and capture | `src/inputs/*`, `src/capture.rs`, `src/ocr.rs` | 2026-09-19 | 4 | #261, #262, #263, #264, #265 |
| Actions and executors | `src/actions/*`, `src/executors/*`, `src/router.rs`, `src/payment_denylist.rs` | 2026-09-19 | 4 | #266, #267 |
| App shell and input plumbing | `src/app.rs`, `mode.rs`, `hotkey.rs`, `hotkey_conflicts.rs`, `pause.rs`, `single_instance.rs` | 2026-09-19 | 4 | #268, #269 |
| Remaining UI | `src/ui/settings.rs`, `palette.rs`, `palette_model.rs`, `region.rs`, `tray.rs` | 2026-09-19 | 4 | #271, #272 |
| Egress, diagnostics, connectors, calc | `src/egress.rs`, `diagnostics.rs`, `src/connectors/*`, `src/calc/*`, `src/autostart.rs` | 2026-09-19 | 4 | #270, #273, #274, #275, #276, #277 |
| Test suite quality | `tests/*`, in-module `#[cfg(test)]` | 2026-09-19 | 4 | #260, #281, #282 |
| Build, CI, packaging | `Cargo.toml`, `deny.toml`, `scripts/*`, `packaging/*`, `install.ps1` | 2026-09-19 | 4 | #278, #279, #280 |
| Prompt and schema corpus | `src/provider/mod.rs` (`DEFAULT_PROMPT`), `src/actions/*.rs` (each action's `BASE_PROMPT`), `src/actions/schema.rs` | 2026-09-19 | 4 | #294, #296, #297 |
| Cross-module seams | interfaces rather than one owning file: `config.rs` / `mode.rs` / `app.rs` (mode sync), `app.rs` / `provider/*` (vision-capability routing) | 2026-09-19 | 4 | #283, #295 |
| The docs themselves | `README.md`, `CLAUDE.md`, `AGENTS.md`, `PRIVACY.md`, `docs/*`, `docs/superpowers/specs/*` | 2026-09-19 | 4 | #286, #287, #288, #289, #290, #291, #292, #293 |

"Partial" means findings exist from a round that was not scoped as a
dedicated pass over that area: real defects were found, but nobody read the
whole thing.

## Rounds

**Round 3, 2026-09-18 overnight.** Five read-only auditors alongside ten build
agents. 28 findings including every P1 of the night. MEASURED: the auditors
were the better value by a wide margin, at no RAM cost and no tree writes.
Issues #226 to #252.

**Round 4, 2026-09-19 overnight.** Auditors only, two at a time, against the
areas above in order: providers, the card and confirm chain, config and
secrets, inputs and capture, actions and executors, the app shell, the
remaining UI, egress/connectors/calc, build/CI/test quality, cross-module
seams, the docs themselves, and the prompt and schema corpus.
`auditing-a-module` skill written before the round so each pair shares one
brief instead of a hand-written one. Issues #253 to #297; every finding
against the docs (#286-#293) turned out to be the docs underclaiming work
that had already shipped, not the code being wrong, and was fixed in place
by a dedicated docs fix agent rather than left as open issues. #285 (triage
of the five unmerged 2026-09-18 fan-out branches) is a process item, not a
module finding, and is not attributed to any single area row above.

Round 4 ran wider than its area rows suggest and then turned into a fix
round: thirteen of its findings were fixed and merged the same night, by six
agents working in isolated worktrees, and one adversarial verifier re-checked
the thirteen most serious findings before any of that started, confirming all
thirteen and refuting none.

## Where round 4 did NOT look

"Audited" above means an auditor read the area with a brief. It does not mean
every line. These are the gaps the round's own agents named when they ran out
of budget, recorded here because the issue tracker cannot express "nobody
read this" and the next round should start from it rather than rediscover it.

| Not read | Reported by |
|---|---|
| `src/config.rs` ~2340-2715 and ~2775-2930 (secrets import/hydrate/push test bodies) | config and secrets |
| `src/app.rs` ~2380-2600 (`calendar_worker`, `review_worker`, `form_fill_worker`, `router_worker` bodies) | app shell |
| `src/actions/review_email.rs` ~880-1541 (its test module) | actions and executors |
| `packaging/Wingman.Common.Tests.ps1` (47 KB Pester suite; only the module it tests was read) | build and CI |
| `.github/ISSUE_TEMPLATE/*.yml` (only `config.yml` was read) | build and CI |
| `scripts/rename-repo-folder-to-wingman.ps1`, `scripts/merge-agent-branches.sh` (skimmed for injection only) | build and CI |
| The `#[cfg(test)]` modules of roughly 60 `src/*.rs` files, individually | build and CI |
| `src/provider/ollama_admin.rs` ~500-600 (`pull`, `stream_pull_progress`) not build-verified | providers |

The last row on that list is the one worth acting on: **no exhaustive
per-test audit of the ~1600 `#[test]` functions was done.** The build-and-CI
agent attempted a scripted sweep, it timed out, and the agent fell back to a
seven-file sample. That sample found nothing gross, but two genuinely vacuous
tests were found by other means the same night (#281, and the guard the
app-shell fixer mutation-checked), so the base rate is not zero. A round 5
scoped only at test quality, with a working scanner, is a reasonable use of
one agent.
