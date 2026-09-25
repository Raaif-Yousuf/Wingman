//! "Review this email" (#38): the second action to run the full **Look,
//! Propose, Confirm, Do** loop, and the first one whose "Do" step writes
//! text into an arbitrary UIA-editable control instead of opening a
//! generated file (`calendar_add`'s shape). Mirrors `actions::calendar`'s
//! split -- built-in action definition, prompt, a `parse_*` from raw
//! provider JSON, and a pure model of the flow's states -- plus two pieces
//! calendar's flow didn't need: a deterministic edit-application algorithm
//! (`apply_edits`) and an input-source decision (`choose_input_source`),
//! both required by #38's task brief and both pure, so both are proven with
//! plain unit tests, no Win32 involved.
//!
//! Kept as its own file (not folded into `actions::mod`, which other
//! overnight agents are editing concurrently for their own new built-in
//! actions) so this feature lands as one new file plus the smallest
//! possible diff to `actions::mod`, `actions::schema`, `app.rs` and
//! `ui::tray`.
//!
//! # Input source and "Do it"
//!
//! #38's task brief: "the focused compose body via UIA TextPattern (capture
//! a TargetRef for it), else the current selection, else the screen
//! (read-only then: no Do it)." This module resolves that priority as:
//!
//! 1. **[`InputSource::ComposeBody`]**: the desktop's UIA-focused element,
//!    when it is not a password field and supports both `ValuePattern`
//!    (needed later to write the edited text back -- `replace_text`'s own
//!    write path, `src/executors/replace_text.rs`) and `TextPattern` (this
//!    module's chosen signal for "a genuine, possibly multi-line text body",
//!    not e.g. a checkbox or a button that merely has a name). Its full
//!    `ValuePattern.Value` -- not just whatever is selected inside it -- is
//!    the text sent for review, and a [`CapturedTarget`] (mirroring
//!    `replace_text::TargetRef`'s identity fields byte for byte) is
//!    captured for it.
//! 2. **[`InputSource::Selection`]**: falls back to
//!    `inputs::selection::get_selection_foreground`, the crate's existing,
//!    already-tested selection capture (UIA `TextPattern.GetSelection`,
//!    else a clipboard-safe synthetic Ctrl+C). This gives text only, no
//!    element identity -- there is nothing here to re-resolve at "Do" time.
//! 3. **[`InputSource::Screen`]**: a screenshot, exactly like
//!    `actions::calendar`'s flow.
//!
//! [`source_has_target`] says which *category* of source can ever carry a
//! target: `ComposeBody` always does; `Screen` never does; `Selection`
//! *can*, but does not always (#219) -- see [`CapturedInput::Selection`]'s
//! doc comment. `app.rs`'s actual "show Do it or not" gate therefore checks
//! [`ReviewOutcome::target`] itself (`Some`/`None`), not
//! `source_has_target` alone; `source_has_target` remains useful as the
//! pure, testable "this category is even eligible" classification.
//! `Selection` (when no target was captured) and `Screen` both end in an
//! informational card -- honest about what Wingman can act on rather than
//! offering a button that would fail every time it was pressed (rule 7).
//!
//! # Deterministic edit application
//!
//! [`apply_edits`] is the executor-facing half of "Do it": given the
//! captured full text and the model's `edits` (each an exact `before`
//! string, an `after` replacement, and a `reason`), it produces the new
//! full text by substring replacement, **in the order the model listed the
//! edits**, never by re-deriving anything from the model again. Two rules
//! from the task brief, both load-bearing for repeated or overlapping
//! `before` text:
//!
//! - Each edit searches the CURRENT text starting only from just after the
//!   position the *previous* edit's replacement ended (`search_from`) --
//!   never from the start of the string again. A `before` string that
//!   occurs more than once therefore always matches its first still-unused
//!   occurrence, and an edit whose target text was already consumed by an
//!   earlier edit (two edits that overlap) correctly fails to find it again
//!   rather than accidentally matching some unrelated later occurrence.
//! - An edit whose `before` cannot be found (from `search_from` onward) is
//!   dropped: it has no effect on the text, and it is reported back
//!   ([`ApplyEditsResult::dropped`]) rather than silently ignored, so the
//!   caller can say so.

use serde_json::Value;

use super::{Action, InputKind, Prefer};

/// This action's id.
pub const ACTION_ID: &str = "review-this-email";

/// The base system prompt, before any per-request text is appended (unlike
/// `actions::calendar::build_prompt`, nothing here varies per request --
/// there is no date/offset to bake in -- so [`builtin_action`] uses this
/// directly as `Action::prompt`, and callers needing the full text just use
/// the resolved action's `prompt` field as-is).
pub const BASE_PROMPT: &str = "You are shown the text of an email the user is writing (or, if no compose box could be read directly, a screenshot of one). Proofread it: fix spelling and grammar mistakes, and flag anything that reads as unintentionally harsh or unclear in tone. For each individual problem, propose one edit: the EXACT original text to find (\"before\"), what it should become (\"after\"), and a short reason. Keep each edit as small and specific as possible -- one edit per individual mistake (e.g. one misspelled word), never one edit spanning an entire sentence or paragraph just because it contains more than one mistake. Only propose an edit for a real problem -- never rewrite text that is already fine just to have something to say, and never propose an edit whose \"after\" is identical to its \"before\": if there is nothing to change, do not propose that edit at all. If nothing needs fixing, set \"verdict\" to \"good_to_go\" and leave \"edits\" empty. Otherwise set \"verdict\" to \"needs_edits\". Separately, note the overall tone in one short sentence (\"tone_note\"), and set \"missing_attachment\" to true only when the text itself mentions an attachment (e.g. \"see attached\", \"I've attached\") since you cannot see whether a real attachment is present -- this is a heuristic on the wording alone, not a real attachment check. Never suggest sending, submitting or otherwise finishing the email; only propose edits to its text. Use plain text only in every field: no markdown (no asterisks, backticks, headers or bullet characters), no LaTeX, and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// The built-in "Review this email" action (#38): group Writing, input
/// priority Uia (compose body) then Selection then Screen (see the module
/// doc comment), proposal `text_review`, executor `replace_text`, `confirm
/// = true` for the same reason `calendar::builtin_action` is: the model's
/// proposal always needs a human "Do it" before anything is written.
pub fn builtin_action() -> Action {
    Action {
        id: ACTION_ID.to_string(),
        name: "Review this email".to_string(),
        group: Some("Writing".to_string()),
        // Catalogue metadata only, in priority order -- see the module doc
        // comment's numbered list. Nothing reads `Action::inputs` on the
        // real execution path yet (same "inert until its caller exists"
        // status `actions::mod`'s own doc comment gives it); the real
        // priority decision is `choose_input_source`, called directly by
        // `app.rs`'s worker.
        inputs: vec![InputKind::Uia, InputKind::Selection, InputKind::Screen],
        proposal: "text_review".to_string(),
        executor: "replace_text".to_string(),
        confirm: true,
        prompt: BASE_PROMPT.to_string(),
        prefer: Prefer::default(),
        hotkey: None,
        rate_difficulty: false,
        enabled: true,
    }
}

// ---------------------------------------------------------------------------
// The text_review proposal: parsing, verdict, edits
// ---------------------------------------------------------------------------

/// One edit the model proposed, exactly as `actions::schema`'s
/// `text_review` schema shapes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditProposal {
    pub before: String,
    pub after: String,
    pub reason: String,
}

/// Parses a `text_review`-shaped [`crate::provider::Completion::text`] into
/// the raw proposal `Value` the preview card and [`edits_from_value`] both
/// read straight out of -- same reason `actions::calendar::parse_calendar_proposal`
/// returns a raw `Value` rather than a typed struct: the preview card
/// renders straight off the schema-described `Value`, so a second typed
/// representation would just be a second thing that could drift from it.
/// This only proves the completion is well-formed (every field present,
/// the right JSON type, `verdict` one of the two documented strings), never
/// a partial/best-effort parse.
pub fn parse_text_review_proposal(text: &str) -> anyhow::Result<Value> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("provider: completion text is not valid JSON: {e}"))?;
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("provider: text_review completion is not a JSON object"))?;

    let edits = obj.get("edits").and_then(Value::as_array).ok_or_else(|| {
        anyhow::anyhow!("provider: text_review completion is missing an \"edits\" array field")
    })?;
    for (i, edit) in edits.iter().enumerate() {
        let edit_obj = edit.as_object().ok_or_else(|| {
            anyhow::anyhow!("provider: text_review completion's edits[{i}] is not a JSON object")
        })?;
        for field in ["before", "after", "reason"] {
            if !edit_obj.get(field).is_some_and(Value::is_string) {
                anyhow::bail!(
                    "provider: text_review completion's edits[{i}] is missing a \"{field}\" string field"
                );
            }
        }
    }

    let verdict = obj.get("verdict").and_then(Value::as_str).ok_or_else(|| {
        anyhow::anyhow!("provider: text_review completion is missing a \"verdict\" string field")
    })?;
    anyhow::ensure!(
        verdict == "good_to_go" || verdict == "needs_edits",
        "provider: text_review completion has an unrecognized \"verdict\" value \"{verdict}\"; expected \"good_to_go\" or \"needs_edits\""
    );

    if !obj.get("tone_note").is_some_and(Value::is_string) {
        anyhow::bail!("provider: text_review completion is missing a \"tone_note\" string field");
    }
    if !obj.get("missing_attachment").is_some_and(Value::is_boolean) {
        anyhow::bail!(
            "provider: text_review completion is missing a \"missing_attachment\" boolean field"
        );
    }

    Ok(value)
}

/// Whether `value`'s `verdict` is the documented "nothing to fix" sentinel.
/// An exact match, same shape as `actions::calendar::is_no_event`.
pub fn is_good_to_go(value: &Value) -> bool {
    value.get("verdict").and_then(Value::as_str) == Some("good_to_go")
}

/// Extracts the typed `edits` list from a parsed `text_review` `Value` for
/// [`apply_edits`]. Degrades to an empty list on any malformed shape rather
/// than panicking (rule 7) -- callers only ever reach this after
/// [`parse_text_review_proposal`] already validated the shape, so this is a
/// second line of defense, not the primary check.
pub fn edits_from_value(value: &Value) -> Vec<EditProposal> {
    value
        .get("edits")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|edit| {
                    let obj = edit.as_object()?;
                    Some(EditProposal {
                        before: obj.get("before")?.as_str()?.to_string(),
                        after: obj.get("after")?.as_str()?.to_string(),
                        reason: obj.get("reason")?.as_str()?.to_string(),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Deterministic edit application (pure)
// ---------------------------------------------------------------------------

/// One edit that was actually applied to the text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedEdit {
    pub before: String,
    pub after: String,
    pub reason: String,
}

/// The result of [`apply_edits`]: the new full text, plus which edits were
/// applied and which were dropped (in the same relative order the model
/// proposed them).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ApplyEditsResult {
    pub new_text: String,
    pub applied: Vec<AppliedEdit>,
    pub dropped: Vec<EditProposal>,
}

/// Applies `edits` to `original` in order, producing the new full text. See
/// the module doc comment's "Deterministic edit application" section for
/// the exact rule (search only forward from the end of the previous edit's
/// replacement). An edit with an empty `before` is always dropped: there is
/// no meaningful location to anchor it to.
pub fn apply_edits(original: &str, edits: &[EditProposal]) -> ApplyEditsResult {
    let mut text = original.to_string();
    let mut search_from = 0usize;
    let mut applied = Vec::new();
    let mut dropped = Vec::new();

    for edit in edits {
        if edit.before.is_empty() {
            dropped.push(edit.clone());
            continue;
        }

        match text
            .get(search_from..)
            .and_then(|rest| rest.find(edit.before.as_str()))
        {
            Some(rel_pos) => {
                let pos = search_from + rel_pos;
                let end = pos + edit.before.len();
                text.replace_range(pos..end, &edit.after);
                search_from = pos + edit.after.len();
                applied.push(AppliedEdit {
                    before: edit.before.clone(),
                    after: edit.after.clone(),
                    reason: edit.reason.clone(),
                });
            }
            None => dropped.push(edit.clone()),
        }
    }

    ApplyEditsResult {
        new_text: text,
        applied,
        dropped,
    }
}

// ---------------------------------------------------------------------------
// Input source (pure decision)
// ---------------------------------------------------------------------------

/// Which of #38's three input sources supplied the reviewed text. See the
/// module doc comment's numbered list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputSource {
    ComposeBody,
    Selection,
    Screen,
}

/// The pure priority decision behind #38's "Inputs in priority order":
/// compose body, else selection, else screen. Kept separate from the real
/// (Win32) capture attempts in [`capture_input`] so the ordering itself has
/// a plain unit test, independent of whether a live UIA call happens to
/// succeed on any given machine.
pub fn choose_input_source(compose_body_available: bool, selection_available: bool) -> InputSource {
    if compose_body_available {
        InputSource::ComposeBody
    } else if selection_available {
        InputSource::Selection
    } else {
        InputSource::Screen
    }
}

/// Whether `source` is even the KIND of source that can ever carry a
/// target "Do it" could write through. `ComposeBody` always can;
/// `Selection` can too (#219: when the UIA `TextPattern` path captured the
/// owning element's identity and offsets -- see
/// [`CapturedInput::Selection`]'s doc comment for the cases where it still
/// does not); `Screen` never can, by design. This is necessary but NOT
/// sufficient for "show Do it": a `Selection`-sourced review can still end
/// up with no target (a `ValuePattern`-only control, a clipboard-fallback
/// capture, a discontiguous multi-range selection), so `app.rs`'s actual
/// gate also checks [`ReviewOutcome::target`] itself.
pub fn source_has_target(source: InputSource) -> bool {
    matches!(source, InputSource::ComposeBody | InputSource::Selection)
}

/// Identifies one UIA element, captured at "Look" time. Field-for-field the
/// same shape as `executors::replace_text::TargetRef` (that type is private
/// to the `executors` module tree, so this is a deliberate, independent
/// twin, not a duplicate of a *shared* type -- see [`build_replace_text_proposal`]
/// for where the two shapes are reconciled: as JSON, the wire format
/// `executors::replace_text::parse_target_ref` already reads, not as a
/// shared Rust type across a module boundary that does not otherwise need
/// one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedTarget {
    pub hwnd: isize,
    pub runtime_id: Vec<i32>,
    pub automation_id: String,
    pub name: String,
    pub control_type: String,
}

/// Which UIA element (if any) "Do it" would write through, and how --
/// [`ReplaceTarget::Whole`] for a `ComposeBody` capture (`replace_text`'s
/// `ReplaceMode::ReplaceAll`), [`ReplaceTarget::Selection`] for a
/// `Selection` capture that carried a
/// [`crate::inputs::selection::SelectionTarget`] (`ReplaceMode::ReplaceSelection`,
/// #219). `Screen`, and a `Selection` with no captured target, have no
/// `ReplaceTarget` at all -- see [`source_has_target`] and
/// [`CapturedInput::target`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplaceTarget {
    Whole(CapturedTarget),
    Selection(crate::inputs::selection::SelectionTarget),
}

/// What "Look" gathered before asking the model, tagged by which
/// [`InputSource`] supplied it. `Selection`'s `target` is `Some` only when
/// `inputs::selection::get_selection_foreground_with_target`'s UIA path
/// found a single contiguous selection with everything readable (#219) --
/// it is `None` for a `ValuePattern`-only control (no `TextPattern` at
/// all), a discontiguous multi-range selection, or a capture that fell back
/// to the clipboard, all of which have text but no re-resolvable element to
/// write back through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapturedInput {
    ComposeBody {
        target: CapturedTarget,
        text: String,
    },
    Selection {
        text: String,
        target: Option<crate::inputs::selection::SelectionTarget>,
    },
    Screen,
}

impl CapturedInput {
    pub fn source(&self) -> InputSource {
        match self {
            CapturedInput::ComposeBody { .. } => InputSource::ComposeBody,
            CapturedInput::Selection { .. } => InputSource::Selection,
            CapturedInput::Screen => InputSource::Screen,
        }
    }

    pub fn text(&self) -> Option<&str> {
        match self {
            CapturedInput::ComposeBody { text, .. } => Some(text.as_str()),
            CapturedInput::Selection { text, .. } => Some(text.as_str()),
            CapturedInput::Screen => None,
        }
    }

    /// The [`ReplaceTarget`] "Do it" would write through, if any. Owned
    /// (not a reference) since `ComposeBody` and `Selection` carry two
    /// different underlying types -- see [`ReplaceTarget`]'s own doc
    /// comment.
    pub fn target(&self) -> Option<ReplaceTarget> {
        match self {
            CapturedInput::ComposeBody { target, .. } => Some(ReplaceTarget::Whole(target.clone())),
            CapturedInput::Selection {
                target: Some(target),
                ..
            } => Some(ReplaceTarget::Selection(target.clone())),
            _ => None,
        }
    }
}

/// Real capture, in priority order: [`com::capture_focused_compose_body`]
/// (Win32/UIA), else `inputs::selection::get_selection_foreground`
/// (already crate-existing), else [`CapturedInput::Screen`] (the caller
/// still needs to grab and attach a screenshot itself; this function only
/// decides which text-bearing source won, since the screenshot capture has
/// its own main-thread timing constraints `app.rs` already handles for
/// `calendar_worker`'s flow -- see that call site's doc comment).
///
/// **Must be called from a dedicated worker thread**, never from the
/// low-level keyboard hook's thread or the main message-loop thread: both
/// the UIA call and `get_selection_foreground`'s own fallback can block for
/// a while against a hung or slow foreground app (`inputs::uia`'s and
/// `inputs::selection`'s own module docs carry the same requirement).
pub fn capture_input(foreground_hwnd: isize) -> CapturedInput {
    let compose_body = com::capture_focused_compose_body(foreground_hwnd)
        .ok()
        .flatten()
        .filter(|(_, text)| !text.trim().is_empty());

    let selection = crate::inputs::selection::get_selection_foreground_with_target(
        foreground_hwnd,
        crate::inputs::selection::DEFAULT_MAX_CHARS,
        crate::inputs::selection::DEFAULT_CLIPBOARD_WAIT_BUDGET,
    )
    .ok()
    .filter(|s| !s.text.trim().is_empty());

    // The real capture attempts happen above (each independently cheap to
    // have already run, so trying both costs nothing extra); which one
    // actually gets used is decided by the one pure function so the
    // priority order itself stays covered by a plain unit test, not just
    // by this function's own (Win32-dependent, hand-checked) behaviour.
    match choose_input_source(compose_body.is_some(), selection.is_some()) {
        InputSource::ComposeBody => {
            let (target, text) = compose_body.expect("just matched Some above");
            CapturedInput::ComposeBody { target, text }
        }
        InputSource::Selection => {
            let selection = selection.expect("just matched Some above");
            CapturedInput::Selection {
                text: selection.text,
                target: selection.target,
            }
        }
        InputSource::Screen => CapturedInput::Screen,
    }
}

// ---------------------------------------------------------------------------
// Building the "Do it" replace_text proposal, and running it
// ---------------------------------------------------------------------------

/// What `review_worker` (in `app.rs`) hands back over
/// `WM_APP_REVIEW_RESULT`: the raw `text_review` proposal, the text that
/// was actually reviewed (needed as `apply_edits`'s `original` and, for a
/// `ComposeBody` source, as `ReviewContext::original_text`/the stale-target
/// check's baseline), and the [`CapturedTarget`] when [`InputSource::ComposeBody`]
/// supplied it -- `None` for `Selection`/`Screen`, which is exactly
/// [`source_has_target`]'s signal that "Do it" is unavailable.
#[derive(Debug, Clone, PartialEq)]
pub struct ReviewOutcome {
    pub proposal: Value,
    pub original_text: String,
    pub target: Option<ReplaceTarget>,
    /// Which [`InputSource`] supplied `original_text`/`target`. `app.rs`
    /// calls [`source_has_target`] on this as the coarse "this category can
    /// ever have a target" check; since #219 that is necessary but not
    /// sufficient, so `app.rs` also checks `target.is_some()` directly (a
    /// `Selection` source does not always set `target` -- see
    /// [`CapturedInput::Selection`]'s doc comment).
    pub source: InputSource,
}

/// Everything "Do it" needs, stashed by `app.rs` between the preview being
/// shown and the user's decision arriving (`WM_APP_PREVIEW_DECIDED`) --
/// `App::pending_review`'s value. `original_text`/`new_text` are already
/// computed (by [`apply_edits`]) before the preview is ever shown, so "Do
/// it" only builds and sends the `replace_text` proposal; it never asks the
/// model again and never re-derives the edit application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewContext {
    pub target: ReplaceTarget,
    pub original_text: String,
    pub new_text: String,
}

/// Builds the JSON proposal `executors::replace_text`'s
/// `parse_target_ref`/`parse_replace_text` expect (verified by hand against
/// that module's source: `target.{hwnd,runtime_id,automation_id,name,control_type}`,
/// `new_text`, `mode`, `expected_current_text`, and for `ReplaceSelection`
/// also `selection_start`/`selection_end`).
///
/// [`ReplaceTarget::Whole`] (a `ComposeBody` capture) always uses
/// `"replace_all"`: #38's task brief is explicit that "Do it" replaces the
/// whole captured text with the edited version there. [`ReplaceTarget::Selection`]
/// (#219) uses `"replace_selection"`: `ctx.new_text` is the edited SELECTED
/// snippet only (`apply_edits` ran against `ctx.original_text`, which for a
/// `Selection` capture is just the selected text -- see
/// [`CapturedInput::Selection`]'s doc comment), and `expected_current_text`
/// is the element's WHOLE current text (`sel.full_text`, not
/// `ctx.original_text`) -- the base `replace_text::splice_utf16` splices
/// `new_text` into at `[sel.start, sel.end)`.
fn build_replace_text_proposal(ctx: &ReviewContext) -> Value {
    match &ctx.target {
        ReplaceTarget::Whole(target) => serde_json::json!({
            "target": {
                "hwnd": target.hwnd as i64,
                "runtime_id": target.runtime_id,
                "automation_id": target.automation_id,
                "name": target.name,
                "control_type": target.control_type
            },
            "new_text": ctx.new_text,
            "mode": "replace_all",
            "expected_current_text": ctx.original_text
        }),
        ReplaceTarget::Selection(sel) => serde_json::json!({
            "target": {
                "hwnd": sel.hwnd as i64,
                "runtime_id": sel.runtime_id,
                "automation_id": sel.automation_id,
                "name": sel.name,
                "control_type": sel.control_type
            },
            "new_text": ctx.new_text,
            "mode": "replace_selection",
            "expected_current_text": sel.full_text,
            "selection_start": sel.start,
            "selection_end": sel.end
        }),
    }
}

/// Runs `executor` (the real `replace_text` executor from
/// `executors::registry::resolve("replace_text")`, or a test fake
/// implementing [`crate::executors::Executor`]) against the proposal built
/// from `ctx`. This is what "the flow's own tests exercise with an
/// injectable replace executor" means in practice: `app.rs`'s real caller
/// (`App::on_preview_decided`) passes the live executor; this file's own
/// tests pass a fake one, so the "build the proposal, mint a confirmation,
/// execute" wiring is provable with no real UIA involved.
///
/// Minting a fresh `Confirmed<Value>` here (via `ui::confirm::confirm` +
/// `ui::confirm::user_confirmed`, both `pub(crate)`) rather than reusing the
/// `Confirmed<Value>` the preview card itself produced is deliberate, not a
/// bypass: the preview's own `Confirmed` carries a `text_review`-shaped
/// value (`edits`/`verdict`/`tone_note`/`missing_attachment`, built for
/// display), which is not the JSON shape `replace_text` parses at all. This
/// function only ever runs from `App::on_preview_decided`'s branch that
/// already required `Card::take_confirmed()` to return `Some` -- i.e. the
/// card's own "Do it" handler already called `user_confirmed()` once for
/// this exact button press; this is a second, differently-shaped
/// `Confirmed<Value>` for the same already-confirmed user decision, not a
/// second, unconfirmed one.
pub fn do_review_confirmed(
    executor: &dyn crate::executors::Executor,
    ctx: &ReviewContext,
) -> anyhow::Result<crate::executors::Undo> {
    let proposal_value = build_replace_text_proposal(ctx);
    let confirmed = crate::ui::confirm::confirm(
        crate::ui::confirm::Proposal::new(proposal_value),
        crate::ui::confirm::user_confirmed(),
    );
    executor.execute(confirmed)
}

// ---------------------------------------------------------------------------
// The flow's pure state machine
// ---------------------------------------------------------------------------

/// The "Review this email" flow's states, the same shape as
/// `actions::calendar::FlowState` (Proposed -> Previewed -> Confirmed ->
/// Executed / Cancelled), with one addition: [`FlowState::GoodToGo`] is its
/// own terminal state, not folded into `Cancelled` -- unlike calendar's "no
/// event" case, "good to go" is a real, positive result the user should see
/// a card for ("Good to go"), not a silent no-op.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowState {
    /// The provider returned a real (`needs_edits`) proposal.
    Proposed(String),
    /// `verdict == "good_to_go"`: terminal, shown as a plain card, no
    /// preview and no "Do it".
    GoodToGo,
    /// On screen in the preview card, waiting for a decision.
    Previewed(String),
    /// "Do it" pressed.
    Confirmed(String),
    /// The executor ran successfully; carries the summary text the result
    /// card shows.
    Executed { summary: String },
    /// "Cancel"/Esc: nothing runs.
    Cancelled,
    /// The provider chain or the executor failed; carries the error text
    /// for the error card.
    Failed(String),
}

/// One thing that can happen to move the flow from one [`FlowState`] to the
/// next. `Proposed`/`Previewed`/`Confirmed` carry a plain `String` id
/// (rather than the full parsed proposal) since nothing about the
/// transition rules themselves depends on the proposal's contents beyond
/// its verdict, which [`FlowEvent::ProviderReturned`] already inspects
/// before the state is built.
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FlowEvent {
    /// The provider chain finished (or failed). `Ok` carries the proposal id
    /// and whether it was `good_to_go`.
    ProviderReturned(Result<(String, bool), String>),
    Shown,
    Confirmed,
    Cancelled,
    Executed(Result<String, String>),
}

/// The whole flow as one pure transition function, the same "no-op on an
/// out-of-order event, never a panic" contract
/// `actions::calendar::advance` has.
#[allow(dead_code)]
pub fn advance(state: FlowState, event: FlowEvent) -> FlowState {
    match (state, event) {
        (_, FlowEvent::ProviderReturned(Err(e))) => FlowState::Failed(e),
        (_, FlowEvent::ProviderReturned(Ok((_, true)))) => FlowState::GoodToGo,
        (_, FlowEvent::ProviderReturned(Ok((id, false)))) => FlowState::Proposed(id),
        (FlowState::Proposed(id), FlowEvent::Shown) => FlowState::Previewed(id),
        (FlowState::Previewed(id), FlowEvent::Confirmed) => FlowState::Confirmed(id),
        (FlowState::Previewed(_), FlowEvent::Cancelled) => FlowState::Cancelled,
        (FlowState::Confirmed(_), FlowEvent::Executed(Ok(summary))) => {
            FlowState::Executed { summary }
        }
        (FlowState::Confirmed(_), FlowEvent::Executed(Err(e))) => FlowState::Failed(e),
        (other, _) => other,
    }
}

// ---------------------------------------------------------------------------
// Win32 / UI Automation: capturing the focused compose body
// ---------------------------------------------------------------------------

mod com {
    //! `GetFocusedElement()`, then read enough to decide "is this a
    //! writable text body" and, if so, its identity (for [`CapturedTarget`])
    //! and its whole current text. Deliberately its own small module
    //! (duplicating `ComApartment`/the runtime-id SAFEARRAY reader rather
    //! than sharing `inputs::uia`'s or `executors::replace_text`'s private
    //! copies) -- same "duplicated, not shared" call `executors::replace_text::com`'s
    //! own doc comment already makes for this exact tradeoff: those are
    //! private to their own files, and #38's scope is this file plus the
    //! small `actions::mod`/`actions::schema`/`app.rs`/`ui::tray` diffs
    //! listed in the module doc comment, not a cross-module refactor.
    //!
    //! **Win32, checked by hand (rule 8), not by an automated test in this
    //! file**: a synthetic test window has no real "compose body" for a
    //! human to have focused, so the meaningful check is a real one, in a
    //! real mail client -- tracked as the Gmail compose manual check in
    //! issue #166, per this task's own instructions, rather than invented
    //! here as a flaky synthetic-window test.

    use super::CapturedTarget;
    use anyhow::Result;
    use windows::core::Interface;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_INPROC_SERVER,
        COINIT_APARTMENTTHREADED, SAFEARRAY,
    };
    use windows::Win32::System::Ole::{
        SafeArrayAccessData, SafeArrayDestroy, SafeArrayGetLBound, SafeArrayGetUBound,
        SafeArrayUnaccessData,
    };
    use windows::Win32::UI::Accessibility::{
        CUIAutomation8, IUIAutomation, IUIAutomationValuePattern, UIA_TextPatternId,
        UIA_ValuePatternId,
    };

    struct ComApartment;

    impl ComApartment {
        fn enter() -> Result<Self> {
            let hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
            hr.ok()?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    fn is_null<I: Interface>(i: &I) -> bool {
        i.as_raw().is_null()
    }

    fn control_type_to_string(id: windows::Win32::UI::Accessibility::UIA_CONTROLTYPE_ID) -> String {
        use windows::Win32::UI::Accessibility::{
            UIA_CheckBoxControlTypeId as CHECKBOX, UIA_ComboBoxControlTypeId as COMBOBOX,
            UIA_DocumentControlTypeId as DOCUMENT, UIA_EditControlTypeId as EDIT,
            UIA_ListControlTypeId as LIST, UIA_RadioButtonControlTypeId as RADIOBUTTON,
            UIA_TextControlTypeId as TEXT,
        };
        if id == EDIT {
            "Edit"
        } else if id == COMBOBOX {
            "ComboBox"
        } else if id == DOCUMENT {
            "Document"
        } else if id == CHECKBOX {
            "CheckBox"
        } else if id == RADIOBUTTON {
            "RadioButton"
        } else if id == LIST {
            "List"
        } else if id == TEXT {
            "Text"
        } else {
            "Other"
        }
        .to_string()
    }

    /// Same shape as `executors::replace_text::com::runtime_id_from_safearray`
    /// (duplicated, see the module doc comment).
    unsafe fn runtime_id_from_safearray(psa: *mut SAFEARRAY) -> Result<Vec<i32>> {
        if psa.is_null() {
            return Ok(Vec::new());
        }

        struct SafeArrayGuard(*mut SAFEARRAY);
        impl Drop for SafeArrayGuard {
            fn drop(&mut self) {
                unsafe {
                    let _ = SafeArrayDestroy(self.0);
                }
            }
        }
        let _guard = SafeArrayGuard(psa);

        let lbound = unsafe { SafeArrayGetLBound(psa, 1) }?;
        let ubound = unsafe { SafeArrayGetUBound(psa, 1) }?;
        if ubound < lbound {
            return Ok(Vec::new());
        }
        let count = (ubound - lbound + 1) as usize;

        let mut data_ptr: *mut core::ffi::c_void = std::ptr::null_mut();
        unsafe { SafeArrayAccessData(psa, &mut data_ptr) }?;
        let slice = unsafe { std::slice::from_raw_parts(data_ptr as *const i32, count) };
        let result = slice.to_vec();
        unsafe { SafeArrayUnaccessData(psa) }?;

        Ok(result)
    }

    /// The real capture: `GetFocusedElement()`, then (in order) the
    /// password guard, the `ValuePattern` requirement (needed for "Do it"
    /// to write back later), the `TextPattern` requirement (this module's
    /// "compose body" signal), and finally reading the identity fields plus
    /// the element's whole current text via `ValuePattern.CurrentValue` --
    /// the SAME property `replace_text`'s own re-resolution reads
    /// (`UIA_ValueValuePropertyId`/`CachedValue`/`CurrentValue`, all the
    /// live `ValuePattern.Value`), so a field genuinely untouched between
    /// "Look" and "Do" reads back identical and never spuriously trips the
    /// stale-target check.
    ///
    /// `Ok(None)` (never an `Err` bubbled past this function for an
    /// ordinary "nothing usable focused" case) covers: no focused element,
    /// a password field, no `ValuePattern`, no `TextPattern`. A genuine COM
    /// failure (the automation object could not be created at all) is the
    /// one case that still surfaces as `Err`, exactly like every other
    /// `anyhow::Result`-returning Win32 call in this crate -- `capture_input`
    /// treats both the same way (falls through to the next input source),
    /// since neither means "Do it should still be offered".
    pub(super) fn capture_focused_compose_body(
        hwnd: isize,
    ) -> Result<Option<(CapturedTarget, String)>> {
        let _apartment = ComApartment::enter()?;
        let automation: IUIAutomation =
            unsafe { CoCreateInstance(&CUIAutomation8, None, CLSCTX_INPROC_SERVER) }?;

        let element = unsafe { automation.GetFocusedElement() }?;
        if is_null(&element) {
            return Ok(None);
        }

        let is_password = unsafe { element.CurrentIsPassword() }
            .map(|b| b.as_bool())
            .unwrap_or(false);
        if is_password {
            // NEVER read a password field's text -- same guard shape as
            // `inputs::uia`/`inputs::selection`/`executors::replace_text`.
            return Ok(None);
        }

        let value_pattern: Option<IUIAutomationValuePattern> =
            match unsafe { element.GetCurrentPattern(UIA_ValuePatternId) } {
                Ok(unknown) if !is_null(&unknown) => unknown.cast().ok(),
                _ => None,
            };
        let Some(value_pattern) = value_pattern else {
            return Ok(None);
        };

        let has_text_pattern = matches!(
            unsafe { element.GetCurrentPattern(UIA_TextPatternId) },
            Ok(unknown) if !is_null(&unknown)
        );
        if !has_text_pattern {
            return Ok(None);
        }

        let text = unsafe { value_pattern.CurrentValue() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        if text.trim().is_empty() {
            // Nothing worth reviewing.
            return Ok(None);
        }

        let name = unsafe { element.CurrentName() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let automation_id = unsafe { element.CurrentAutomationId() }
            .map(|b| b.to_string())
            .unwrap_or_default();
        let control_type = control_type_to_string(unsafe { element.CurrentControlType() }?);
        let runtime_id = unsafe { element.GetRuntimeId() }
            .ok()
            .and_then(|psa| unsafe { runtime_id_from_safearray(psa) }.ok())
            .unwrap_or_default();

        Ok(Some((
            CapturedTarget {
                hwnd,
                runtime_id,
                automation_id,
                name,
                control_type,
            },
            text,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- builtin_action -------------------------------------------------

    #[test]
    fn builtin_action_matches_the_task_brief() {
        let a = builtin_action();
        assert_eq!(a.id, ACTION_ID);
        assert_eq!(a.name, "Review this email");
        assert_eq!(a.group.as_deref(), Some("Writing"));
        assert_eq!(a.proposal, "text_review");
        assert_eq!(a.executor, "replace_text");
        assert!(a.confirm, "this action must always show the preview card");
        assert!(!a.rate_difficulty);
        assert!(a.enabled);
        assert!(!a.prompt.is_empty());
        assert!(
            !a.prompt.contains('\u{2014}'),
            "no em dashes in the prompt (rule 11 in spirit for anything a model might echo back)"
        );
    }

    // -- parse_text_review_proposal --------------------------------------

    fn sample_proposal() -> Value {
        serde_json::json!({
            "edits": [
                {"before": "wnated", "after": "wanted", "reason": "typo"},
                {"before": "folow", "after": "follow", "reason": "typo"}
            ],
            "verdict": "needs_edits",
            "tone_note": "Friendly and direct.",
            "missing_attachment": false
        })
    }

    fn good_to_go_proposal() -> Value {
        serde_json::json!({
            "edits": [],
            "verdict": "good_to_go",
            "tone_note": "Clear and polite.",
            "missing_attachment": false
        })
    }

    #[test]
    fn parse_text_review_proposal_parses_a_full_object() {
        let text = sample_proposal().to_string();
        let value = parse_text_review_proposal(&text).expect("valid text_review JSON must parse");
        assert_eq!(value["verdict"], "needs_edits");
    }

    #[test]
    fn parse_text_review_proposal_rejects_invalid_json() {
        let err = parse_text_review_proposal("not json").unwrap_err();
        assert!(err.to_string().contains("JSON"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_a_non_object() {
        let err = parse_text_review_proposal("[1,2,3]").unwrap_err();
        assert!(err.to_string().contains("object"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_a_missing_edits_field() {
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("edits");
        let err = parse_text_review_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("edits"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_an_edit_missing_a_field() {
        let mut value = sample_proposal();
        value["edits"][0].as_object_mut().unwrap().remove("reason");
        let err = parse_text_review_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("reason"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_an_unrecognized_verdict() {
        let mut value = sample_proposal();
        value["verdict"] = serde_json::json!("maybe");
        let err = parse_text_review_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("verdict"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_a_missing_tone_note() {
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("tone_note");
        let err = parse_text_review_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("tone_note"));
    }

    #[test]
    fn parse_text_review_proposal_rejects_a_missing_missing_attachment() {
        let mut value = sample_proposal();
        value.as_object_mut().unwrap().remove("missing_attachment");
        let err = parse_text_review_proposal(&value.to_string()).unwrap_err();
        assert!(err.to_string().contains("missing_attachment"));
    }

    #[test]
    fn parse_text_review_proposal_accepts_the_good_to_go_shape() {
        let text = good_to_go_proposal().to_string();
        let value =
            parse_text_review_proposal(&text).expect("the good_to_go shape must still parse");
        assert!(is_good_to_go(&value));
    }

    // -- is_good_to_go ----------------------------------------------------

    #[test]
    fn is_good_to_go_true_for_the_exact_verdict() {
        assert!(is_good_to_go(&good_to_go_proposal()));
    }

    #[test]
    fn is_good_to_go_false_for_needs_edits() {
        assert!(!is_good_to_go(&sample_proposal()));
    }

    // -- edits_from_value ---------------------------------------------------

    #[test]
    fn edits_from_value_extracts_every_edit_in_order() {
        let edits = edits_from_value(&sample_proposal());
        assert_eq!(edits.len(), 2);
        assert_eq!(edits[0].before, "wnated");
        assert_eq!(edits[0].after, "wanted");
        assert_eq!(edits[1].before, "folow");
    }

    #[test]
    fn edits_from_value_is_empty_for_a_malformed_shape_not_a_panic() {
        assert!(edits_from_value(&serde_json::json!("not an object")).is_empty());
        assert!(edits_from_value(&serde_json::json!({})).is_empty());
    }

    // -- apply_edits: the deterministic edit-application table --------------

    fn edit(before: &str, after: &str) -> EditProposal {
        EditProposal {
            before: before.to_string(),
            after: after.to_string(),
            reason: "test".to_string(),
        }
    }

    #[test]
    fn apply_edits_with_no_edits_leaves_text_unchanged() {
        let result = apply_edits("hello world", &[]);
        assert_eq!(result.new_text, "hello world");
        assert!(result.applied.is_empty());
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn apply_edits_applies_a_single_edit() {
        let result = apply_edits("I wnated to help", &[edit("wnated", "wanted")]);
        assert_eq!(result.new_text, "I wanted to help");
        assert_eq!(result.applied.len(), 1);
        assert!(result.dropped.is_empty());
    }

    #[test]
    fn apply_edits_applies_multiple_non_overlapping_edits_in_order() {
        let result = apply_edits(
            "I wnated to folow up",
            &[edit("wnated", "wanted"), edit("folow", "follow")],
        );
        assert_eq!(result.new_text, "I wanted to follow up");
        assert_eq!(result.applied.len(), 2);
    }

    #[test]
    fn apply_edits_drops_an_edit_whose_before_is_not_found() {
        let result = apply_edits("hello world", &[edit("xyz", "abc")]);
        assert_eq!(result.new_text, "hello world");
        assert!(result.applied.is_empty());
        assert_eq!(result.dropped, vec![edit("xyz", "abc")]);
    }

    #[test]
    fn apply_edits_drops_only_the_unmatched_edit_and_still_applies_the_rest() {
        let result = apply_edits(
            "I wnated to help",
            &[edit("xyz", "abc"), edit("wnated", "wanted")],
        );
        assert_eq!(result.new_text, "I wanted to help");
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.dropped, vec![edit("xyz", "abc")]);
    }

    #[test]
    fn apply_edits_drops_an_edit_with_an_empty_before() {
        let result = apply_edits("hello", &[edit("", "x")]);
        assert_eq!(result.new_text, "hello");
        assert_eq!(result.dropped.len(), 1);
    }

    #[test]
    fn apply_edits_repeated_substring_replaces_the_first_unmatched_occurrence_after_the_previous_edit(
    ) {
        // "cat cat cat": first edit consumes the first "cat", second edit
        // must land on the SECOND "cat", not re-match the first one (now
        // "dog") or skip straight to the third.
        let result = apply_edits("cat cat cat", &[edit("cat", "dog"), edit("cat", "bird")]);
        assert_eq!(result.new_text, "dog bird cat");
        assert_eq!(result.applied.len(), 2);
    }

    #[test]
    fn apply_edits_overlapping_edit_is_dropped_once_its_target_is_already_consumed() {
        // The second edit's "before" ("world") only existed inside the
        // first edit's "before" ("the world"); once the first edit runs,
        // "world" no longer appears anywhere from search_from onward, so
        // the second edit must be dropped, not match some earlier,
        // already-passed occurrence.
        let result = apply_edits(
            "the world is great",
            &[edit("the world", "the galaxy"), edit("world", "planet")],
        );
        assert_eq!(result.new_text, "the galaxy is great");
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.dropped, vec![edit("world", "planet")]);
    }

    #[test]
    fn apply_edits_never_rematches_inside_its_own_just_inserted_replacement() {
        // The replacement text itself contains the next edit's "before" --
        // search_from must skip past the inserted text, not re-scan it.
        let result = apply_edits("x", &[edit("x", "cat"), edit("cat", "dog")]);
        assert_eq!(result.new_text, "cat");
        assert_eq!(result.applied.len(), 1);
        assert_eq!(result.dropped, vec![edit("cat", "dog")]);
    }

    // -- choose_input_source / source_has_target -----------------------------

    #[test]
    fn choose_input_source_prefers_compose_body_over_selection_and_screen() {
        assert_eq!(choose_input_source(true, true), InputSource::ComposeBody);
        assert_eq!(choose_input_source(true, false), InputSource::ComposeBody);
    }

    #[test]
    fn choose_input_source_falls_back_to_selection_then_screen() {
        assert_eq!(choose_input_source(false, true), InputSource::Selection);
        assert_eq!(choose_input_source(false, false), InputSource::Screen);
    }

    #[test]
    fn source_has_target_for_compose_body_and_selection_never_for_screen() {
        assert!(source_has_target(InputSource::ComposeBody));
        assert!(
            source_has_target(InputSource::Selection),
            "#219: Selection CAN carry a target, even though it does not always"
        );
        assert!(!source_has_target(InputSource::Screen));
    }

    // -- CapturedInput accessors ----------------------------------------------

    fn sample_target() -> CapturedTarget {
        CapturedTarget {
            hwnd: 4242,
            runtime_id: vec![1, 2, 3],
            automation_id: "compose-body".to_string(),
            name: "Message Body".to_string(),
            control_type: "Document".to_string(),
        }
    }

    /// #219: a UIA-identified selection target, mirroring
    /// `inputs::selection`'s own `sample_identity`-shaped test fixtures.
    fn sample_selection_target() -> crate::inputs::selection::SelectionTarget {
        crate::inputs::selection::SelectionTarget {
            hwnd: 4343,
            runtime_id: vec![4, 5, 6],
            automation_id: "selection-field".to_string(),
            name: "Body".to_string(),
            control_type: "Edit".to_string(),
            full_text: "I wnated to help".to_string(),
            start: 2,
            end: 8,
        }
    }

    #[test]
    fn captured_input_compose_body_reports_its_own_source_text_and_target() {
        let c = CapturedInput::ComposeBody {
            target: sample_target(),
            text: "hi there".to_string(),
        };
        assert_eq!(c.source(), InputSource::ComposeBody);
        assert_eq!(c.text(), Some("hi there"));
        assert_eq!(c.target(), Some(ReplaceTarget::Whole(sample_target())));
    }

    #[test]
    fn captured_input_selection_has_text_but_no_target_when_none_was_captured() {
        let c = CapturedInput::Selection {
            text: "hi there".to_string(),
            target: None,
        };
        assert_eq!(c.source(), InputSource::Selection);
        assert_eq!(c.text(), Some("hi there"));
        assert_eq!(c.target(), None);
    }

    #[test]
    fn captured_input_selection_with_identity_reports_a_replace_target() {
        // #219: a Selection source CAN carry a target when the UIA path
        // captured one.
        let c = CapturedInput::Selection {
            text: "wnated".to_string(),
            target: Some(sample_selection_target()),
        };
        assert_eq!(c.source(), InputSource::Selection);
        assert_eq!(c.text(), Some("wnated"));
        assert_eq!(
            c.target(),
            Some(ReplaceTarget::Selection(sample_selection_target()))
        );
    }

    #[test]
    fn captured_input_screen_has_no_text_and_no_target() {
        let c = CapturedInput::Screen;
        assert_eq!(c.source(), InputSource::Screen);
        assert_eq!(c.text(), None);
        assert_eq!(c.target(), None);
    }

    // -- build_replace_text_proposal: matches executors::replace_text's ------
    // expected wire shape (verified by hand against that module's
    // `parse_target_ref`/`parse_replace_text`, see this function's doc
    // comment) -----------------------------------------------------------

    fn sample_ctx() -> ReviewContext {
        ReviewContext {
            target: ReplaceTarget::Whole(sample_target()),
            original_text: "I wnated to help".to_string(),
            new_text: "I wanted to help".to_string(),
        }
    }

    /// #219: a `Selection`-sourced context -- `original_text`/`new_text`
    /// are the SELECTED snippet only (before/after `apply_edits`), while
    /// `expected_current_text` in the built proposal comes from
    /// `sel.full_text` (the whole element), not from `original_text`.
    fn sample_selection_ctx() -> ReviewContext {
        ReviewContext {
            target: ReplaceTarget::Selection(sample_selection_target()),
            original_text: "wnated".to_string(),
            new_text: "wanted".to_string(),
        }
    }

    #[test]
    fn build_replace_text_proposal_has_the_expected_target_fields() {
        let ctx = sample_ctx();
        let v = build_replace_text_proposal(&ctx);
        assert_eq!(v["target"]["hwnd"], serde_json::json!(4242i64));
        assert_eq!(v["target"]["runtime_id"], serde_json::json!([1, 2, 3]));
        assert_eq!(v["target"]["automation_id"], "compose-body");
        assert_eq!(v["target"]["name"], "Message Body");
        assert_eq!(v["target"]["control_type"], "Document");
    }

    #[test]
    fn build_replace_text_proposal_is_replace_all_for_a_whole_target() {
        let ctx = sample_ctx();
        let v = build_replace_text_proposal(&ctx);
        assert_eq!(v["mode"], "replace_all");
    }

    #[test]
    fn build_replace_text_proposal_carries_new_text_and_expected_current_text() {
        let ctx = sample_ctx();
        let v = build_replace_text_proposal(&ctx);
        assert_eq!(v["new_text"], "I wanted to help");
        assert_eq!(v["expected_current_text"], "I wnated to help");
    }

    // -- build_replace_text_proposal: ReplaceSelection target (#219) --------

    #[test]
    fn build_replace_text_proposal_is_replace_selection_for_a_selection_target() {
        let ctx = sample_selection_ctx();
        let v = build_replace_text_proposal(&ctx);
        assert_eq!(v["mode"], "replace_selection");
    }

    #[test]
    fn build_replace_text_proposal_selection_target_has_the_expected_target_fields() {
        let ctx = sample_selection_ctx();
        let v = build_replace_text_proposal(&ctx);
        assert_eq!(v["target"]["hwnd"], serde_json::json!(4343i64));
        assert_eq!(v["target"]["runtime_id"], serde_json::json!([4, 5, 6]));
        assert_eq!(v["target"]["automation_id"], "selection-field");
        assert_eq!(v["target"]["name"], "Body");
        assert_eq!(v["target"]["control_type"], "Edit");
    }

    #[test]
    fn build_replace_text_proposal_selection_target_uses_full_text_not_original_text() {
        let ctx = sample_selection_ctx();
        let v = build_replace_text_proposal(&ctx);
        // `expected_current_text` must be the element's WHOLE text
        // (`sel.full_text`), never `ctx.original_text` (the selected
        // snippet only) -- `replace_text::splice_utf16` needs the whole
        // string to splice into.
        assert_eq!(v["expected_current_text"], "I wnated to help");
        assert_ne!(v["expected_current_text"], ctx.original_text.as_str());
        assert_eq!(v["new_text"], "wanted");
        assert_eq!(v["selection_start"], serde_json::json!(2usize));
        assert_eq!(v["selection_end"], serde_json::json!(8usize));
    }

    // -- do_review_confirmed: injectable executor ----------------------------

    struct FakeReplaceExecutor {
        result: std::sync::Mutex<Option<anyhow::Result<crate::executors::Undo>>>,
        seen: std::sync::Mutex<Option<Value>>,
    }

    impl crate::executors::Executor for FakeReplaceExecutor {
        fn name(&self) -> &'static str {
            "replace_text"
        }
        fn effect(&self) -> crate::executors::Effect {
            crate::executors::Effect::Writes
        }
        fn execute(
            &self,
            confirmed: crate::ui::confirm::Confirmed<Value>,
        ) -> anyhow::Result<crate::executors::Undo> {
            *self.seen.lock().unwrap() = Some(confirmed.into_value());
            self.result
                .lock()
                .unwrap()
                .take()
                .expect("execute called more than once in this test")
        }
    }

    #[test]
    fn do_review_confirmed_builds_the_proposal_and_runs_the_injected_executor() {
        let fake = FakeReplaceExecutor {
            result: std::sync::Mutex::new(Some(Ok(crate::executors::Undo::none("wrote it")))),
            seen: std::sync::Mutex::new(None),
        };
        let ctx = sample_ctx();
        let undo = do_review_confirmed(&fake, &ctx).expect("fake executor succeeds");
        assert_eq!(undo.summary, "wrote it");

        let seen = fake.seen.lock().unwrap();
        let seen = seen.as_ref().expect("execute must have been called");
        assert_eq!(seen["new_text"], "I wanted to help");
        assert_eq!(seen["mode"], "replace_all");
    }

    #[test]
    fn do_review_confirmed_builds_a_replace_selection_proposal_for_a_selection_target() {
        let fake = FakeReplaceExecutor {
            result: std::sync::Mutex::new(Some(Ok(crate::executors::Undo::none("wrote it")))),
            seen: std::sync::Mutex::new(None),
        };
        let ctx = sample_selection_ctx();
        do_review_confirmed(&fake, &ctx).expect("fake executor succeeds");

        let seen = fake.seen.lock().unwrap();
        let seen = seen.as_ref().expect("execute must have been called");
        assert_eq!(seen["mode"], "replace_selection");
        assert_eq!(seen["new_text"], "wanted");
        assert_eq!(seen["selection_start"], serde_json::json!(2usize));
        assert_eq!(seen["selection_end"], serde_json::json!(8usize));
    }

    #[test]
    fn do_review_confirmed_surfaces_the_injected_executors_error() {
        let fake = FakeReplaceExecutor {
            result: std::sync::Mutex::new(Some(Err(anyhow::anyhow!(
                "replace_text: the target element could not be found"
            )))),
            seen: std::sync::Mutex::new(None),
        };
        let ctx = sample_ctx();
        let err = do_review_confirmed(&fake, &ctx)
            .err()
            .expect("the injected executor's error must surface");
        assert!(err.to_string().contains("could not be found"));
    }

    // -- advance: the pure flow state machine, with a scripted provider ------
    // and the injectable executor from above --------------------------------

    #[test]
    fn advance_provider_error_yields_failed_from_any_state() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(Err("boom".to_string())),
        );
        assert_eq!(next, FlowState::Failed("boom".to_string()));
    }

    #[test]
    fn advance_good_to_go_proposal_yields_good_to_go_not_proposed() {
        let next = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(Ok(("p1".to_string(), true))),
        );
        assert_eq!(next, FlowState::GoodToGo);
    }

    #[test]
    fn advance_good_to_go_is_a_no_op_target_for_further_events() {
        // GoodToGo is terminal in the same sense Cancelled is: nothing
        // further should move it anywhere else.
        let next = advance(FlowState::GoodToGo, FlowEvent::Confirmed);
        assert_eq!(next, FlowState::GoodToGo);
    }

    #[test]
    fn advance_full_happy_path_proposed_to_executed_with_a_scripted_provider_and_fake_executor() {
        // "Scripted provider": this test plays the provider's role by
        // constructing the Result the real chain would eventually hand
        // `app.rs`, exactly like `actions::calendar`'s own `advance` tests
        // do for `ProviderReturned`.
        let scripted_provider_result: Result<(String, bool), String> =
            Ok(("proposal-1".to_string(), false));

        let s = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(scripted_provider_result),
        );
        assert_eq!(s, FlowState::Proposed("proposal-1".to_string()));

        let s = advance(s, FlowEvent::Shown);
        assert_eq!(s, FlowState::Previewed("proposal-1".to_string()));

        let s = advance(s, FlowEvent::Confirmed);
        assert_eq!(s, FlowState::Confirmed("proposal-1".to_string()));

        // The injectable executor: a fake standing in for the real
        // replace_text executor, run through `do_review_confirmed`.
        let fake = FakeReplaceExecutor {
            result: std::sync::Mutex::new(Some(Ok(crate::executors::Undo::none(
                "applied 2 edit(s)",
            )))),
            seen: std::sync::Mutex::new(None),
        };
        let ctx = sample_ctx();
        let executed: Result<String, String> = do_review_confirmed(&fake, &ctx)
            .map(|undo| undo.summary)
            .map_err(|e| e.to_string());

        let s = advance(s, FlowEvent::Executed(executed));
        assert_eq!(
            s,
            FlowState::Executed {
                summary: "applied 2 edit(s)".to_string()
            }
        );
    }

    #[test]
    fn advance_cancel_from_previewed_yields_cancelled() {
        let s = FlowState::Previewed("p".to_string());
        let s = advance(s, FlowEvent::Cancelled);
        assert_eq!(s, FlowState::Cancelled);
    }

    #[test]
    fn advance_confirmed_with_executor_error_yields_failed() {
        let s = FlowState::Confirmed("p".to_string());
        let s = advance(s, FlowEvent::Executed(Err("target not found".to_string())));
        assert_eq!(s, FlowState::Failed("target not found".to_string()));
    }

    #[test]
    fn advance_an_out_of_order_event_is_a_no_op() {
        let s = FlowState::Proposed("p".to_string());
        let next = advance(s.clone(), FlowEvent::Confirmed);
        assert_eq!(
            next, s,
            "an out-of-order event must leave the state unchanged"
        );
    }

    // -- good_to_go path shows no Do it: proven at the flow level -----------
    // (the card-level assertion -- `show_preview` never called -- belongs
    // to `app.rs`'s own tests, since it needs a real `Card`; this proves
    // the DECISION the flow makes is available before any card exists.)

    #[test]
    fn good_to_go_path_never_reaches_a_previewed_or_confirmed_state() {
        let s = advance(
            FlowState::Cancelled,
            FlowEvent::ProviderReturned(Ok(("p".to_string(), true))),
        );
        assert_eq!(s, FlowState::GoodToGo);
        // Attempting to progress it the way a real "needs_edits" proposal
        // would is a no-op: there is no preview to confirm.
        let s2 = advance(s.clone(), FlowEvent::Shown);
        assert_eq!(s2, s, "GoodToGo must never become Previewed");
    }

    // -- live check (#38): text-only path against a real local Ollama -------
    // `cargo test review_email_live -- --ignored --nocapture`. Requires
    // Ollama running on 127.0.0.1:11434 with `llama3.1:8b` pulled. Text
    // only (no image, `Request::images` empty) -- see
    // `review_request_from_text`'s own doc comment for why this still goes
    // through `Chain::complete_parsed_with_fallback` in production, unused
    // here since this test calls the provider directly.
    //
    // MEASURED 2026-09-17 (llama3.1:8b, this machine): against
    // `BASE_PROMPT`'s first version (no "keep each edit small" or "never a
    // no-op edit" guidance), the model twice produced a plausible but
    // undesired shape -- once merging both typos into a single edit
    // spanning the whole sentence, once adding a THIRD, no-op edit whose
    // "after" was byte-identical to its "before" (a hallucinated
    // "correction" of text that was already fine). Both are real model
    // behaviour, not a bug in this file's parsing/application code (each
    // response still parsed cleanly and `apply_edits` would have handled
    // either shape without erroring). `BASE_PROMPT`'s two added sentences
    // ("keep each edit as small and specific as possible... one edit per
    // individual mistake" and "never propose an edit whose after is
    // identical to its before") fixed both, reproducibly, across the run
    // this test's current assertion is checked against. Latency: 40-51s
    // per request on this machine (CPU, no `OLLAMA_IGPU_ENABLE`) -- this is
    // a text-only request (no image), so the vision-path timing this
    // crate's other live checks report does not apply here.

    #[test]
    #[ignore]
    fn review_email_live_text_only_finds_two_typos() {
        use crate::provider::Provider as _;

        let schema = crate::actions::schema::schema_for("text_review", false)
            .expect("text_review is registered");
        let mut provider = crate::provider::Ollama::new(
            crate::provider::ollama::DEFAULT_BASE_URL,
            "llama3.1:8b",
            "low",
        );
        // Unload the model after this one-off check, per #13's precedent
        // (`ollama_live_answers_a_synthetic_arithmetic_screenshot`).
        provider.keep_alive = "0".to_string();

        let synthetic_email = "Hi Sam,\n\nI wnated to folow up on the proposal we discussed last week. Let me know if you have any questions.\n\nThanks,\nAlex";

        let request = crate::provider::Request {
            system: BASE_PROMPT.to_string(),
            user: synthetic_email.to_string(),
            images: vec![],
            schema: Some(schema),
            effort: crate::provider::Effort::Unset,
            max_tokens: 0,
        };

        let started = std::time::Instant::now();
        let completion = provider
            .complete(&request)
            .expect("live ollama request should succeed");
        let elapsed = started.elapsed();

        let value = parse_text_review_proposal(&completion.text)
            .expect("response should parse as a text_review completion");
        let edits = edits_from_value(&value);

        // MEASURED result and latency are recorded in the commit message
        // and the #38 closing comment, not asserted here beyond the count
        // (the model's exact wording is not a stable thing to assert on).
        eprintln!(
            "review_email_live: model=llama3.1:8b elapsed={:?} edits={} verdict={:?}",
            elapsed,
            edits.len(),
            value.get("verdict")
        );
        assert_eq!(
            edits.len(),
            2,
            "the synthetic email has exactly two typos (\"wnated\", \"folow\"); got {edits:?}"
        );
    }
}
