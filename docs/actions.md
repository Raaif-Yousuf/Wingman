# Actions

An action is a `[[actions]]` block in `actions.toml`: a name, what it looks
at, what JSON shape it asks a model for (or no model at all), and which
executor is allowed to act on the confirmed result. This is the contribution
surface CLAUDE.md means by "the action framework is the product and actions
are the contribution surface": most new capability should be one TOML block
that reuses an existing proposal schema and executor, not new Rust.

Checked against the code, not the spec: `src/actions/mod.rs` (the `Action`
model, merge and visibility rules), `src/actions/schema.rs` (the proposal
schema registry) and the five built-in actions in `src/actions/*.rs`.

## Where `actions.toml` lives

`%APPDATA%\Wingman\actions.toml`, computed by `actions::path()`. A missing
file is not an error -- it means "no overrides"; `load_actions_from` (the
path-injectable core, used by every test) merges it with the five built-in
actions from `actions::builtin_actions()` and filters to what should
actually run or be shown. Nothing writes this file today (no Settings UI
edits it yet): a user who wants to override or add an action creates or
edits it by hand. `actions.example.toml` at the repo root is a real,
loadable copy of today's five built-ins, kept in sync with `builtin_actions()`
by hand -- diff it against a fresh `builtin_actions()` output if you change a
built-in.

## Merge and visibility rules

- **Merge is by `id`, wholesale, not per-field.** A user `[[actions]]` block
  whose `id` matches a built-in replaces that built-in's entire record --
  every field comes from the user's block, not a mix of old and new. A new
  `id` is appended. (`merge_actions`, `src/actions/mod.rs`.)
- **`enabled = false` hides an action**, and so does listing its `group` in
  the top-level `disabled_groups = [...]` array. Both checks run after
  merge, so a disabled built-in whose group is later re-enabled still
  reflects any override already applied to it. (`visible`.)
- **A typo is a parse error, not a silently dropped field.** Every struct in
  this module carries `#[serde(deny_unknown_fields)]`, so a mistyped key in
  a hand-edited `actions.toml` surfaces as a named `toml` parse error instead
  of being ignored.

## The `actions.toml` schema, field by field

```toml
disabled_groups = ["Study"]        # top level, applies across all actions

[[actions]]
id       = "translate-selection"   # stable identity; merge key
name     = "Translate selection"   # shown in the (not yet built) palette
group    = "Writing"               # optional; omit for an ungrouped action
inputs   = ["selection", "screen"] # catalogue metadata -- see below
proposal = "text_answer"           # schema-registry key, actions::schema::schema_for
executor = "none"                  # executor-registry key, or "none"
confirm  = true                    # show the preview card before "Do it"?
prompt   = "Translate the selected text."
rate_difficulty = false            # verdict-only; default false
enabled  = true                    # default true

[actions.prefer]
mode = "auto"                      # only field Prefer has today
```

| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | string | yes | The merge key (see above). Also what `app.rs` looks up to run the hotkey's default action (`DEFAULT_ACTION_ID = "check-my-work"`). |
| `name` | string | yes | Display name. Not currently rendered anywhere live (the palette that will show it, #25, is not the `ui::palette` code that already exists for something else -- check the issue before assuming it is wired). |
| `group` | string, optional | no | Navigation grouping (`"Study"`, `"Work"`, `"Writing"`, ...). `None` for ungrouped; never an empty string. Drives `disabled_groups`. |
| `inputs` | array of strings | yes | One of `screen`, `window`, `region`, `selection`, `clipboard`, `text`, `uia`, `context` (`InputKind`, snake_case). **Catalogue metadata only.** Nothing on the real execution path reads this field generically yet -- each built-in action's actual data gathering (which UIA snapshot, which selection fallback, whether to screenshot at all) is hardcoded per action in `app.rs`'s worker and in the action's own module (see `actions::review_email`'s `choose_input_source`, for example). An input kind not in this list is a parse error, so a forward reference to an ungathered input kind still round-trips instead of silently vanishing. |
| `proposal` | string | yes | A lookup key into `actions::schema::schema_for` (see below), or a deliberately unregistered name for an action that never calls a model (`extract_text`'s `"ocr_text"`). `schema_for` returning `None` for a name nothing has registered yet is a load error for any action that DOES need a model, not a silent no-schema request. |
| `executor` | string | yes | A lookup key into `executors::registry::resolve`: `"none"`, `"clipboard"`, `"calendar_add"`, `"image_clipboard"`, `"replace_text"`, `"fill_form"`. An unknown name is a named load error (see [`docs/executors.md`](executors.md)), never a panic. |
| `confirm` | bool | yes | Whether "Do it" (the preview card) must be shown before the executor runs. `false` only makes sense paired with a `Effect::ReadOnly` executor -- `ui::confirm::auto_confirm_read_only` refuses to skip confirmation for anything that writes. |
| `prompt` | string | yes | The system prompt sent to the model. Empty string for an action with no model call (`extract_text`). Some built-ins (`calendar`) append request-time context (today's date, UTC offset) to this string rather than baking it in, so an `actions.toml` override replaces the base text, not the appended part. |
| `prefer.mode` | string | no, default `"auto"` | The only field `Prefer` has today. Nested rather than flattened onto `Action` because the plan's own worked examples show it as a table, and a second `prefer.*` field (a preferred model) is a plausible near-term addition. |
| `hotkey` | table, optional | no | `{ vk, ctrl, shift, alt, win }`. Parsed and round-tripped, but **inert**: nothing reads `Action::hotkey` yet (per-action hotkeys are a later phase). Do not rely on setting this doing anything today. |
| `rate_difficulty` | bool | no, default `false` | Only affects the `"verdict"` proposal kind's schema (adds a `difficulty` property and its rubric-driven enum). Accepted unconditionally on every action so a caller never has to know which proposal kinds care about it; ignored by every other schema. |
| `enabled` | bool | no, default `true` | See "Merge and visibility rules". |

Top level, alongside `[[actions]]`:

| Field | Type | Notes |
|---|---|---|
| `disabled_groups` | array of strings | Group names to hide, applied after merge (#199). |

## Two worked examples, copied from `actions.example.toml`

### The simplest: `extract-text-to-clipboard`

No model call at all -- OCR straight to the clipboard:

```toml
[[actions]]
id = "extract-text-to-clipboard"
name = "Copy text from screen"
group = "Work"
inputs = ["screen"]
proposal = "ocr_text"
executor = "clipboard"
confirm = false
prompt = ""
rate_difficulty = false
enabled = true

[actions.prefer]
mode = "auto"
```

`proposal = "ocr_text"` is deliberately unregistered (`schema_for("ocr_text",
_)` is `None` and always will be): this action's real path
(`actions::extract_text::run_pipeline`) never asks a model, so there is
nothing to build a JSON Schema for. The `Action` value above is catalogue
metadata for the future palette; the live execution path is
`ui::tray::cmd::EXTRACT_TEXT` calling `actions::extract_text` directly, not
this record. `confirm = false` matches reality: `"clipboard"` is
`Effect::ReadOnly`, so it auto-confirms.

### The two-stage shape: `fill-this-form`

```toml
[[actions]]
id = "fill-this-form"
name = "Fill this form"
group = "Work"
inputs = [
    "uia",
    "screen",
]
proposal = "form_fill"
executor = "fill_form"
confirm = true
prompt = 'You are shown a screenshot of a form and a short list of its fields Wingman could not confidently map to a profile field by label alone. For each one, decide whether a profile field (named below) is the right source, or whether you can read/infer a short, literal value directly from what is visible on screen. Never invent personal data that is not visible and not in the profile. Never fill a payment field (card number, CVV, expiry, IBAN, account or routing number) by any means; leave it as "skip". If you are unsure, choose "skip" rather than guessing.'
rate_difficulty = false
enabled = true

[actions.prefer]
mode = "auto"
```

`fill_form` (the action layer, `src/actions/fill_form.rs`, not the executor
of the same name) is the two-stage worked example, for token cost:

1. **Local, deterministic, no model call.** Every fillable UIA field is
   matched against the user's profile by label (`profile::match_label`). A
   field that matches a profile field with a non-empty value is filled
   straight from the profile, `source: "profile:<field>"`, at zero token
   cost.
2. **Only the leftovers go to a model.** Whatever `match_label` could not
   map -- or mapped to an empty profile field -- is collected into one
   completion request covering ALL of them together (never one request per
   field). If nothing is left unmapped, no request is built at all: zero
   model calls for a form the profile already covers.

The `"form_fill"` schema this second stage's completion is checked against
(`actions::schema::form_fill_schema`) is deliberately lean for the same
reason: it never asks the model to echo back a label it was already sent, or
a `target`, since the action layer already has both from the UIA snapshot.
"Review this email" (`review-this-email`, `proposal = "text_review"`,
`executor = "replace_text"`) is the other two-stage-flavoured built-in worth
reading for the same reason: it picks its input (compose box via UIA, else
the current selection, else a screenshot) before deciding whether to even
offer "Do it" at all.

## How the JSON proposal schema is declared

`actions::schema::schema_for(proposal, rate_difficulty) -> Option<Value>` is
the one registry mapping a proposal name to the JSON Schema a model's
completion must satisfy. Four kinds are registered today: `"verdict"` (today's
"Check my work"), `"calendar_event"` (#26), `"text_review"` (#38) and
`"form_fill"` (#40). Each schema function returns a `serde_json::json!`
literal shaped like standard JSON Schema (`"type": "object"`, `"properties"`,
`"required"`, `"additionalProperties": false`), plus one
non-standard keyword:

- **`"editable": true`** on a property marks it as a field the confirm card's
  preview renders as an edit control rather than plain text
  (`ui::preview::PreviewModel::from_schema` is the only reader; a provider's
  completion never sees this key echoed back). Keeping the flag on the same
  schema value sent to the provider -- instead of a parallel per-proposal
  table -- is what keeps "which fields are editable" from drifting out of
  sync with "which fields exist".

A proposal name with no registered schema returns `None`; the caller
(`provider::physics_request`, or an action's own request builder) treats
that as a load error for anything that actually needs a completion, never as
"send no schema".

## Why `serde_json` keeps `preserve_order` (CLAUDE.md rule 3)

This is the single most surprising rule in the repo, and it is not
cosmetic.

By default, `serde_json::Value::Object` is backed by a `BTreeMap`, which
serializes its keys **alphabetically** -- not in the order they were
inserted, and not in the order they appear in the Rust source that built the
`json!` literal. Every schema in `actions::schema` is written as a `json!`
literal with a specific property order chosen for a reason (see each
function's own doc comment); without `preserve_order`, that order would be
silently discarded the moment the schema is serialized to send over the
wire, and the model would always see the schema's properties sorted
alphabetically instead.

That matters because of how a JSON-Schema-constrained completion actually
gets generated: the model fills the response object's fields **in the order
the schema declares them**, effectively writing later fields conditioned on
having already committed to earlier ones. Property order is therefore not
metadata about the shape of the answer -- it is a constraint on the
*reasoning order* the model is forced through:

- `"verdict"`: `detail` is declared before `headline`. The model has to work
  the problem out and write its full step-by-step solution FIRST; only then
  does it write the one-line verdict, which is now constrained to follow
  from the working it already committed to. Reverse the order and the model
  can (and, per this repo's own testing, sometimes will) write a snap
  verdict first and then pad or contradict it with a "derivation" that
  merely restates the conclusion.
- `"calendar_event"`: `title` before `start`/`end`. The model anchors on
  *which* event this is before it has to decide *when* it happens --
  otherwise a start/end time extracted first has nothing yet to be a start
  or end time OF.
- `"text_review"`: `edits` before `verdict`. The model has to enumerate the
  concrete list of problems (if any) before it is allowed to write the
  summary judgment that is supposed to follow from that list. Reverse it and
  a model can commit to `"good_to_go"` before it has actually checked
  anything, then pad `edits` to match.
- `"form_fill"`: `control_id`, then `source`, then `profile_field`, then
  `value`, then `sensitive`, in that order. The model commits to a
  *strategy* (`source`: profile, model, or skip) before naming specifics,
  then only fills in a literal `value` when `source == "model"`, and only
  judges its own value's sensitivity once that value has actually been
  produced.

Every one of these orderings is checked by an explicit order-sensitive test
in `actions/schema.rs`'s `#[cfg(test)] mod tests` (comparing against a
`serde_json::json!` array literal, which IS order-sensitive, unlike a
`HashMap`/`BTreeMap` equality check would be) -- removing
`preserve_order` would not fail compilation, only silently break the
property that keeps a model from contradicting itself, which is why the rule
lives in `CLAUDE.md` and in a comment directly on the `serde_json` line in
`Cargo.toml` rather than only here.

## See also

- [`docs/executors.md`](executors.md): what happens to a proposal once a
  human confirms it.
- [2026-09-16 expansion plan](superpowers/specs/2026-09-16-expansion-plan-design.md)
  §6: the product-level "Confirm" and action-catalogue rules this module
  implements.
- [2026-09-17 action-model design](superpowers/specs/2026-09-17-action-model-design.md):
  the rationale for the merge/origin/visibility rules above.
- `CONTRIBUTING.md`'s "Add an action in 20 minutes": the contributor-facing
  walkthrough for adding a new action; this file is its schema reference.
