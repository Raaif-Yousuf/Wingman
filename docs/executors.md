# Executors

An executor is deterministic Rust that turns a confirmed proposal into a
real effect: writing a calendar file, replacing text in a UIA control,
filling a form, copying to the clipboard. It is the "Do" in **Look, Propose,
Confirm, Do** -- the model never acts directly; it only fills a typed
proposal that a human then confirms, and only THEN does an executor run.

Checked against the code, not the spec: `src/executors/mod.rs` (the
`Executor` trait, `Effect`, `Undo`), `src/executors/registry.rs`,
`src/ui/confirm.rs` (the `Confirmed<P>` boundary) and every
`src/executors/*.rs` file.

## The executor contract

```rust
pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    fn effect(&self) -> Effect;
    fn execute(&self, confirmed: Confirmed<serde_json::Value>) -> anyhow::Result<Undo>;
}
```

- **`name`** is the id `actions.toml`'s `executor` field and
  `executors::registry::resolve` both use; it also appears in error and
  card text, so it must read sensibly there.
- **`effect`** is `Effect::ReadOnly` or `Effect::Writes`. This is the ONE
  thing that gates whether an action may skip the confirm card:
  `ui::confirm::auto_confirm_read_only` refuses to auto-confirm anything
  that is not `ReadOnly`. Declaring `ReadOnly` on an executor that writes
  anything a user did not already have would silently defeat the confirm
  step for every action that uses it -- there is no test that catches a
  mismatch between what an executor actually does and what it claims here,
  so get it right by inspection, the same way `wired-to-nothing` asks you to
  check any other silent-no-op shape.
- **`execute`** always takes `Confirmed<serde_json::Value>` -- not generic
  over a proposal type, so `Executor` stays object-safe
  (`Box<dyn Executor>` in the registry). `serde_json::Value` is the same
  currency `actions::schema`'s proposal registry already speaks, so this is
  the natural common wire format, not a second one. `execute` consumes the
  `Confirmed` by value: there is no way to call it twice on the same
  confirmation, and no path to re-derive one from something other than the
  card the user actually saw.

`Undo` is what `execute` returns on success:

```rust
pub struct Undo {
    pub summary: String,
    restore: Box<dyn FnOnce() -> anyhow::Result<()> + Send>, // private
}
```

`Undo::none(summary)` is for an executor with nothing to restore (nothing
was written). `Undo::recording(summary, restore)` pairs a human-readable
description with a closure that puts things back. `summary` describes what
*actually happened*, built from the executor's own return value -- never
from what the proposal merely intended -- and it is written in plain,
no-em-dash prose (rule 11): it is the text a result card shows.

## Resolving an executor by name

```rust
// src/executors/registry.rs
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Executor>>
```

A plain `match` over the known names -- `"none"`, `"clipboard"`,
`"calendar_add"`, `"image_clipboard"`, `"replace_text"`, `"fill_form"` --
each constructing a fresh boxed executor. An unknown name (a typo in
`actions.toml`, or an action naming an executor nobody has written yet) is a
named `anyhow` error, never a panic (rule 7): "No executor named
\"<name>\". Check the action's executor field in actions.toml." is the exact
text that ends up on an error card. `actions::resolve_executor(&action)` is
the one call site that turns `Action::executor` (a plain, unvalidated-until-now
string) into this lookup.

## The `Confirmed<P>` boundary, and why an executor cannot fabricate one

`execute` only ever receives a `Confirmed<serde_json::Value>` -- never a
`Proposal` or a raw `Value` -- and `Confirmed<P>` (`src/ui/confirm.rs`) is
built so that **nothing outside that one file can construct one**:

```rust
pub struct Confirmed<P> {
    value: P,   // no `pub` -- this is the whole boundary
}
```

Rust field privacy is scoped to the defining module and its descendants, not
to the whole crate. `src/executors/*.rs` is a *sibling* of `src/ui/confirm.rs`,
not a descendant of it, so writing `Confirmed { value: x }` from any
executor is `error[E0451]: field \`value\` of struct
\`ui::confirm::Confirmed\` is private` -- the same error an entirely
separate crate would get. The only two functions anywhere in the crate that
can write that field are declared in `ui::confirm` itself:

- **`confirm(proposal, token)`** -- `pub(crate)`, and it requires a
  `ConfirmationToken`, a private-field newtype whose only constructor,
  `user_confirmed()`, is the stand-in for the confirm card's "Do it"/Enter
  handler. `pub(crate)` on `confirm`/`user_confirmed` widens *who may call
  them*, never who may write the private field directly -- an executor can
  call `confirm`, but only by already holding a `ConfirmationToken`, which
  it has no way to construct itself.
- **`auto_confirm_read_only(executor, proposal)`** -- the one sanctioned
  exception, for a `Effect::ReadOnly` executor whose action has
  `confirm = false` (no preview shown at all). It refuses outright if the
  executor is not `ReadOnly`.

The practical upshot: an executor's `execute` body can read a confirmed
value (`confirmed.value()` / `confirmed.into_value()`, both public
accessors) but there is no expression an executor can write that produces a
NEW `Confirmed<P>` from scratch. It can only ever have been handed one by
the confirm card or by `auto_confirm_read_only`.

**This boundary is now covered by an automated compile-fail gate**
(`tests/compile_fail.rs`, issue #203), not only the doc-comment example on
`Confirmed` itself: `cargo test compile_fail` fails outright if the field or
its module's constructors are ever widened enough for a sibling module to
write it directly. Before that gate existed, this was checked only by a
one-time manual `rustc` compile and by tests proving the sanctioned path
works, neither of which would have caught the boundary being *widened*.

## The stale-target check

`replace_text` and `fill_form` both act on a UIA element identified at
"Look" time (a [`TargetRef`], `src/executors/target.rs`: window handle,
runtime id, automation id, name, control type) and re-resolved fresh at "Do"
time -- never a live COM pointer held across the gap between showing the
preview and the user clicking "Do it". Re-resolution can fail two distinct
ways, and both are a refusal, never a silent write:

1. **Not found, or ambiguous.** A fresh walk of the window's descendants
   (`executors::target::resolve_index`) finds zero or more than one exact
   match for the captured identity. More than one match is never guessed at
   -- the executor bails with a named error either way.
2. **Stale text.** `executors::target::is_stale(expected, actual)` compares
   the element's live current text against what the preview card showed at
   "Look" time. If they differ -- the user (or something else) typed into
   the field between Look and Do -- the write is refused rather than
   clobbering whatever is there now.

`Undo` for these two executors re-resolves the SAME way and applies the SAME
staleness check before restoring: it refuses to overwrite a field that has
changed again since the executor itself wrote it, and reports that refusal
rather than forcing the old value back over a newer edit.

## The absolute rule: never Send, Submit, Buy or Pay

No executor may invoke, or even target, a control that reads as a
final-action button. This is checked in two independent places:

- **By control type.** `executors::target::is_invokable_control_type`
  (`Button`, `Hyperlink`, `MenuItem`, `SplitButton`) is refused outright by
  `fill_form` regardless of the control's label -- an innocuous-looking
  "Continue" or "Next" button is still never treated as a form field.
- **By name/automation id.** `executors::uia_guard::is_forbidden_target`
  checks a candidate element's Name AND AutomationId against a deny-list
  covering English (`send`, `submit`, `buy`, `pay`, `place order`,
  `checkout`, `check out`) plus localized German/French/Spanish/
  Portuguese/Italian/Dutch equivalents (#204), matched whole-word
  (single terms) or as a substring (phrases), case- and
  accent-insensitive. Both `replace_text` and `fill_form` call this as a
  second line of defense on top of their own targeting logic.

Payment data gets the same absolute treatment from the other direction:
`fill_form` refuses any field whose label reads as payment-shaped (card
number, CVV/CVC, expiry, IBAN, account/routing number --
`crate::payment_denylist`, the one shared term table both this check and the
profile's own save/load-time guard read from) regardless of where the value
would have come from. The user profile has no payment fields to source one
from in the first place; this check exists for a value a provider might
still propose by label alone.

## The four rules every executor obeys

(2026-09-16 expansion plan §6, "The rules every executor obeys"; see
`CONTRIBUTING.md`'s "If the action needs a new executor" for the
contributor-facing phrasing this section matches.) A pull request adding an
executor is reviewed against these explicitly:

1. **Takes a `Confirmed<Proposal>`, never anything else.** The type system
   enforces this (see "The `Confirmed<P>` boundary" above) -- there is no
   way to call `execute` on an unconfirmed value.
2. **Does exactly what the confirmed preview showed.** Nothing is
   re-generated or re-interpreted at execution time; `execute` parses the
   confirmed JSON `Value` into a typed proposal once, on its first line, and
   never touches the raw `Value` again (every executor in this crate follows
   this shape -- see `calendar_add::execute`'s or `replace_text`'s own doc
   comments for the same statement made file-locally).
3. **Returns an outcome describing what actually happened, with an undo
   where the platform allows one.** `Undo::summary` is built from the
   executor's real return value (a written file's path, a field actually
   set), never from what the proposal merely intended -- and is honest when
   undo is incomplete (`calendar_add`'s summary says plainly that undo
   deletes the generated `.ics` file but cannot remove the event from
   whatever calendar app opened it).
4. **Never presses Send, Submit, Buy or Pay, and never touches payment
   data.** See the section above; this is the one rule with no exceptions
   and no configuration to turn it off.

## See also

- [`docs/actions.md`](actions.md): the proposal side of the loop -- what an
  action asks a model for, before any of this runs.
- [2026-09-17 executor design](superpowers/specs/2026-09-17-executor-design.md):
  the full rationale for the `Proposal`/`Confirmed` type-state split, the
  registry shape, and the MEASURED privacy-boundary compile.
- [2026-09-16 expansion plan](superpowers/specs/2026-09-16-expansion-plan-design.md)
  §6: the product-level rules this module implements.
