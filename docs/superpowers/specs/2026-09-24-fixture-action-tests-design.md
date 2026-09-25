# Fixture-screenshot action tests

Status: **proposed** -- needs owner approval before implementation (rule 12).

Scope: a `cargo test`-runnable harness that proves a given action's whole
pipeline (gather input -> build request -> get a model-shaped answer ->
parse into a proposal -> confirm -> execute) does the right thing for a
fixed screenshot, with no network call and no real model. Out of scope:
judging whether a *live* model's answer is good (that stays a by-hand
check, named below), the palette, the intent router, and any new executor
or proposal kind -- this issue only tests what already exists.

## Why a design doc (rule 12)

This adds a new test-only module tree (`tests/fixtures/`,
`tests/action_fixtures.rs`) and a naming/discovery convention every future
action PR is expected to follow (CONTRIBUTING.md's "Add an action in 20
minutes" step 5 currently asks only for a unit test plus a manual check;
this issue is what lets step 5 also say "and a fixture test"). Getting the
replay boundary wrong -- testing a layer so close to the real `Provider`
that it flakes, or so far from it that it stops proving anything -- is
exactly the kind of decision AGENTS.md rule 12 asks to be written down and
approved before code exists.

## Dependency on #242

Issue #242 ("generic action dispatch") is being implemented in parallel
(branch `fix/242-generic-action-dispatch`) and changes how an action goes
from "resolved `Action`" to "ran". Today (checked against `src/app.rs`,
this branch not yet merged) there is one bespoke worker function per action
-- `worker` for `check-my-work`, `calendar_worker` for `add-to-calendar`,
similar shapes inside `review_email`/`fill_form`'s own modules -- each
hand-wiring its own `physics_request`/`calendar_request`-equivalent,
its own parse function, and its own executor lookup. #242's job is to
collapse that into one function that takes a `Resolved` action and runs it
generically off `Action::proposal` and `Action::executor`.

This matters here because the harness this spec adds calls into the
pipeline **above** the per-action glue, not into `app.rs`'s per-action
worker functions directly:

```rust
pub fn run_action_fixture(
    action_id: &str,
    fixture: &ActionFixture,
) -> anyhow::Result<FixtureOutcome>
```

- **Before #242 lands**: `run_action_fixture` contains its own small
  `match action_id { "check-my-work" => ..., "add-to-calendar" => ..., ... }`
  dispatch, calling the same request-builder/parse-function pairs
  `app.rs`'s worker functions call today (`physics_request`/`parse_answer`,
  `calendar_request`/`parse_calendar_proposal`, etc.). This issue is not
  blocked on #242 and can land first.
- **After #242 lands**: that internal match is deleted and
  `run_action_fixture` calls the same generic dispatch function `app.rs`
  calls, so the fixture test starts exercising the exact production path
  instead of a parallel copy of it. A one-file, mechanical follow-up
  (tracked once #242 merges and its function signature is final, not
  written into this spec) -- not a reason to block on #242, since the
  fixture format, the fake `Provider`, and the fake executors below are
  identical either way.

A fixture test written against the pre-#242 harness needs no changes to its
`.json`/`.png` files when the internal dispatch is swapped; only
`run_action_fixture`'s body changes. That is the falsifiable part of "works
for both built-in and `actions.toml` actions" below.

## What a fixture proves, and what it deliberately does not

**Proves:** given this screenshot and this canned model response, the
action's request is built correctly, the response parses into the
documented proposal shape, and the executor produces the documented effect
on a fake target -- deterministically, offline, in well under a second.

**Does not prove:** that a real model, shown this screenshot, would
*produce* that canned response. A fixture with a hand-written "good"
response passes even if no real model on earth would ever say that. This
is the same gap `docs/actions.md`'s "verdict" schema-ordering section
already treats as separate from schema correctness -- prompt quality and
model behaviour are a product question, not a parsing question, and this
harness only ever tests parsing, dispatch and execution.

**The by-hand check that remains** (CONTRIBUTING.md step 5's existing
language, unchanged by this issue): press the hotkey with the action
selected against a real screen, using a real configured provider, and
confirm the card shows a sensible result. `needs-manual-check` per the
`working-an-issue` skill if a PR's fixture is green but this has not been
done.

## Fixture format and location

```
tests/fixtures/actions/<action-id>/<case-name>/
    screenshot.png        # exactly what capture::grab_raw would have produced
    response.json         # the canned "model" reply, see below
    expected_proposal.json
    expected_effect.json  # or expected_error.json -- see "Error fixtures"
    case.toml             # small metadata, see below
```

`<action-id>` matches `Action::id` (`"check-my-work"`,
`"add-to-calendar"`, ...). `<case-name>` is a short slug
(`"physics-correct"`, `"physics-arithmetic-error"`,
`"malformed-response"`). One directory is one test case; a contributor adds
a case by adding a directory, never by editing a shared table.

`case.toml`:

```toml
# tests/fixtures/actions/check-my-work/physics-correct/case.toml
action = "check-my-work"          # redundant with the directory name, checked
                                   # for drift by the harness (see below)
description = "A correctly solved kinematics problem; expect verdict = pass"
inputs = ["screen"]                # which InputKind values this case supplies
                                    # fixture data for (screenshot.png covers
                                    # "screen"; a future uia case would add
                                    # uia_snapshot.json alongside it)
skip_ocr = false                   # see "The non-vision fallback path" below
```

`response.json` is the exact JSON body `Chain::complete` would have
returned as `Completion.text` -- i.e. the model's raw completion string,
already schema-shaped, before `parse_answer`/`parse_calendar_proposal`/etc.
runs on it. Fixtures are the *parser's* input, not a raw HTTP response body:
retry, repair and provider-selection logic (`complete_parsed_with_fallback`,
`repair_refusal_trigger`) are covered by their own existing unit tests in
`provider/mod.rs`, not re-proven per action here.

`expected_proposal.json` is the parsed, typed value the harness asserts
`response.json` decodes into (compared field-by-field, not as opaque JSON,
so a `serde` rename or default silently changing behaviour fails the
diff). `expected_effect.json` describes what the fake executor recorded --
see "Executor without touching real windows" below.

## How the model call is replaced

A fixture never sends an HTTP request. `run_action_fixture` builds a
`Chain` from exactly one provider: a fixture-only `struct
CannedProvider { text: String }` implementing the existing `Provider`
trait's `complete(&self, _req: &Request) -> anyhow::Result<Completion>` by
returning `response.json`'s contents verbatim as `Completion.text` --
the same shape `src/provider/mod.rs`'s own `MockProvider`/`SequencedProvider`
test doubles already use for `Chain::complete_parsed_with_fallback`'s own
unit tests, just promoted from `#[cfg(test)] mod tests` to a small
`tests`-visible helper (`tests/fixtures/canned_provider.rs`) so the action
harness and `provider/mod.rs`'s own tests share one implementation instead
of two copies drifting apart.

`CannedProvider::ready()` returns `true` unconditionally and
`capabilities()` reports vision support, matching what `response.json`
implicitly assumes: a `skip_ocr = false` fixture exercises the vision path,
so the fake provider must claim vision or `complete_parsed_with_fallback`
would divert it into the non-vision branch before ever calling `complete`.

### The non-vision fallback path (`skip_ocr`)

`worker()`'s real chain runs `non_vision_inputs` (OCR + a compact UIA
snapshot) when the ready provider reports `Caps.vision == false`
(`docs/actions.md`, `complete_parsed_with_fallback`). A fixture with
`skip_ocr = true` exercises this branch by having its `CannedProvider`
report `vision: false`; the harness then calls the **real**
`ocr::recognize` against `screenshot.png` rather than faking OCR text,
because OCR is exactly the kind of pure-input-to-pure-output call a
fixture can run for real without touching a window, a clipboard, or a
network socket, and running it for real is what catches an OCR regression
a hand-typed "pretend OCR text" fixture never would.

**THEORY (unverified):** `Windows.Media.Ocr` (`src/ocr.rs`) works from a
`cargo test` binary launched under `cargo test --all-targets` in CI the
same way the 2026-09-17 executor-design doc's sibling measurement
(`ocr::has_package_identity()` returning false, `RecognizeAsync` still
succeeding, `src/ocr.rs`'s `ocr_live_recognizes_gdi_rendered_text`) showed
for a synthetic GDI-rendered image on the dev machine. That test is
`#[ignore]`d today precisely because it was not yet proven safe to run
unattended in CI; this spec's `skip_ocr = true` fixtures inherit the same
open question and MUST be run once by hand
(`cargo test --all-targets -- --ignored --nocapture` on the CI runner
image, or by adding them un-ignored and watching one real CI run) before
being merged un-`#[ignore]`d. If that measurement comes back negative,
`skip_ocr = true` fixtures fall back to a canned `OcrOutput` value instead
(the same shape `src/actions/extract_text.rs`'s existing `FakeRecognizer`
test double already injects), and this paragraph is corrected in place per
rule 10, not silently dropped.

Most fixtures will set `skip_ocr = false` and never touch this path at
all -- it exists so a `vision: false` provider (e.g. a text-only Ollama
model) is covered by at least one fixture per action that supports it, not
every fixture.

## Executor without touching real windows, clipboard or files

Every executor already documented in `docs/executors.md` is generic over an
injectable access trait for exactly this reason (`ClipboardAccess`,
`UiaAccess`/`FakeAccess` in `fill_form.rs`/`replace_text.rs`,
`FakeConverter` in `connectors/ics.rs`). The fixture harness reuses these,
never the real `ArboardClipboard`/real UIA/real filesystem writer:

```rust
pub struct FixtureExecutorEnv {
    pub clipboard: RecordingClipboard,   // wraps executors::clipboard's FakeClipboard
    pub uia: FixtureUiaAccess,           // wraps fill_form/replace_text's FakeAccess,
                                          // pre-seeded from case.toml's [uia] table
                                          // when the case supplies one
    pub ics_writer: RecordingIcsWriter,  // wraps connectors::ics's FakeConverter,
                                          // captures the .ics bytes instead of writing
                                          // them to a real path
}
```

`run_action_fixture` resolves the executor via
`actions::resolve_executor(action)` exactly like production code, then
constructs it with `FixtureExecutorEnv`'s fakes instead of the executor's
own `::new()` default (every executor already exposes a
`with_*`/generic-over-trait constructor per `docs/executors.md`'s "The
executor contract", so this needs zero new constructor surface). This
satisfies AGENTS.md rule 9 by construction: no fixture test can name or
touch `Wingman`'s real registry paths, mutex, window classes or the real
system clipboard, because the fakes never call into `windows`/`arboard` at
all.

`expected_effect.json` is checked against whatever the relevant
`Recording*` fake exposes after `execute` returns -- for
`RecordingClipboard`, the text it was asked to set and the "previous
value" it recorded for undo; for `RecordingIcsWriter`, the `.ics` body's
parsed fields (start, end, title), not a byte-exact file compare, since
line-ending/`X-WR-TIMEZONE`-ordering churn in the ICS writer is not this
harness's concern. Confirmation is exercised the same way production code
exercises it: a fixture whose action has `confirm = false` calls
`ui::confirm::auto_confirm_read_only`; one with `confirm = true` calls
`ui::confirm::user_confirmed()` then `ui::confirm::confirm` (both already
`pub(crate)`, reachable from `tests/` since integration tests link the
crate, not from outside it) -- never a fabricated `Confirmed` value, so
this harness cannot itself become the thing that widens the privacy
boundary the executor-design spec's compile-fail gate protects.

### Error fixtures

A case directory may supply `expected_error.json` instead of
`expected_proposal.json`/`expected_effect.json`:
`{ "stage": "parse", "contains": "missing field `headline`" }`. `stage` is
one of `parse` (canned response fails to decode), `confirm` (a `confirm =
true` action's fixture tries to auto-confirm, or a `Writes` executor's
fixture supplies no confirmation at all), or `execute` (a well-formed
proposal that the executor itself rejects, e.g. a `fill-this-form` fixture
naming a payment-shaped field). `contains` is matched as a substring of the
error's `Display` (`{e:#}`), the same shape `docs/executors.md`'s own
error-text guarantees are checked by today. This is what proves a
malformed fixture -- or, more usefully, a malformed real-world model
response someone pastes into a new fixture after seeing it in the wild --
produces the documented card text, not a panic (rule 7).

## How a contributor adds a fixture for a new action, in minutes

1. `mkdir tests/fixtures/actions/<action-id>/<case-name>/`.
2. Drop in a screenshot (a real one from the app, or a small synthetic PNG
   -- nothing requires it be real; `docs/actions.md`'s catalogue actions
   already vary on how literally they read the screen).
3. Write `response.json` by hand, matching the proposal schema
   `actions::schema::schema_for` returns for that action's `proposal`
   field (or copy-paste a real completion captured once by hand and
   trimmed).
4. Write `expected_proposal.json` and `expected_effect.json` (or
   `expected_error.json`).
5. `cargo test --test action_fixtures <action-id>` runs just that action's
   cases; the harness auto-discovers every directory under
   `tests/fixtures/actions/<action-id>/`, so no test function or table
   entry needs to be added by hand -- this is the single biggest lever for
   "in minutes": a fixture is data, not code, and the loop is edit-JSON,
   `cargo test`, done.

A drift guard test (`fixture_case_toml_matches_directory`) fails the whole
suite if a `case.toml`'s `action` field disagrees with its parent
directory's action-id segment, or if `action_id` names an action that
resolves to nothing (built-ins plus the case's own `actions.toml` snippet,
next paragraph) -- a mismatch fails loudly, not silently.

### Actions-only-in-`actions.toml`

To prove a fixture works for an action that exists only in a contributor's
`actions.toml` (never a built-in), a case directory may include its own
`actions.toml` snippet (`case.toml`'s `[actions_toml]` inline string, or an
`actions_override.toml` file alongside `case.toml`); `run_action_fixture`
then calls `actions::load_actions_from` merging that snippet over
`actions::builtin_actions()` instead of the crate's real
`actions::load_actions()`, exactly mirroring how `actions::mod`'s own tests
already avoid the real `%APPDATA%` path (rule 9). This is what makes "works
for built-in and `actions.toml` actions" a single code path rather than
two: a built-in-only fixture simply omits `[actions_toml]` and gets
`builtin_actions()` unmodified.

## What CI runs, and the cost/RAM limits

`cargo test --all-targets` (the existing CI step in `.github/workflows/ci.yml`)
already runs every `tests/*.rs` integration test binary; `action_fixtures`
needs no new CI job, only a new test binary CI already picks up for free.
Each fixture case is a plain `#[test]` (generated at build time by a
`build.rs`-free `include!`-based directory walk, or, more simply, one
`#[test]` per discovered directory via a `walkdir`-equivalent helper run
inside a single `#[test] fn all_action_fixtures()` that iterates and
asserts per case with `eprintln!`-tagged sub-failures -- final choice left
to the implementer, since it is a test-authoring detail, not a design
decision this spec needs to pin) -- no network, no provider API key, no
spawned process, no polling (rule 5 is about the shipped app, not test
code, but the same "nothing idle" instinct applies: a fixture test that
sleeps or retries is a fixture that flakes).

**RAM**: fixture PNGs are kept small on purpose -- a cropped region around
the relevant UI, not a full 4K monitor grab, per `filing-findings`'s
build-machine RAM guidance for anything that shares a machine with a
concurrent `cargo build`. **THEORY (unverified):** a full test run over the
fixture set (expected to start around 5-10 cases, one to three per action)
completes in low single-digit seconds, matching `docs/actions.md`'s
existing characterization of the crate's whole `cargo test` run as "a few
seconds here" -- to be confirmed against the actual wall-clock time once
the first fixtures exist, and corrected here if wrong.

**Cost**: exactly zero -- this is the issue title's whole point. No
fixture test holds or requires an API key; `CannedProvider` never dials
out, and CI's existing `cargo test --all-targets` step sets no provider
secrets today and does not need to gain any for this harness to run.

## Falsifiable acceptance criteria

1. `tests/fixtures/actions/check-my-work/` has at least two cases (a
   passing verdict, a failing verdict) and `cargo test --test
   action_fixtures check-my-work` exits 0 asserting both proposals and, for
   any case with `confirm = false`, the executor effect.
2. Corrupting any one byte of a fixture's `response.json` so it no longer
   parses (e.g. deleting a required key) makes that case's own test fail
   with an assertion naming the missing field, not a panic and not an
   unrelated case going red -- proving each case is isolated and the
   parser path is actually exercised, not skipped.
3. Deleting `screenshot.png` from a fixture case (leaving `case.toml` and
   the JSON files) fails that one case with a named "fixture missing
   screenshot.png" error at test-collection time, not a silent pass and
   not a build failure elsewhere.
4. A new fixture directory added with no code change (no new `#[test] fn`,
   no table edit) is picked up by the next `cargo test --test
   action_fixtures` run -- proven by adding a throwaway case in review,
   observing the test count increase, then deleting it.
5. Running the full fixture suite touches no real Win32 clipboard, no real
   window, no real file outside `target/`, and makes zero outbound network
   connections -- checked once by hand (the same class of check
   `docs/offline.md`'s Wireshark-based criterion uses for offline mode) and
   re-stated in the PR that implements this, not re-verified per CI run.
6. At least one `skip_ocr = true` fixture exists and passes, with the
   `THEORY (unverified)` paragraph above either confirmed (and rewritten as
   `MEASURED`) or replaced with the `FakeRecognizer` fallback before that
   fixture is merged un-`#[ignore]`d.

## What stays out of scope

- No fixture proves a real model would produce a given response; that
  remains the by-hand check named above, unchanged by this issue.
- No fixture drives the actual confirm-card UI (`ui::card`) -- it calls
  `ui::confirm`'s functions directly, the same boundary
  `on_calendar_result`/the review/fill-form flows already sit behind in
  `app.rs`. A UI-rendering regression (wrong preview layout, say) is not
  caught here; that is `docs/executors.md`'s and the palette issue's
  territory.
- No fixture exists yet for an action whose input gathering is not yet
  generic (per `docs/actions.md`'s `inputs` field note, most built-ins
  hardcode their own gathering); a fixture only ever supplies
  `screenshot.png` (+ optional `[uia]`/`actions_override.toml`), never a
  live selection or live clipboard snapshot, so an action whose real
  pipeline depends on those still needs its existing bespoke unit tests
  alongside any fixture this harness adds.
- The `run_action_fixture`-calls-#242's-generic-dispatch follow-up
  described above is intentionally not designed here; it is a small,
  mechanical change once #242's actual function signature exists on
  `master`.

## Open questions for the owner

- Whether the pre-#242 internal `match` in `run_action_fixture` is worth
  writing at all, or whether this issue should simply wait for #242 to
  merge first and call its dispatch function from day one (trades "lands
  sooner" against "briefly duplicates per-action glue that gets deleted a
  few days later").
- Whether `skip_ocr = true` fixtures should ship un-`#[ignore]`d in the
  first PR (pending the CI-runner OCR measurement in "The non-vision
  fallback path"), or start `#[ignore]`d like `ocr_live_recognizes_gdi_rendered_text`
  until that measurement exists, with a follow-up issue to un-ignore them.
- Whether `expected_effect.json`'s ICS comparison (parsed fields, not
  byte-exact) is precise enough, or the owner wants a stricter byte compare
  for at least one canonical calendar fixture.
