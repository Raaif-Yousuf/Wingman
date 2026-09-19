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
| Providers | `src/provider/*` | 2026-09-19 | 4 | in progress |
| Card and confirm chain | `src/ui/card.rs`, `preview.rs`, `confirm.rs`, `text.rs`, `src/dismiss.rs` | 2026-09-19 | 4 | in progress |
| Config and secrets | `src/config.rs`, `secrets.rs`, `dpapi.rs`, `known_folder.rs`, `src/profile/*` | never | -- | -- |
| Inputs and capture | `src/inputs/*`, `src/capture.rs`, `src/ocr.rs` | never | -- | -- |
| Actions and executors | `src/actions/*`, `src/executors/*`, `src/router.rs`, `src/payment_denylist.rs` | partial (2026-09-18) | 3 | #242, #245, #246, #247, #252 |
| App shell and input plumbing | `src/app.rs`, `mode.rs`, `hotkey.rs`, `hotkey_conflicts.rs`, `pause.rs`, `single_instance.rs` | partial (2026-09-18) | 3 | #227, #236, #237, #243 |
| Remaining UI | `src/ui/settings.rs`, `palette.rs`, `palette_model.rs`, `region.rs`, `tray.rs` | partial (2026-09-18) | 3 | #229, #230, #231, #233 |
| Egress, diagnostics, connectors, calc | `src/egress.rs`, `diagnostics.rs`, `src/connectors/*`, `src/calc/*`, `src/autostart.rs` | partial (2026-09-18) | 3 | #238, #248, #249 |
| Test suite quality | `tests/*`, in-module `#[cfg(test)]` | never | -- | -- |
| Build, CI, packaging | `Cargo.toml`, `deny.toml`, `scripts/*`, `packaging/*`, `install.ps1` | partial (2026-09-18) | 3 | #251 |

"Partial" means findings exist from a round that was not scoped as a
dedicated pass over that area: real defects were found, but nobody read the
whole thing.

## Rounds

**Round 3, 2026-09-18 overnight.** Five read-only auditors alongside ten build
agents. 28 findings including every P1 of the night. MEASURED: the auditors
were the better value by a wide margin, at no RAM cost and no tree writes.
Issues #226 to #252.

**Round 4, 2026-09-19 overnight.** Auditors only, two at a time, against the
areas above in order. `auditing-a-module` skill written before the round so
each pair shares one brief instead of a hand-written one.
