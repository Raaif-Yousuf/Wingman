---
name: filing-findings
description: Use when you notice a bug, a suspected bug, a code-quality problem or an improvement while working in this repo, before filing a GitHub issue about it. Also use when running an audit of a module, and when an agent shares a build machine with other agents (the RAM rules for cargo live here).
---

# Filing findings

A finding nobody can act on is noise. A finding filed twice is worse. Every
issue this repo files must let a stranger reproduce or reject it in minutes.

## 1. Search before filing

```
gh issue list --state all --search "<two or three distinctive words>" --limit 20
```

Search by the symptom and by the function name. If it exists, add a comment
with your new evidence instead of a new issue.

## 2. Classify honestly (CLAUDE.md rule 10)

| You have | Labels | Title starts with |
|---|---|---|
| Observed it: a failing test, a reproduced run | `bug` + area + priority | the symptom |
| Read it in the code, not observed | `bug`, `needs-repro` + area + priority | the symptom |
| Structure, duplication, missing tests, no user-visible bug | `tech-debt` + area + priority | the change |
| New capability or a better behaviour | `enhancement` + area + priority | the capability |

Areas: `area:core ui inputs providers actions executors connectors awareness
packaging docs ci`. Priority: `P1` breaks the core loop, loses data, leaks a
secret or drains battery while idle; `P2` wrong but survivable; `P3` polish.

## 3. The body

```
**Where:** `src/file.rs:123` (`function_name`), at commit <short sha>
**What happens:** <observed, or THEORY (unverified): expected from reading>
**Why it matters:** <user-visible effect>
**Evidence:** <test output, the exact lines, or the reasoning chain>
**Suggested fix:** <one or two sentences; optional>

**Done when:** <one observable check, a test name or a manual step>
```

Line numbers drift; always name the function too. Never paste secrets,
`config.toml` contents or API responses containing keys.

## 4. Enhancements must fit the product

Wingman is one press, one action, one card. **No chat, no follow-ups, no
streaming.** Frame additions as an action (a proposal schema plus an
executor) or as making the existing loop faster, safer or cheaper. Never
propose something that presses Send, Submit, Buy or Pay.

## 5. Shared-machine cargo rules (several agents build at once)

Unbounded parallel builds have frozen this machine by exhausting RAM.

- **Never run bare `cargo test`** or `cargo test --release`. Run the tests
  you touched: `cargo test <module_or_test_name>`. The orchestrator runs
  the full suite. `scripts/hooks/block_unfiltered_cargo_test.py` enforces
  this: it denies any `cargo test` invocation with no test-name filter. The
  orchestrator's full-suite run sets `WINGMAN_FULL_SUITE=1` on the command
  to bypass the check; that override is not for individual agents to use.
- In a worktree, use **your own** target dir plus the shared sccache, cap
  jobs, and drop debug info (the linker is the RAM peak, and debug info is
  most of what it holds):

  ```
  export CARGO_TARGET_DIR=C:/Users/raaif/copilot-ask/target/wt/$(basename "$PWD")
  export RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=1
  export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_TEST_DEBUG=0
  ```

  `CARGO_BUILD_JOBS=2` is fine when at most five agents share the machine;
  use `1` from six agents up. MEASURED 2026-09-16: five unbounded parallel
  builds exhausted 31 GB and froze the machine.
  Do not share one target dir between worktrees. MEASURED 2026-09-16: with a
  shared dir, a filtered run in one worktree did not list tests that existed
  in its source until a `touch` forced a rebuild. THEORY (unverified): cargo
  hashes a workspace member by its workspace-relative path, so every worktree
  of this crate writes the same test binary. sccache shares the dependency
  compiles safely across target dirs.
- "Blocking waiting for file lock" is expected. Wait; do not delete locks.
- No `cargo build --release` unless the task is about the release binary.
- master is `cargo fmt` clean (since #161): run `cargo fmt --all` in your
  worktree before committing; it only reformats what you changed.
- The orchestrator's merge gate is `scripts/verify-all.sh` (fmt, clippy, full
  suite, deny, hook tests, Pester, PSScriptAnalyzer; one line per step).
  It runs the full suite, so agents do not run it.
