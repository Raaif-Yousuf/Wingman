# Documentation index

> The detailed reference. `AGENTS.md` at the repo root is the quick-reference
> card and points here for depth.

A native Windows 11 tray assistant behind the Copilot key. Rust, Win32, one
exe, cloud or local models, MIT.

## Who owns which fact

Several files look like places to write the same sentence. Each owns a
different kind of fact, on purpose. Before "simplifying" two of them into one,
check whether the fact being moved is a backlog item, a design decision, a
handoff snapshot, or a human-only action: those are four different things.

| Surface | Owns | Lifecycle |
|---|---|---|
| GitHub Issues (once the public repo exists) | every bug, feature and roadmap item | open to closed; nothing duplicates it in markdown |
| `docs/superpowers/specs/` | design decisions and their rationale, one dated file per topic | written before the code; superseded specs say so at the top and stay in git |
| `NEXT_SESSION.md` (gitignored, per contributor) | where your last session stopped and what the next one should do first | local only; never committed |
| [`OWNER_TODO.md`](../OWNER_TODO.md) | actions only a human can take: a Settings click, a repo name, an account | rows deleted when done |
| [`CHANGELOG.md`](../CHANGELOG.md) (Keep a Changelog) | what shipped, per version | append-only |

## Reading order for a new contributor

1. [`README.md`](../README.md) at the root: what it does and how to install it.
2. [`AGENTS.md`](../AGENTS.md): the rules and the pitfalls, ten minutes.
3. [`2026-09-14-copilot-ask-design.md`](superpowers/specs/2026-09-14-copilot-ask-design.md): the running app.
4. [`2026-09-15-packaging-and-install-design.md`](superpowers/specs/2026-09-15-packaging-and-install-design.md): how it gets onto a machine.
5. [`2026-09-16-expansion-plan-design.md`](superpowers/specs/2026-09-16-expansion-plan-design.md): where it is going.

## Skills (`.claude/skills/`)

Loaded with the `Skill` tool at the start of the matching task. The
instructions live there, not duplicated here.

| Skill | Reach for it when |
|---|---|
| `tests-first` | Before writing any feature, fix or behaviour change: failing test, red output, neighbouring tests, then code |
| `fixing-a-bug` | Fixing any bug or regression, before writing fix code. Also when a fix "should work" but the symptom persists |
| `wired-to-nothing` | Before reporting any change as done. The Win32-specific list of ways code compiles, passes and does nothing |
| `working-an-issue` | Before starting or closing any GitHub issue |
| `filing-findings` | Before filing an issue about something you noticed, and for the shared-machine cargo RAM rules |
| `auditing-a-module` | Auditing a module or file set for defects worth filing, and when dispatched as a read-only auditor in a fan-out |

Agents (`.claude/agents/`): `cold-diff-reviewer` reviews a diff with no ticket
or author framing, checking for this repo's recorded bug shapes.

Hooks (`.claude/settings.json`, scripts in `scripts/hooks/`): a recursive
force-delete aimed inside the repo and any mutating `git stash` are refused
before they run. Both scripts explain the replacement in their message.

## Orchestrator scripts (`scripts/`)

| Script | What it does |
|---|---|
| `merge-agent-branches.sh` | Merges a fan-out's leftover branches one at a time, least-contended first; aborts and reports any conflict instead of resolving it |
| `pr-overlap.sh` | Lists open pull requests with author, CI state, mergeable state and changed files, finds every pair that touches the same file, and suggests a merge order (fewest overlaps first). Needs `gh` and `jq` |

## Index

| Doc | What is in it |
|---|---|
| [superpowers/specs/2026-09-14-copilot-ask-design.md](superpowers/specs/2026-09-14-copilot-ask-design.md) | Threading model, module contracts, config shape, provider request shapes (OpenAI Responses, Anthropic Messages), capture, the GDI card, tray, hotkeys and learn mode, error handling, test plan |
| [superpowers/specs/2026-09-15-packaging-and-install-design.md](superpowers/specs/2026-09-15-packaging-and-install-design.md) | Install layout, why sparse MSIX and not full, manifest, signing, launch semantics, the Copilot-key picker registration and the one click that cannot be scripted |
| [superpowers/specs/2026-09-16-expansion-plan-design.md](superpowers/specs/2026-09-16-expansion-plan-design.md) | Name candidates, target architecture, extended provider trait, Ollama and OpenAI-compatible and Gemini providers, modes and the offline guard, the Quick Ask palette and actions-as-data, the no-chat rule and the settings window, the awareness engine, secrets separation, repo and CI files for GitHub, open-source sources with verified licenses, decisions owed, phased roadmap and issue map |
| [providers.md](providers.md) | Per-provider request shapes, auth and key storage, effort/thinking mapping, structured-output mechanism, retry/429 handling, Ollama-specific facts, how to add Ollama or Gemini to `providers.order` by hand |
| [offline.md](offline.md) | The four modes exactly as `mode.rs` implements them, what the Offline guard blocks and where, what it cannot guarantee, how to verify it with pktmon or Resource Monitor |
| [reproducible-builds.md](reproducible-builds.md) | What is and is not verified byte-for-byte reproducible (`wingman.exe` is, `wingman.msix` is not yet), the `/Brepro`/`SOURCE_DATE_EPOCH`/toolchain-pin measurement, how to reproduce it, and the CycloneDX SBOM `release.yml` attaches to every release |
| [actions.md](actions.md) | The `actions.toml` schema field by field, two worked examples, the proposal schema registry, and why `serde_json` keeps `preserve_order` |
| [executors.md](executors.md) | The executor contract, the four rules, the `Confirmed<P>` privacy boundary, the stale-target check, and the never-Send/Submit/Buy/Pay rule |
| [audit-coverage.md](audit-coverage.md) | Which parts of the tree an auditor has actually read, when, and what came out. The issue tracker cannot tell you where nobody has looked |
| [positioning.md](positioning.md) | Positioning and launch research (Hacker News, GitHub, Windows enthusiast forums, tech press), the one-line pitch, and the launch-day checklist |

Planned, per the expansion plan § 10: `architecture.md`.
