---
name: tests-first
description: Use before writing ANY feature, fix, or behaviour change in this repo. Owner rule (2026-08-28, carried over from the sibling CLAIR repo): write the failing tests first, prove they fail, add the neighbouring tests, then write the code, then prove they pass. Also use when a fix "should work" but nothing proves it.
---

# Tests first

Owner rule, verbatim: "Before writing a feature or big fix, first we write
tests, then make sure they fail. Then write similar tests, then we fix the bug
or write the code, then check if the tests pass."

## The loop, in order

1. **Name the observable.** One sentence: what differs when the change is
   wired to nothing? (See `wired-to-nothing`.) The first test asserts that.
   For Win32 code that cannot be unit-tested, the observable is a manual
   check, and you write it down before writing the code.
2. **Write the failing test(s).** Pure logic lives next to the code in a
   `#[cfg(test)] mod tests`; recorded API responses go in `tests/fixtures/`.
   No network in tests. No production names for kernel objects, registry
   values or paths (Hard Rule 9).
3. **Run them and paste the failure.** `cargo test <name> -- --nocapture`. A
   test that passes before the code exists is asserting the bug (MEASURED
   three times in the sibling repo); rewrite it.
4. **Write the neighbouring tests.** The same shape on the adjacent input:
   the empty case, the malformed case, the value at the boundary (a `u8`
   difficulty of 0, 11, and an overflowing string), the provider that is not
   `ready()`, the refusal stop reason, the response with the field missing.
5. **Write the code.** Smallest change that turns the tests green.
6. **Run the targeted tests again and paste the pass.** Then the whole crate:
   `cargo test` is a few seconds here, so there is no reason not to. Then
   `cargo clippy --all-targets -- -D warnings` and `cargo fmt --check`.
7. **Report** with both outputs (red, then green), verbatim.

## Non-negotiables

- No code before step 3's red output exists in your transcript.
- "I will add tests after" is the failure mode this skill exists to stop.
- A guard, allowlist or parser gets a test that plants the violation and sees
  it caught, plus one proving it accepts at least one real input. Every
  assertion checks the VALUE: `assert!(body.contains("effort"))` passed in the
  sibling repo with the production argument deleted, because the stub's own
  default wrote the same key.
- A mutation arm that stays green is a coverage hole, not a pass: break the
  production code on purpose once and watch the new test go red.
- Hard Rule 10: a mechanism you did not observe is `THEORY (unverified):`.
