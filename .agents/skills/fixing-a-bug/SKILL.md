---
name: fixing-a-bug
description: Use when fixing any bug, regression, or defect in this repo, before writing any fix code. Also use when a fix "should work" but the symptom persists, when the suite is green while the reported behaviour is still wrong, or when adding a guard or parser.
---

# Fixing a bug

A green test suite is not evidence. The sibling CLAIR repo shipped at least
eighteen features that compiled, ran, passed their tests and did nothing, tests
that asserted the bug as intended behaviour, and mutation arms that stayed
green because no test could see the fix. Every lesson below was paid for there.

The order exists because each step catches a failure the next step cannot.

## The order (do not reorder)

1. **Write the failing test FIRST, at the symptom level.**
2. **Watch it fail**, and read the failure message.
3. **Broaden it into a matrix.**
4. **Mutation-check: break the code on purpose** and confirm the tests go red.
5. **Narrow the cause**: whole repo to a few files to roughly 100 lines.
6. **Fix it.**
7. **Revert-check**: undo the fix, confirm the new tests fail, restore.
8. **Full green**: `cargo test`, `cargo clippy --all-targets -- -D warnings`,
   `cargo fmt --check`.

## 1. Failing test first, at the SYMPTOM level

Write the test from the reported observable, not from your theory of the cause.
Not knowing the cause is an advantage: a test written from the symptom tests
behaviour; a test written after you find the cause tends to test your fix.

State the symptom as an assertion a user would recognise:

- "a pasted key is truncated to the field's visible width"
- "Codex Haiku 4.5 returns 400 and the card says `Bad response`"
- "the second launch opens Settings instead of asking"

**If you cannot write a failing test from the symptom, that IS the finding.**
Win32 behaviour often cannot be reached from `cargo test`. Say so, name the
manual reproduction, and add it to `NEXT_SESSION.md`. Then narrow first and
come back. Do not skip to fixing because the test was awkward.

## 2. Watch it fail, and READ the failure

Not "it errored". The message must describe the real defect. A test that fails
with a panic on your own fixture is not yet a test of anything.

**Doubt the harness before you doubt the product.** In the sibling repo, three
"bugs" in one day were the console mangling a print, a fixture stripped before
the code under test ran, and a placeholder string the harness itself wrote.
Check the value as stored, not as displayed.

## 3. Broaden into a matrix

One reproduction is an anecdote. Sweep the dimensions the bug lives in and
assert against the stated rule, not against current behaviour. Here the axes
are usually: each provider, each model family (thinking and not, vision and
not), each `effort`, empty key versus bad key, light and dark theme, each DPI,
Offline mode versus not.

## 4. Mutation-check: break the code on purpose

**Mandatory.** Before trusting any test or guard, damage the production code
and confirm the test goes red with a message that names the problem. Then
restore. For a parser or guard with several arms, prove every arm fires. An arm
that stays green is the finding, not a pass.

**Assert the VALUE, never mere presence.** `body.contains("model")` passes
against any request. `body["model"] == "gemma4:12b"` does not.

**A green check you never saw go red is a claim about the harness, not about
the bug.**

## 5. Narrow the cause

Only now go looking. Useful moves:

- `git log -S'<literal>'` to find when a value changed.
- Compare the failing input against the nearest passing one.
- Check the pair, not each half: a request builder and a response parser can
  each be correct and disagree with each other.
- **Run it, do not read it.** Careful reading produced confident false leads
  in the sibling repo; pushing a real input through the real path found seven
  bugs in a day. Here that means: run the exe, press the key, read the card,
  and where a provider is involved, read the recorded fixture against the
  live response.

## 6. Fix it

Fix the cause you narrowed to, not the symptom. If the issue body proposes a
fix, verify the diagnosis yourself first: issue bodies routinely carry a wrong
fix.

Hard Rule 10: every causal claim carries `MEASURED <date>:` plus the
observation, or `THEORY (unverified):`.

**If an existing test goes red against your fix, the TEST may be the bug.**
When a test's assertion is the reported symptom, replace it in place and say so
in the commit.

**Before you write a new helper, check whether it already exists.** Grep for
the concept's vocabulary, not the function name you were about to type. One
concept with two implementations, one of them wrong, is the shape to avoid.

## 7. Revert-check

Undo the fix. Run the new tests. **They must fail.** Restore. Use a patch file,
never `git stash` (the hook blocks it anyway):

```
git diff > "$SCRATCH/fix.patch"
git checkout -- <the SOURCE files only>
cargo test <name>          # must go RED
git apply "$SCRATCH/fix.patch"
git diff --stat            # prove byte-exact
```

An assertion that passes both ways is a property invariant, not a regression
guard.

## 8. Green

`cargo test`, then clippy with `-D warnings`, then `cargo fmt --check`. All
three, every time.

## Before you say it is done

Name the one observable that would differ if your change were wired to
nothing, and go check it (`wired-to-nothing`). A thing you can see: a rendered
card, a registry value, a process that exits, a request body in a log.

## Red flags: stop and go back a step

- "The test passes, so it works" (did you break the code and watch it fail?)
- "I traced every call site" (that is not the check)
- "It is a one-line change" (the one-line change went inside a comment once)
- "The issue says the cause is X" (verify it)
- "I could not reproduce it, but the fix is obviously right"
- "I will write the test after the fix" (you will test your fix, not the bug)

## Recurring bug shapes worth recognising

- **A hand-maintained list.** A guard that checks "every X is covered" cannot
  see the X nobody added to its list. Here: model lists in `config.rs`
  defaults, provider names in `order`, difficulty anchors in prompt and parser.
  Widen the predicate, never narrow the scope.
- **The gate you ran versus the gate that ships.** `cargo test` runs unpackaged
  with your environment. The user runs a signed exe launched by the shell with
  no arguments and no env vars. Name what differs before trusting green.
- **A message that reports its branch's INTENT, not the OUTCOME.** Derive any
  string asserting an outcome from what actually happened (what the API
  returned, what was written), never from the branch that decided to act.
- **Copied posture.** Copying a sibling call site copies timeouts and retry
  counts calibrated for a different consequence.
