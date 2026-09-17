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

Carried over from the packaging work: the owner may still owe the one
Settings click that points the Copilot key at the app (Settings ▸ Bluetooth
& devices ▸ Keyboard ▸ Customize Copilot key ▸ Custom). Ask before assuming
the picker route is live; the hook route works regardless.

- **#18/#206 non-vision fallback, end-to-end through a real key press**
  (overnight agent session, 2026-09-17, branch `worktree-agent-a6dd7c51956060b22`).
  What was built and verified without the exe: `provider::Chain::complete_parsed_with_fallback`
  + `provider::non_vision_request` (unit-tested: golden request bodies,
  laziness, truncation, OCR-failure skip semantics -- see
  `src/provider/mod.rs`'s new tests) and `app.rs`'s `worker`/`non_vision_inputs`
  wiring (compiles, clippy/fmt clean, all existing `app::`/`provider::`/`ocr`
  tests still pass). The live `#[ignore]`d check
  `provider::tests::non_vision_fallback_live_ocr_to_text_only_ollama_answers_arithmetic`
  ran for real against this machine's Ollama: a GDI-rendered "What is 17 + 25
  ?" image, OCR'd, answered by `llama3.2:3b` (text-only, no vision) through
  the real chain, MEASURED 2026-09-17 elapsed 11.9s, headline `"42"`, model
  confirmed unloaded afterwards (`/api/ps` empty).
  **Not yet checked**: the actual Copilot-key path through `App::ask` --
  `GetForegroundWindow()` captured before `show_pending()`, the UIA snapshot
  running on its own spawned thread, and `worker()` actually reaching
  `non_vision_inputs`. Could not run (no live desktop / exe launch in this
  session; CLAUDE.md rule 8's "checked by hand" observable). To verify: set
  `providers.order = ["ollama"]` with `[providers.ollama]` `model =
  "llama3.2:3b"` (or another text-only model), put a readable arithmetic
  problem on screen, press the Copilot key, and confirm the card shows an
  answer (not "no providers configured", not a blank/error card) -- that is
  the one observable that would differ if this wiring were wired to nothing.

- **#62/#36 DPAPI cross-account unreadability** (overnight agent session,
  2026-09-17, branch `worktree-agent-a16f5b1a39aed9da0`, commits `ed264fb`
  `src/dpapi.rs` and `ae9c104` `src/profile/`). What was built and verified
  in this session: `dpapi::protect`/`unprotect` round-trip through the real
  `CryptProtectData`/`CryptUnprotectData` (17 tests, `cargo test dpapi`,
  including tamper and wrong/missing entropy failure cases, verified by a
  temporary mutation that made the relevant tests go red then reverted);
  `profile::Profile::save_to`/`load_from` round-trip through that envelope
  at a scratch path, and `saved_file_is_not_plaintext_json` confirms the
  on-disk bytes are neither the plaintext values nor the plaintext JSON
  field names (49 tests, `cargo test profile`).
  **Not yet checked**: actual cross-account unreadability -- this sandbox
  has one Windows account, so "the database file read from another account
  yields no plaintext" (#62's and #36's own wording) rests on DPAPI's
  documented user-scoped contract, not a run against a second account. To
  verify: on a machine with a second local Windows account, have this
  build's user account call `Profile::save_to` (or `dpapi::protect`
  directly) to write a file with a known plaintext marker string, log in as
  the second account, and confirm `CryptUnprotectData` (or
  `Profile::load_from`) fails against that file rather than returning the
  marker -- that is the one observable that would differ if the "user
  scoped" claim were wrong.

## Facts established this session, not derivable from the code

- Git history contains no key-shaped strings (`git log -p --all` grep for
  `sk-` patterns, 2026-09-16). The repo is safe to push as it stands.
- Ollama 0.34.1 is installed. Vision-capable local models: `gemma3:4b`,
  `gemma3:12b`, `gemma4:12b`, `qwen3.5:2b/4b/9b`. Text-only: `llama3.1:8b`,
  `qwen3:14b`, `deepseek-r1:14b`, `mistral:7b`, `llama3.2:3b`.
- Hardware: Core Ultra 7 255H, Intel Arc 140T (shared memory), 31 GB RAM.
- Screenpipe relicensed to a proprietary commercial license; Piper's successor
  is GPL-3.0; Open WebUI carries a branding clause. None may be copied from.
