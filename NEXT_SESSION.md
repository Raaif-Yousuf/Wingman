# Handoff: after the 2026-09-16 planning session

Written 2026-09-16. First handoff for this repo. Re-check `git log -1` before
trusting any sha here.

`master` = `80b2dcc` ("Make Windows aware the app exists"). Working tree has
uncommitted additions from this session (listed below). Nothing pushed; no
GitHub repo exists yet.

## What this session was

Planning only, by owner instruction. No app code changed. The project moved
from `Downloads\copilot-ask` to `C:\Users\raaif\copilot-ask`; the Claude memory
directory was copied across and the project memory's path updated.

Produced, all uncommitted:

- `docs/superpowers/specs/2026-09-16-expansion-plan-design.md`: the plan.
  Name candidates, architecture, providers (Ollama, OpenAI-compatible, Gemini),
  modes and the offline guard, the palette and actions-as-data, settings window
  options, awareness engine, secrets separation, GitHub repo files, verified
  open-source sources, decisions owed, roadmap and issue map.
- `CLAUDE.md`, `docs/README.md`, `.claude/skills/{tests-first,fixing-a-bug,
  wired-to-nothing,working-an-issue}`, `.claude/agents/cold-diff-reviewer.md`,
  `.claude/settings.json` with two PreToolUse hooks under `scripts/hooks/`,
  `OWNER_TODO.md`, this file. All ported from the sibling CLAIR repo and cut to
  what applies to an 8k-LOC Rust crate.

## Decisions made by the owner this session

Name **Wingman**, **no chat at all** (one press, one action, one card; the
WebView2 window via `wry` is a rarely opened settings window only), keys in
Credential Manager,
awareness **on** by default (consent screen required), copied code MIT-only.
Scope reframed: a universal act-on-what-is-on-screen assistant (Look, Propose,
Confirm, Do), not a homework tool; community project. Recorded in the plan's
header table and § 1, § 6.

GitHub repo `Raaif-Yousuf/Wingman` exists (empty, public); the logged-in `gh`
account has admin. Filed from this session: 14 labels, 7 milestones (Phases
0 to 5 plus "Phase 3b — Knowledge and memory"), issues #1 to #130 each with a
"Done when" criterion, roadmap #80 pinned, seven `good first issue` actions.
Late additions: the knowledge-and-memory subsystem (plan § 18, issues #81 to
#97) and an additional-functionality sweep (plan § 19, #98 to #130). `origin`
remote added locally; **nothing pushed** (owner pushes).

## Positioning and launch (added late in the session)

`docs/positioning.md` holds the research read-out: five differentiators
against Copilot, the WinRAR/Greenshot/Notepad++ test, the ranked demand
list, the distrust triggers turned into rules, and a 10-step launch
sequence. The recommendation is that **the attention moment is v0.2 (Phase
2 complete plus a no-model tier)**, timed to Microsoft's Copilot-key remap
setting reaching GA (Release Preview 2026-09-10) and before the Windows 10
ESU end on 2026-10-13. Reddit was unreachable from automated tools, so
the owner connected the Chrome extension and the threads were read by hand
through `old.reddit.com` JSON pages; findings are in the doc's "What Reddit
actually says" section. Reusable extractor: the session scratchpad's
`comments.py` pattern (regex over saved `get_page_text` dumps, since the page
text is capped at 50k characters and breaks strict JSON parsing).

## What the next session does first

1. `git push -u origin master` if the owner has not, so the issues' spec links
   resolve. Commit this session's files first (owner's call on the message).
2. Phase 0 (milestone "Phase 0 — Rename and go public"): start with
   `superpowers:writing-plans` against the approved spec, then `tests-first`
   per task. First issue: the rename with config migration.
3. Before any task: `working-an-issue`. Before reporting any task done:
   `wired-to-nothing`.

## To verify by hand

Nothing outstanding from this session. Carried over from the packaging work:
the owner may still owe the one Settings click that points the Copilot key at
the app (Settings ▸ Bluetooth & devices ▸ Keyboard ▸ Customize Copilot key ▸
Custom). Ask before assuming the picker route is live; the hook route works
regardless.

## Facts established this session, not derivable from the code

- Git history contains no key-shaped strings (`git log -p --all` grep for
  `sk-` patterns, 2026-09-16). The repo is safe to push as it stands.
- Ollama 0.34.1 is installed. Vision-capable local models: `gemma3:4b`,
  `gemma3:12b`, `gemma4:12b`, `qwen3.5:2b/4b/9b`. Text-only: `llama3.1:8b`,
  `qwen3:14b`, `deepseek-r1:14b`, `mistral:7b`, `llama3.2:3b`.
- Hardware: Core Ultra 7 255H, Intel Arc 140T (shared memory), 31 GB RAM.
- Screenpipe relicensed to a proprietary commercial license; Piper's successor
  is GPL-3.0; Open WebUI carries a branding clause. None may be copied from.
