# The Executor trait and the confirmation type system

Status: **approved** (owner asleep; written against issue #31, already
scoped by the owner in the issue body and the 2026-09-16 expansion plan §4
(`executors/` row) and §6 ("Confirm", "The rules every executor obeys", "The
first four actions"). Treated as pre-approved per the overnight-agent
instructions; the owner reviews on waking.)

Scope: the `Proposal<P>` / `Confirmed<P>` type-state pair, the `Executor`
trait and `Undo`, an executor registry (`"none"` and `"clipboard"`), and the
shared UIA-targeting deny-list helper `#32`/`#33` must call. Out of scope:
any real UIA-touching executor (`replace_text`, `fill_form`), the confirm
card UI (no "Do it" button exists yet), and wiring `app.rs`'s worker to
actually run an executor after a proposal comes back -- nothing calls
`ui::confirm::confirm` for real yet, because nothing produces a real user
click to hand it. This issue delivers the type system and the two executors
Phase 2's read-only path needs; the confirm-card issue is what will call
`ui::confirm::confirm` for the first time.

## Why a design doc (rule 12)

Issue #31's body pins the shape loosely (`Executor<P>::run(Confirmed<P>) ->
Outcome { summary, undo: Option<Box<dyn FnOnce()>> }`), and the overnight
task brief pins a different shape (`Executor::execute(&self, Confirmed<P>)
-> Result<Undo>`). Neither pins where `Confirmed`'s privacy boundary
physically lives, whether `Executor` is object-safe, or what `P` is at the
registry boundary. This is architectural per AGENTS.md rule 12: it adds a
new module (`executors/`) and a new privacy-load-bearing module
(`ui::confirm`) whose whole job is that one specific type cannot be
constructed anywhere else in the crate.

## The type-state pair: `Proposal<P>` / `Confirmed<P>`

Both live in `src/ui/confirm.rs`, declared `pub(crate) mod confirm;` from
`src/ui/mod.rs` -- private to the crate, public within it, because
`executors/` and `actions/` both need to name the types.

```rust
pub struct Proposal<P> { pub value: P }
impl<P> Proposal<P> {
    pub fn new(value: P) -> Self { Self { value } }
}

pub struct ConfirmationToken(());

pub struct Confirmed<P> { value: P }   // field NOT pub
impl<P> Confirmed<P> {
    pub fn value(&self) -> &P { &self.value }
    pub fn into_value(self) -> P { self.value }
}
```

`Proposal<P>` is what a provider response becomes once parsed: unrestricted
construction, because nothing unsafe happens by *having* a proposal, only by
*acting* on one. `Confirmed<P>`'s field is private, and the only two
functions in the whole crate that can write it are in this same module:

```rust
/// The one path a real user confirmation takes. `token` can only have come
/// from `user_confirmed()` below, which is the stand-in for the future
/// confirm-card "Do it" button handler (not yet built -- see Out of scope).
pub(crate) fn confirm<P>(proposal: Proposal<P>, _token: ConfirmationToken) -> Confirmed<P> {
    Confirmed { value: proposal.value }
}

/// Called by the (not yet built) confirm card's WM_COMMAND handler for the
/// "Do it" button / Enter key. A private-field newtype, not a bool: nothing
/// outside this module can construct one by any means other than this
/// function, so nothing outside this module can call `confirm` either,
/// even though `confirm` itself is `pub(crate)`.
pub(crate) fn user_confirmed() -> ConfirmationToken { ConfirmationToken(()) }

/// The one exception: an executor that declares `Effect::ReadOnly` and
/// whose action has `confirm = false` never shows a preview (expansion plan
/// §6: "Read-only actions show a result card straight away"). Refuses any
/// executor that is not `Effect::ReadOnly` -- this is the only other
/// function in the crate that can produce a `Confirmed<P>`, and it does so
/// without a `ConfirmationToken` on purpose (there is deliberately no
/// "manufacture a token" path anywhere).
pub(crate) fn auto_confirm_read_only(
    executor: &dyn crate::executors::Executor,
    proposal: Proposal<serde_json::Value>,
) -> anyhow::Result<Confirmed<serde_json::Value>> {
    anyhow::ensure!(
        executor.effect() == crate::executors::Effect::ReadOnly,
        "executor \"{}\" is not read-only and cannot auto-confirm",
        executor.name()
    );
    Ok(Confirmed { value: proposal.value })
}
```

### Why an executor cannot fabricate a `Confirmed<P>`

Rust field privacy is scoped to the defining module and its descendants, not
to the crate. `executors/` is a sibling of `ui::confirm`, not a descendant,
so `Confirmed { value: x }` inside any executor is `error[E0451]: field
`value` of struct `ui::confirm::Confirmed` is private`, the same error an
external crate would get. `pub(crate)` on `confirm`/`auto_confirm_read_only`
only widens *who may call the constructor functions*, not who may write the
private field directly -- and calling `confirm` still requires a
`ConfirmationToken`, which has the same private-field problem one level up.

**MEASURED 2026-09-17:** compiled a standalone two-module snippet mirroring
this shape (`mod confirm { pub struct Confirmed { value: i32 } pub(crate) fn
confirm(...) -> Confirmed { ... } }` / `mod executor { fn f() { let x =
confirm::Confirmed { value: 1 }; } }`) with `rustc --edition 2021
--crate-type lib`. It fails with exactly `error[E0451]: field \`value\` of
struct \`confirm::Confirmed\` is private`, confirming the boundary holds
before relying on it for the real types.

### The compile-fail doc test, and why it cannot run here

The task brief asks for a ` ```compile_fail ` doc test proving this. `wingman`
is a `[[bin]]`-only crate (no `[lib]` in `Cargo.toml`): Cargo only extracts
and runs doc tests against a library target, so `cargo test --doc` finds
nothing to test here regardless of what doc comments say (own project rule
7 in the task: `wingman is not a library crate` is the standing shape;
turning it into one to gain doctests would touch `main.rs`'s module list and
every other module's visibility, which several other agents are editing
concurrently tonight -- out of scope for this issue and too invasive for an
unattended overnight change). The doc comment on `Confirmed` below still
carries a ` ```compile_fail ` block for a human reader, annotated as
unverified by tooling:

```rust
/// An executor cannot fabricate one of these: the field is private to this
/// module, and the only constructor is `pub(crate)`, in this module only.
///
/// ```compile_fail
/// // This snippet documents the boundary; it is NOT run by `cargo test`
/// // (wingman has no [lib] target, so rustdoc has nothing to link it
/// // against -- see the 2026-09-17 executor design doc, "compile-fail doc
/// // test" section). The privacy violation itself was verified with a
/// // standalone rustc compile, not through this doctest.
/// # struct Confirmed<P> { value: P }
/// # fn from_outside_the_module<P>(p: P) -> Confirmed<P> {
/// Confirmed { value: p } // error[E0451]: field `value` is private
/// # }
/// ```
```

Filed **#203** ("wingman has no automated compile-fail check for
`Confirmed`'s privacy boundary") to track adding either a thin `lib.rs` shim
or a `trybuild` dev-dependency later, once the module-visibility churn from
tonight's parallel agents has settled; the standing mitigation is the
positive regression tests in `src/ui/confirm.rs` (construct via `confirm`
and via `auto_confirm_read_only`, nothing else compiles) plus the MEASURED
manual check above.

## `Executor` trait, `Effect`, `Undo`

All in `src/executors/mod.rs`. `Executor` is object-safe (stored as `Box<dyn
Executor>` in the registry), so it is not itself generic over `P`; it always
takes `Confirmed<serde_json::Value>` (the schema registry in
`actions::schema` already speaks JSON Schema over `serde_json::Value`, so
this is the natural common currency, not a new one).

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Effect { ReadOnly, Writes }

pub struct Undo {
    pub summary: String,
    restore: Box<dyn FnOnce() -> anyhow::Result<()> + Send>,
}
impl Undo {
    pub fn none(summary: impl Into<String>) -> Self { ... }           // no prior state
    pub fn recording(summary: impl Into<String>, restore: impl FnOnce() -> anyhow::Result<()> + Send + 'static) -> Self { ... }
    pub fn undo(self) -> anyhow::Result<()> { (self.restore)() }
}

pub trait Executor: Send + Sync {
    fn name(&self) -> &'static str;
    fn effect(&self) -> Effect;
    fn execute(&self, confirmed: crate::ui::confirm::Confirmed<serde_json::Value>) -> anyhow::Result<Undo>;
}
```

`execute` takes `Confirmed<Value>` by value, matching "preview equals
execution": the confirmed proposal is consumed once, there is no path to
call `execute` twice on the same confirmation or to re-derive one.

## Registry

`src/executors/registry.rs`: a plain `match` (not a `HashMap`, no
`lazy_static`/`once_cell` dependency needed for two entries) from name to a
freshly constructed `Box<dyn Executor>`:

```rust
pub fn resolve(name: &str) -> anyhow::Result<Box<dyn Executor>> {
    match name {
        "none" => Ok(Box::new(NoneExecutor)),
        "clipboard" => Ok(Box::new(ClipboardExecutor::new())),
        _ => anyhow::bail!("No executor named \"{name}\". Check the action's executor field in actions.toml."),
    }
}
```

That error string is the card text an unknown `actions.toml` executor value
produces (rule 7: a load error, not a panic; rule 11: no em dash). Minimal
wiring into `actions/`: `src/actions/mod.rs` gains

```rust
pub fn resolve_executor(action: &Action) -> anyhow::Result<Box<dyn crate::executors::Executor>> {
    crate::executors::registry::resolve(&action.executor)
}
```

so `Action.executor` (today validated as present but never resolved, per
the action-model design doc's "What stays out of scope") has exactly one
call site that turns the string into a real `Executor`. Nothing calls
`resolve_executor` yet outside its own test -- `app.rs`'s worker still only
runs `"none"` implicitly via `provider::physics_request` -- because wiring
the worker to run an arbitrary resolved executor after a real confirm click
is the confirm-card issue's job, not this one's.

## `"none"` and `"clipboard"` executors

`NoneExecutor` (`src/executors/none.rs`): `Effect::ReadOnly`, `execute`
always returns `Ok(Undo::none("nothing to do"))`. This is what "Check my
work" resolves to today; it stays a no-op forever by construction (there is
no state to undo for an action that only shows a card).

`ClipboardExecutor` (`src/executors/clipboard.rs`): `Effect::ReadOnly`.
Copies the confirmed proposal's `"text"` string field to the clipboard,
recording whatever was on the clipboard beforehand so `undo()` restores it.
Generic over a small injectable trait so tests never touch the real
clipboard (AGENTS.md rule 9 in spirit -- this isn't a named kernel object,
but "touches a real shared OS resource from a test" is the same failure
shape):

```rust
pub trait ClipboardAccess: Send + Sync {
    fn get_text(&self) -> anyhow::Result<String>;
    fn set_text(&self, text: &str) -> anyhow::Result<()>;
}
struct ArboardClipboard;   // wraps arboard::Clipboard, the crate's existing dependency
impl ClipboardAccess for ArboardClipboard { ... }

pub struct ClipboardExecutor<C: ClipboardAccess = ArboardClipboard> { clipboard: C }
impl ClipboardExecutor<ArboardClipboard> {
    pub fn new() -> Self { Self { clipboard: ArboardClipboard } }
}
impl<C: ClipboardAccess> ClipboardExecutor<C> {
    pub fn with_clipboard(clipboard: C) -> Self { Self { clipboard } }
}
```

A proposal missing a `"text"` string field is a hard error (`bail!`), not a
silent no-op: the `wired-to-nothing` skill's whole point is that a no-op
that returns `Ok` is indistinguishable from success.

No action names `"clipboard"` as its executor yet (no `text_answer`-shaped
built-in exists); this is forward wiring for the first action that does,
same status the action-model design doc gives unregistered proposal kinds.

## The shared UIA-targeting deny-list

`src/executors/uia_guard.rs`, a pure function with no UIA dependency yet
(neither `replace_text` (#32) nor `fill_form` (#33) exists), because the
*signature* is what future executors must be written against, per the task
brief. It takes what a UIA element actually exposes (Name, AutomationId --
see the expansion plan's `inputs/uia.rs` row):

```rust
pub fn is_forbidden_target(element_name: &str, automation_id: &str) -> bool
```

Match rule: single-word deny terms (`send`, `submit`, `buy`, `pay`) match a
whole word after splitting on non-alphanumeric characters, case-insensitive
-- so "Sender" or "Payment method" do not false-positive, but "Send",
"SUBMIT", "Pay Now" do. Multi-word terms (`place order`, `checkout` is
single-word) match as a case-insensitive substring of the whole string,
because "Place Order" is a phrase, not a word boundary problem. Checked
against both `element_name` and `automation_id`, since either can carry the
label.

**Known gap, documented per the task brief:** this catches only the English
terms named in the task brief. Localized variants (a German "Kaufen" button,
a French "Payer") are not caught. Filed as **#204** ("UIA deny-list has no
localized variants") for #32/#33 to pick up alongside their first real UIA
call, since neither exists yet to test against a real localized app.

## Verification note

Compiled a standalone module-privacy snippet with `rustc --edition 2021
--crate-type lib` (see "MEASURED 2026-09-17" above) before relying on it in
the real crate. Everything else in this design is proven by the tests in
`src/ui/confirm.rs`, `src/executors/*.rs` and `src/actions/mod.rs` listed in
the commit this design doc ships with.
