<!--
Thanks for contributing to Wingman. Fill in what applies; delete a section
only if it truly does not apply (e.g. "Spec link" for a one-line typo fix)
and say why in its place.
-->

## What this changes and why

## Spec link

<!--
AGENTS.md rule 12: anything architectural (a new module, a changed thread
responsibility, a changed stored-file format) needs a design spec in
docs/superpowers/specs/YYYY-MM-DD-<topic>-design.md, written and approved
before the implementation. Link it here. If this change is not architectural
(a bug fix, a new action that only adds a TOML entry and reuses an existing
executor, docs), say "not architectural" and why.
-->

## Tests: red, then green

<!--
tests-first (this repo's owner rule): the failing test existed before the
fix, and you watched it fail, before writing the fix. Paste the command and
the failing output, then the same command passing. Win32 code that isn't
meaningfully unit-testable skips this section in favor of the "Observable
checked" section below.
-->

- [ ] Test(s) written first, shown failing, then shown passing (or: this
      change has no unit-testable logic; see "Observable checked" below)

## Docs

- [ ] Any doc this change makes stale is updated in this PR (README.md,
      docs/*, AGENTS.md, or none needed)

## `cargo deny check`

- [ ] `cargo deny check --all-features` is green (paste the output if it
      flagged and you resolved something, e.g. a new dependency's license)

## Observable checked (wired-to-nothing)

<!--
AGENTS.md rule 8 / the wired-to-nothing skill: state the one observable that
would differ if this change were wired to nothing, and say you looked at it.
A hook installed on a thread with no message loop, a PostMessage to a window
that was never created, a menu item with no WM_COMMAND arm all compile, pass
tests, and do nothing -- this is the check that catches that.
-->

**The observable:**

**Checked by:** <!-- what you did to look at it, and what you saw -->

## Anything else the reviewer should know
