# Multi-step actions: a short typed pipeline behind one press

Status: **Draft, awaiting approval** (rule 12). Issue #1279. No code is written
until this is approved. First user: [text-first input](2026-11-01-text-first-input-design.md) (#1280).

## Why

Today one press makes one action call. Around it sit the router
(`src/router.rs`) and a one-time repair call on a parse failure
(`repair_request` in `src/provider/mod.rs`). Some jobs do better as a short chain:

- **Text, then image** (#1280): try cheap OCR text, send the screenshot only if needed.
- **Extract, then verify**: `check-my-work` gets an independent second look before a "wrong" verdict is shown.
- **Plan, then fill**: a long form is split into groups of fields, each filled from the screen and knowledge, one combined preview at the end.

Each of these is hand-written glue in `app.rs` today or would be. This spec gives
them one small, testable shape, without becoming a chat, an agent loop or a
second place that can act.

## Rules the design keeps (from #1279 and AGENTS.md)

1. One press, one final card. Steps are never shown as a conversation.
2. Pending status shows "2 of 3" and the running cost. A key press cancels.
3. Hard caps on steps and on spend per press, in Settings.
4. The model only fills typed proposals. The executor runs last, after Confirm. Nothing is pressed or sent for the user.
5. Every step is unit-testable from fixtures, no model call (#369, `2026-09-24-fixture-action-tests-design.md`).
6. Every failure ends in a card (rule 7). Never a dialog, never a silent no-op.
7. Nothing runs while idle (rule 5). A chain is a worker-thread loop that ends; no timer, no watcher.

## What exists today (checked against master 9313467)

- `Chain::complete_parsed_with_fallback_attributed` runs one `Request` through the providers, with per-provider vision fallback (`non_vision_request`) and one repair call. `Request` is always one system, one user, zero or more images, an optional schema. A `Provider` never sees history.
- `usage::note_run_cost` and `usage::run_cost` keep a per-run total in a thread-local (`RUN_COST`). `Attribution.cost_usd` carries it to the card. `usage::check_budget` and `provider::common::budget_guard` enforce the daily `[budget] daily_usd` inside `post_json_with`, the only billed send path.
- `egress::record` writes one `EgressEntry` per HTTP attempt: provider, model, attachments (kind and size, never content), cost, trigger, redaction counts.
- `ui::pending_status::PendingStage` is plain data with `pending_status_text`; `result_is_current(posted_run, current_run)` in `app.rs` drops a result from a stale run.
- The generic action path is `App::run_generic_action_with`, which reads `Action::inputs`, `proposal`, `executor`.
- A chain deadline exists (`Chain::with_deadline`, #792): past it no new call starts and no repair is sent.

THEORY (unverified): the `Chain` seam is the right layer to leave alone, and the pipeline sits above it. The reason is that every guarantee (budget guard, send preview, redaction at the HTTP boundary, egress log) already lives at or below `Chain`, so a step that calls `Chain` inherits all of them for free.

## Data types

New module `src/pipeline/` (`mod.rs`, `limits.rs`, `run.rs`). Plain data first, so the
logic is testable without Win32 or a model.

```rust
/// What an action declares. Built-in pipelines are Rust values; a user
/// actions.toml entry can only pick a named built-in shape (see Out of scope).
pub struct Pipeline {
    pub id: &'static str,            // "text_then_image", "extract_verify", "plan_fill"
    pub max_steps: u8,               // the pipeline's own ceiling, <= the Settings cap
}

/// One step. A step is a pure function of what came before plus one optional
/// model call. It never acts.
pub trait Step {
    fn name(&self) -> &'static str;              // for logs and the pending text
    /// Build the request, or decide to skip the model entirely.
    fn plan(&self, ctx: &StepCtx) -> StepPlan;
    /// Turn the model's text into a typed result. Pure, fixture-testable.
    fn parse(&self, ctx: &StepCtx, completion_text: &str) -> Result<StepOutput, StepError>;
}

pub enum StepPlan {
    Call(Request),                   // one model call through Chain
    Local(StepOutput),               // no model needed (cache, OCR coverage says text is enough)
}

pub enum StepOutput {
    Next(Carry),                     // data for the next step (typed, see below)
    Done(Proposal),                  // the final typed proposal for Confirm
}

/// What flows between steps. Typed, small, bounded. Never raw chat history.
pub struct Carry { pub fields: serde_json::Map<String, Value>, pub bytes: usize }

pub enum StepError {
    Parse(String),                   // model text did not fit the step's schema
    Declined(String),                // vendor refusal, ends the chain (#767)
    CapReached(CapKind),             // steps or spend
    Cancelled,
    Other(String),
}
```

`Proposal` is the existing proposal type from the schema registry
(`src/actions/schema.rs`); this spec adds no new proposal kind. `Carry` is capped
at `MAX_CARRY_BYTES` (starting point 16 KiB, THEORY unverified: enough for a
field list or an extracted text, small enough that step 2 never pays for a
runaway step 1). A step that would exceed it fails with a card, it is not
truncated silently.

`StepCtx` holds: the press inputs (`Gathered`, screenshot, OCR output, cached
once per press and shared), the `Carry` from the previous step, the step index,
and a read-only view of the limits. It holds no `Chain`, so a step cannot send
anything itself.

```rust
pub struct Limits {
    pub max_steps: u8,               // Settings, default 3, clamp 1..=6
    pub max_cost_usd: f64,           // Settings, default 0.05, 0 = no per-press cap
}
```

Defaults are a starting point, THEORY (unverified): 3 steps covers every example in
#1279; $0.05 is about 25 typical single calls at the "About $0.002" scale the card
already shows. Both get measured after the first user ships.

## The runner

`pipeline::run(pipeline, steps, ctx, chain, limits, cancel, sink) -> PipelineResult`
runs on the existing worker thread, in place of the single
`complete_parsed_with_fallback` call inside `run_generic_action_with`. An action
with no pipeline is exactly the one-step pipeline it is today, so existing actions
do not change behaviour and the existing tests keep passing unchanged.

Loop, in words:

1. If `cancel` is set, return `Cancelled`.
2. If `steps_done == min(pipeline.max_steps, limits.max_steps)`, return `CapReached(Steps)`.
3. If a per-press cost cap is set and the running cost is at or over it, return `CapReached(Spend)`.
4. `sink.stage(index, total, running_cost)` posts the pending update (see UI below).
5. `step.plan(ctx)`. For `Local`, take the output with no call. For `Call`, run it through `chain.complete_parsed_with_fallback_attributed` with `step.parse` as the parser.
6. Add the step's cost to the running total. Record the step (see egress and usage).
7. `Next(carry)`: loop. `Done(proposal)`: return it. An error ends the run with a card.

`Chain` is unchanged. The repair call stays inside `Chain` and counts as part of its
step, not as an extra step. It is billed and logged like any call, so the running
cost includes it.

THEORY (unverified): total step count shown as "2 of 3" is the pipeline's declared
maximum, and an early `Done` simply ends sooner. This avoids a counter that jumps.
For a pipeline whose second step is optional (text then image) the text reads
"Reading the screen" then "Asking the model", not a fixed "of N" (see Open questions).

## Where it plugs in

- **Worker thread only.** Today `begin_model_action` (`app.rs`) captures on the main thread, then spawns a worker that posts a `Result`. The runner is a function the worker calls. The main thread gets one new message kind, `WM_APP_PIPELINE_STAGE`, carrying `(run, index, total, cost_micros)`, handled by the same `stage_is_current(posted_run, current_run)` check as the existing stage updates, so a late stage from a cancelled run is dropped.
- **Capture stays where it is.** The screenshot and the OCR pass are made once, before the worker starts, as today (the redaction stage already runs OCR on cloud requests and the spec there says the pass is paid once and reused). Steps read them from `StepCtx`; no step captures.
- **Final result path is unchanged.** `PipelineResult::Done(proposal)` goes down the same road as a one-call result today: options card, preview, Confirm, executor. A pipeline cannot reach an executor. Only `app.rs`'s existing confirm path can.
- **Where the action says so.** `Action` gets one optional field, `pipeline: Option<String>` (a built-in shape id, `deny_unknown_fields` still on, unknown id is a load error naming the field, same as a bad `InputKind`). Default `None`.

## Pending status

`PendingStage` gets one variant:

```rust
Step { index: u8, total: u8, label: &'static str, cost: Option<String> }
```

Text (no em dashes): `Step 2 of 3: Checking the answer. About $0.004 so far.` The cost part
uses `usage::format_cost`, and is left out when nothing was billed (local model) or
any request was unpriced, the same rule `run_cost()` already has ("never
`Some(0.0)`"). Pure text function, unit-tested like `pending_status_text`.

## Cancellation

A key press while a chain runs sets an `AtomicBool` the runner checks between
steps and the chain checks before each provider attempt (the `with_deadline` check
is the model for it: `past_deadline` already skips the next call and the repair).

- Between steps: immediate.
- During a call: the call finishes (blocking `ureq`, no abort handle; this is today's behaviour for a single call too). Its result is dropped, and its cost is still recorded because it was billed. THEORY (unverified): lowering the HTTP timeout on cancel is not worth the plumbing; revisit if a cancelled press feels slow.
- The card for a cancelled run: none. Nothing was done, so there is nothing to say (owner decision 2026-09-24, no Cancel when nothing was done, #392). This is not a silent no-op in the rule 7 sense: the user did the cancelling.

**Dependency.** The pending card today promises no cancel (`pending_status.rs` module doc: #324 and #393 are open, no working "press the key again" route exists). The runner's cancel flag is built here, but the user-facing route to set it is #324. This spec does not redo #324. Until #324 lands, only the caps stop a chain. Because of that, the pipeline work must not ship a multi-call default before #324 ships, or must ship with a low default cap (see Open questions).

## Budget accounting per step

- **Per-press cap** (new): the runner checks the running total before each step. A call is not started when the cap is already reached. It cannot stop a single step that costs more than what is left; to bound that, each step's `Request.max_tokens` is set so the worst case fits under the remaining cap when the model's price is known (`usage::price_for`). When the price is unknown, the cap cannot be enforced in dollars, so the step cap applies and the pending text shows no cost. THEORY (unverified): most users are on priced models; measure how often `price_for` returns `None` among configured models.
- **Daily budget** (existing): untouched. `budget_guard` still runs inside `post_json_with` for every step, so a chain cannot slip past the daily limit. A step refused by it ends the run with the existing budget card text.
- **Running total**: `usage::note_run_cost` is already called per request and the thread-local lives for the worker thread, so the runner reads `usage::run_cost()` rather than keeping a second counter.
- **Local models** are free and never count toward a dollar cap, but they do count toward the step cap.

## Egress logging per step

Every model call already writes an `EgressEntry` per HTTP attempt, so per-call logging is free. What is missing is **which step**, so a reader of the log can tell a two-call press from two presses. Add two optional fields to `EgressEntry` (serde defaults, old rows still parse, the same pattern as `trigger` and `redactions`):

```rust
#[serde(default)] pub step: Option<StepTag>,   // { pipeline: String, index: u8, name: String }
#[serde(default)] pub input: Option<String>,   // "image" | "text" | "both" (used by #1280)
```

`render_human` shows `step 2 of 3 (verify)` when set. No prompt text, no answers, only the tag. A `Local` step makes no request, so it writes no egress row (nothing left the machine); the usage page shows it from the run record below.

The step tag travels the same way `trigger` does today; implementation issue 3 checks how and follows it.

## Usage records

`usage::record` is per request and stays that way, so totals stay right. A new in-memory `RunRecord { steps: Vec<{name, local, input, cost}> }` is kept for the card footer and for the diagnostics bundle; it is not persisted. Persisting per-step rows in the local store (`2026-10-15-local-store-design.md`) is deferred (see Out of scope), because #1280's "Done when" needs a measurement and a diagnostic line is enough for that.

## Failure ends in a card, per step

| Failure | Card says | Notes |
|---|---|---|
| Step N model call fails (all providers) | `Step 2 of 3 failed: <existing provider error>.` | The provider error text is the one `Chain` already builds. |
| Parse error after the repair call | Same, naming the step | Repair stays inside the step. |
| Vendor decline | The existing decline card | Ends the chain (#767). |
| Step cap reached | `Stopped after 3 steps (your limit). Nothing was done.` | |
| Spend cap reached | `Stopped at $0.05 (your limit). Nothing was done.` | Names the cost so far. |
| Daily budget | The existing budget card | |
| Carry too big | `The first step's result was too large to continue.` | |
| Panic in a step | The worker's existing catch-all card | The worker already catches everything (rule 7). |
| Cancelled | No card | See Cancellation. |

If an earlier step already produced something useful (for example step 1 of text-then-image succeeded in extracting text but step 2 failed), the card still reports the failure. It does not show the half result as if it were the answer.

## Testing without a model

- Pure units: `Limits` clamping, the cap checks, `StepCtx` carry-size rule, the pending text.
- Each `Step::parse` is a pure function, tested against `response.json` fixtures in `tests/fixtures/actions/<action-id>/<case>/`.
- The runner is tested with the harness' `CannedProvider` from `2026-09-24-fixture-action-tests-design.md`, extended to a **`SequencedProvider`** returning one canned text per call, in order, and recording each `Request` it was given. That gives: "step 2's request contains step 1's carry and not step 1's image", "a third call would panic" (the style `repair_is_attempted_exactly_once_a_third_call_would_panic` already uses), "a cancel flag set after step 1 makes no second call", "cost cap hit makes no further call".
- Fixture layout gets one optional file, `responses/01.json`, `02.json`, for a multi-step case; the single `response.json` stays valid and means a one-step pipeline.
- Status of #369: open. The runner needs the fixture harness to land first or in the same stretch; see implementation order.

## Wired-to-nothing check (rule 8)

The observable that differs if the runner is wired to nothing: a two-step action's second request appears in the egress log with `step 2 of 2`, and the pending card text changes from `Step 1 of 2` to `Step 2 of 2` while it runs. Checked by hand with a local Ollama model (free) and a fixture-gated test action.

## Out of scope

- Loops, branches, and any step the model chooses at runtime. The shape is a fixed list per pipeline, declared in Rust.
- User-authored pipelines in `actions.toml`. A user action can only name a built-in pipeline id. Revisit after at least two built-in pipelines exist.
- Tool use, function calling, or letting a step run an executor. Executors run once, after Confirm.
- Chat, follow-ups, showing step results as messages (owner decision 2026-09-16).
- Parallel steps. Everything is sequential on one worker thread.
- A new proposal kind. Persisting per-step rows in the local store. Routing a different model per step (that is #102, a later field on the step).
- Streaming. Still whole results only.

## Open questions, with advice

1. **Is "N of M" shown when a step is optional?** Advice: show `Step 1 of 2` only when the pipeline is fixed length. For text-then-image show named stages with no total: `Reading the screen`, then `Asking the model`, then `Looking at the picture` only if step 2 happens. The cost still shows. This avoids a counter that lies.
2. **Default caps.** Advice: 3 steps and $0.05 per press, both lowest sensible, shown in Settings under Models with plain wording. Raising is one click; a surprise bill is not undoable.
3. **Ship before #324 (key press cancels)?** Advice: no multi-call default before #324. #1280 can ship first because its worst case is two calls, and the second is only ever the screenshot the user would have paid for anyway.
4. **Cancel during a call.** Advice: finish the call, drop the result, still record the cost. Do not add abort plumbing in v1.
5. **Cap in dollars when a model has no price.** Advice: the step cap is the only guard there; show no cost, and say so in the Settings help text ("Spend limits need a priced model").
6. **Should the verify step of `check-my-work` default on?** Advice: off by default behind its own action option, since it doubles cost for the common "your answer is right" press. Decide after #1280 has real numbers.
7. **Where does the cap setting live?** Advice: `[budget]` next to `daily_usd`: `max_steps` and `per_press_usd`, with env override like the other settings, and the config drift test (`config_example.rs`) updated.

## Implementation issues to open after approval

In order. Each is small, test-first, and lands alone.

1. Fixture harness first: #369, extended with `SequencedProvider` and numbered `responses/`.
2. `src/pipeline/` types, `Limits`, the runner and its cap and cancel logic, pure and tested with `SequencedProvider`. No UI, no action uses it.
3. `EgressEntry` `step` and `input` fields, `render_human`, the diagnostics line. Old rows still parse (test with a stored old-format row).
4. `PendingStage::Step`, its text, `WM_APP_PIPELINE_STAGE` and the `stage_is_current` guard. Wired-to-nothing check by hand.
5. Settings: `[budget] max_steps`, `per_press_usd`, the Settings controls, the config example drift test, validation and clamping.
6. `Action::pipeline` field, load error for unknown id, `actions.example.toml` regenerated, `run_generic_action_with` calls the runner (one-step pipeline for every existing action, no behaviour change, existing tests untouched).
7. Cancel flag hookup, after #324 lands. Without #324 the flag has no setter.
8. First user: #1280 (see its spec).
9. Later, each its own spec-light issue: `check-my-work` verify step, plan-then-fill for `fill_form`.
