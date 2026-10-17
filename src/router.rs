//! Intent router (#24): one cheap classification call, started on a worker
//! thread when the options card (#307) opens, that ranks which catalogue
//! actions the screen in front of the user is probably about.
//!
//! Pure module (AGENTS.md rule 8): no `windows` dependency anywhere in this
//! file. `app.rs` owns every piece of Win32/network wiring this needs --
//! capturing the downscaled screenshot on the main thread (mirroring
//! `App::ask`'s own capture-before-spawn ordering), picking and constructing
//! the actual `Provider` to call, spawning the worker thread, and posting the
//! result back to the options card (#307). `ui::card` owns the card-side half:
//! showing the summary and the buttons.
//!
//! See the 2026-09-16 expansion plan §6 "The intent router",
//! `docs/superpowers/specs/2026-09-17-palette-design.md`, and `gh issue view 24`.
//!
//! # What this module does NOT do
//!
//! It never runs an action -- [`RouterResult`] is read-only advice the
//! options card offers and the user picks from. It never decides
//! Mode or Pause; those are `app.rs`'s job before this module's functions are
//! even called (see [`cheapest_router_target`]'s doc for how Mode already
//! folds in via the caller's `ready_provider_names`).

use serde_json::{json, Value};

use crate::provider::{Chain, Effort, NonVisionInputs, Request};
use crate::ui::palette_model::PaletteAction;

/// Long edge (pixels) the router's screenshot is downscaled to before
/// sending -- deliberately far below `config.capture.max_edge`'s default
/// (1568) and the per-provider vision tiers in `capture.rs`: this call only
/// needs to recognize the GENRE of what's on screen (an email compose
/// window, a calendar, a physics problem), not read every character of it,
/// and it runs on every options card rather than only on a confirmed press.
///
/// THEORY (unverified): 512 is a conservative starting point, not a measured
/// legibility/token sweep -- reasoned from `capture.rs`'s own tables (a
/// 512-long-edge PNG is roughly a quarter of the pixel area of the 1568
/// default, so proportionally cheaper to encode, upload and decode) rather
/// than from running the same image through a vision model at several sizes
/// and comparing `intent`/`confidence` quality. Revisit with a real
/// measurement (`router_live_recognizes_email_compose`'s `#[ignore]` test
/// below is the harness to extend for that) before trusting this number past
/// the four Phase-2 catalogue actions it launches with.
pub const ROUTER_IMAGE_LONG_EDGE: u32 = 512;

/// Paired pixel-count budget for [`ROUTER_IMAGE_LONG_EDGE`], passed to
/// `capture::fit_for_model` alongside it. A generous bound for any screen
/// aspect ratio once the long edge is already capped at 512 (a 512x512
/// square is the worst case for total pixels at that long edge), so this
/// never binds tighter than the long-edge cap alone would -- unlike the
/// per-provider tiers in `capture.rs`, which exist specifically to bind
/// before the long-edge cap for wide/short screen aspect ratios.
pub const ROUTER_IMAGE_MAX_PIXELS: u64 =
    (ROUTER_IMAGE_LONG_EDGE as u64) * (ROUTER_IMAGE_LONG_EDGE as u64);

/// Token budget for the router's completion. `{summary, intent, confidence}`
/// is a handful of short fields, about 40-60 tokens of answer.
///
/// The cap also has to cover reasoning: OpenAI `max_output_tokens`, Gemini
/// `maxOutputTokens` and Anthropic `max_tokens` all count thinking tokens, and
/// a reply cut off with no text is the "ran out of room" error (#155). THEORY
/// (unverified, no OpenAI or Gemini key to measure): at `Effort::Low` a small
/// reasoning model can spend more than the old 200 on thinking and return
/// nothing, so the options card would silently get no suggestion and the chain
/// would bill a second provider. MEASURED 2026-10-16 (Anthropic
/// `claude-haiku-5-5`, low effort, a structured calendar reply): 39 to 42
/// output tokens, no thinking. Raising the cap costs nothing when the model
/// does not think, since only generated tokens are billed.
pub const ROUTER_MAX_TOKENS: u32 = 1000;

/// How long the options card waits for the router before it gives up and shows
/// its fallback options. A slow local model that talks on for the whole token
/// budget must not keep the card on "Looking at your screen" for minutes. The call itself still ends on its own HTTP timeout;
/// only the wait is capped.
pub const ROUTER_WALL_CLOCK: std::time::Duration = std::time::Duration::from_secs(10);

/// Waits at most `cap` for the router's result. `None` means the cap passed
/// (or the worker died without answering), and the caller shows no hint.
pub fn await_result<T>(rx: &std::sync::mpsc::Receiver<T>, cap: std::time::Duration) -> Option<T> {
    rx.recv_timeout(cap).ok()
}

/// `palette.router_threshold`'s default (`config.rs`'s `Palette::default`
/// must match this exactly -- `router_threshold_config_default_matches_router_module`
/// below proves it). #24's Done-when doesn't name a number; 0.7 mirrors the
/// expansion plan §6's own worked example confidence (0.86) being comfortably
/// above it while leaving room for a genuinely ambiguous screen to fall
/// below and change nothing.
pub const DEFAULT_ROUTER_THRESHOLD: f64 = 0.7;

/// MEASURED 2026-10-16 (#223, `app::tests::router_live_recognizes_email_compose`,
/// local Ollama `qwen3.5:4b`, the expansion plan's router default): with the
/// earlier one-line prompt, 1 of 4 runs put an action id into `summary` and
/// picked the wrong action. With the field roles stated below (summary is
/// plain prose and never an id, intent is copied exactly or "none"), 5 of 5
/// runs picked `review-this-email` with a prose summary and no echoed id
/// (~14-17 s each on CPU: `/api/ps` showed `size_vram` 0 on this machine, so
/// those times are CPU times). `gemma3:4b` was not re-measured.
///
/// That measurement was of the earlier three-field prompt. The shortlist prompt
/// below is MEASURED 2026-10-16 on one run only (#306, same test, `qwen3.5:4b`,
/// CPU, ~35 s): `review-this-email` first with the label "Review this email to
/// Dana" and a prose summary. THEORY (unverified): it holds 5 of 5 like the old
/// prompt did; it has not been repeated.
const ROUTER_SYSTEM_PREFIX: &str = "You are shown a heavily downscaled screenshot of the user's screen, just before they open an action picker. Fill two fields. \"summary\": one short plain sentence about what is visible on screen, never an action id or action name. \"options\": up to four of the listed actions that suit this screen, best first, an empty list if none fits. Each option has \"intent\": the id of the listed action, copied exactly; \"label\": a short button label of at most six words that names the thing on screen, for example \"Review this email to Dana\"; \"confidence\": a number from 0 to 1. Never explain your reasoning, only report the result. Use plain text only: no markdown and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

/// One candidate action offered to the router: its id and a one-line
/// description (today: [`PaletteAction::name`] -- the catalogue has no
/// separate longer description field to draw from).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterCandidate {
    pub id: String,
    pub description: String,
}

/// Builds the router's candidate list from the palette's own catalogue
/// (`palette_model::catalogue`'s output) -- the router is offered exactly
/// the same rows the palette itself would show, never a separate list that
/// could drift from what Enter can actually dispatch.
pub fn candidates_from_catalogue(catalogue: &[PaletteAction]) -> Vec<RouterCandidate> {
    catalogue
        .iter()
        .map(|a| RouterCandidate {
            id: a.id.clone(),
            description: a.name.clone(),
        })
        .collect()
}

/// The router's JSON-schema, `additionalProperties: false`. Property order
/// is load-bearing (AGENTS.md rule 3): `summary` FIRST, so the model
/// describes what it actually sees before it commits to any option, and
/// inside an option `intent` before `label` before `confidence`, so it names
/// the action before it writes a label for it.
///
/// `intent`'s `enum` is built from `candidate_ids` alone. There is no
/// `"none"` entry: an empty `options` array is how the model says nothing
/// fits. The schema deliberately has no `minItems`/`maxItems` -- providers
/// differ on which array keywords structured output accepts, and a
/// prompt-only limit is not trusted either, so [`parse_router_result`]
/// enforces the cap in code.
pub fn router_schema(candidate_ids: &[String]) -> Value {
    let mut intent = json!({"type": "string"});
    // An empty `enum` is invalid JSON Schema and some providers reject the
    // whole request, so with no candidates the keyword is left out.
    if !candidate_ids.is_empty() {
        intent["enum"] = Value::Array(candidate_ids.iter().map(|id| json!(id)).collect());
    }
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "options": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "intent": intent,
                        "label": {"type": "string"},
                        "confidence": {"type": "number"}
                    },
                    "required": ["intent", "label", "confidence"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["summary", "options"],
        "additionalProperties": false
    })
}

/// Builds the router's `Request`: the downscaled screenshot (`image_png`,
/// already fit to [`ROUTER_IMAGE_LONG_EDGE`]/[`ROUTER_IMAGE_MAX_PIXELS`] by
/// the caller -- this module does no capture or resizing itself, rule 8),
/// the candidate list rendered as one line per action, and
/// [`router_schema`]'s schema. `effort` is `Low` (a classification call has
/// no reason to spend more) and `max_tokens` is [`ROUTER_MAX_TOKENS`].
pub fn build_request(image_png: Vec<u8>, candidates: &[RouterCandidate]) -> Request {
    let candidate_ids: Vec<String> = candidates.iter().map(|c| c.id.clone()).collect();
    let list = if candidates.is_empty() {
        "(no actions available)".to_string()
    } else {
        candidates
            .iter()
            .map(|c| format!("- {}: {}", c.id, c.description))
            .collect::<Vec<_>>()
            .join("\n")
    };
    // #768: the screenshot is data, never instructions. MEASURED 2026-10-16
    // (Anthropic claude-haiku-5-5, low effort, one tiny image): this request
    // and the default screen-check request both came back as a normal
    // completed answer, not a "refusal", with the note in place.
    let note = crate::actions::generic_inputs::UNTRUSTED_SCREEN_NOTE;
    let system = format!("{ROUTER_SYSTEM_PREFIX}\n\nAvailable actions:\n{list}\n\n{note}");
    Request {
        system,
        user: "What does this screen suggest?".to_string(),
        images: vec![image_png],
        schema: Some(router_schema(&candidate_ids)),
        effort: Effort::Low,
        max_tokens: ROUTER_MAX_TOKENS,
    }
}

/// The router's chain: just `provider`, never previewing, and stopping at
/// [`ROUTER_WALL_CLOCK`] after `started` (#792). It bounds only the chain:
/// past that point no further provider call and no repair request is
/// started (and billed). A call already in flight is not interrupted; it
/// ends on its own HTTP timeout.
pub fn router_chain(
    provider: Box<dyn crate::provider::Provider>,
    started: std::time::Instant,
) -> Chain {
    Chain::new(vec![provider])
        .without_prompt()
        .with_deadline(started + ROUTER_WALL_CLOCK)
}

/// Issue #295: runs the router's request through `chain`, the same
/// vision-aware way every other screen action does. Before this function
/// existed, `app.rs`'s `router_worker` built its own `Request` via
/// [`build_request`] and called the chain's plain `complete_parsed`, which
/// never checks a provider's `own_caps().vision` -- so a no-vision provider
/// (a text-only Ollama model, a compat endpoint without vision) still
/// received the real screenshot, unlike `worker`/`calendar_worker`/
/// `review_worker`/`form_fill_worker`, all of which already went through
/// [`crate::provider::Chain::complete_parsed_with_fallback`] (#18/#206).
/// `fallback` is `router_worker`'s OCR-only substitute (no UIA snapshot --
/// see `app.rs`'s `router_non_vision_inputs` doc comment for why); it is
/// only ever invoked when the chain's one provider actually needs it, per
/// `complete_parsed_with_fallback`'s own laziness guarantee.
pub fn route(
    chain: &Chain,
    image_png: Vec<u8>,
    candidates: &[RouterCandidate],
    limits: RouterLimits,
    fallback: impl FnMut() -> anyhow::Result<NonVisionInputs>,
) -> anyhow::Result<RouterResult> {
    let req = build_request(image_png, candidates);
    chain.complete_parsed_with_fallback(&req, fallback, |c| {
        parse_router_result(&c.text, candidates, limits)
    })
}

/// Longest label, in characters, an option may carry. The card button is a
/// fixed width, and the label came from a model.
pub const MAX_LABEL_CHARS: usize = 40;

/// Longest summary line, in characters. Clamped like a label.
pub const MAX_SUMMARY_CHARS: usize = 120;

/// The card's inclusion bar (`options.min_confidence`). Lower than
/// [`DEFAULT_ROUTER_THRESHOLD`] on purpose: the card exists to show
/// alternatives, so at 0.7 it would often hold one option. Small-model
/// confidences are poorly calibrated, so this is a tunable, not a fact.
pub const DEFAULT_MIN_CONFIDENCE: f64 = 0.2;

/// How many options the card shows by default.
pub const DEFAULT_MAX_OPTIONS: usize = 4;

/// The hard cap on options, whatever `options.max_options` says.
pub const MAX_OPTIONS_CAP: usize = 4;

/// The fewest options `options.max_options` may be set to.
pub const MIN_OPTIONS_FLOOR: usize = 2;

/// The two tunables of the shortlist validation, already clamped.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RouterLimits {
    min_confidence: f64,
    max_options: usize,
}

impl RouterLimits {
    /// Clamps a hand-edited config pair: `max_options` to 2..=4, and a
    /// `min_confidence` that is not a number back to the default, one outside
    /// 0..=1 to the nearer end (a safe degradation, never a crash).
    pub fn new(min_confidence: f64, max_options: usize) -> Self {
        let min_confidence = if min_confidence.is_finite() {
            min_confidence.clamp(0.0, 1.0)
        } else {
            DEFAULT_MIN_CONFIDENCE
        };
        Self {
            min_confidence,
            max_options: max_options.clamp(MIN_OPTIONS_FLOOR, MAX_OPTIONS_CAP),
        }
    }
}

impl Default for RouterLimits {
    fn default() -> Self {
        Self::new(DEFAULT_MIN_CONFIDENCE, DEFAULT_MAX_OPTIONS)
    }
}

/// One ranked suggestion: a real catalogue action id, a label written for
/// the screen, and the model's confidence clamped to `0.0..=1.0`.
#[derive(Debug, Clone, PartialEq)]
pub struct RouterOption {
    pub intent: String,
    pub label: String,
    pub confidence: f64,
}

/// The router's parsed, validated answer: a summary of the screen and a
/// ranked shortlist, best first. `options` has 0 to 4 entries and every
/// `intent` is a real candidate id ([`parse_router_result`] drops the rest).
/// Empty means nothing fit; the caller decides what a card shows then
/// ([`with_fallbacks`]).
#[derive(Debug, Clone, PartialEq)]
pub struct RouterResult {
    pub summary: String,
    pub options: Vec<RouterOption>,
}

/// Strips what a button label must not carry: markdown markers, an em dash
/// (rule 11: a colon and a space instead), runs of whitespace, then clamps to
/// `max_chars` characters (never a byte slice: the text came from a model).
///
/// `*` and backticks are dropped anywhere. `_ [ ] | ~` are trimmed only from
/// the ends of a word and `# >` only from its start, so `_email_` and `#1`
/// lose their markers while `snake_case` and `C#` keep theirs.
fn clean_model_line(text: &str, max_chars: usize) -> String {
    let words: Vec<String> = text
        .replace('\u{2014}', ": ")
        .replace(['*', '`'], "")
        .split_whitespace()
        .map(|w| {
            w.trim_start_matches(['#', '>'])
                .trim_matches(['_', '[', ']', '|', '~'])
                .to_string()
        })
        .filter(|w| !w.is_empty())
        .collect();
    crate::provider::sanitize_model_text(&words.join(" "), max_chars)
}

/// Reads one `options` entry, `None` for anything malformed. One bad entry
/// costs only itself, never the whole shortlist.
fn read_option(v: &Value) -> Option<(String, String, f64)> {
    let obj = v.as_object()?;
    let intent = obj.get("intent")?.as_str()?.to_string();
    let label = obj
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let confidence = obj.get("confidence")?.as_f64()?;
    Some((intent, label, confidence))
}

/// Parses a completion produced from [`build_request`]'s schema into a
/// validated shortlist. `candidates` is the SAME list [`router_schema`] was
/// built from, in catalogue order. A router result is advisory, so an odd
/// answer degrades to fewer (or no) options, never an error; only text that
/// is not a JSON object is an error, and the caller turns that into the floor.
///
/// Applied in this order (the spec's "Validation"): drop ids that are not a
/// candidate; drop duplicates keeping the higher confidence; clamp
/// confidence; drop below `limits.min_confidence`; sort by confidence
/// descending (ties keep catalogue order); keep at most `limits.max_options`;
/// clean every label (an empty one becomes the action's catalogue name).
///
/// Lenient input: a reply in the old `{summary, intent, confidence}` shape,
/// which small local models drift to (#223), is read as a one-option list
/// whose label is the action name, with `"none"` meaning no options.
pub fn parse_router_result(
    text: &str,
    candidates: &[RouterCandidate],
    limits: RouterLimits,
) -> anyhow::Result<RouterResult> {
    let value: Value = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("router: completion text is not a valid RouterResult: {e}"))?;
    let Some(obj) = value.as_object() else {
        anyhow::bail!("router: completion text is not a valid RouterResult: not an object");
    };
    let summary = clean_model_line(
        obj.get("summary")
            .and_then(Value::as_str)
            .unwrap_or_default(),
        MAX_SUMMARY_CHARS,
    );

    let raw: Vec<(String, String, f64)> = match obj.get("options").and_then(Value::as_array) {
        Some(items) => items.iter().filter_map(read_option).collect(),
        None => match (
            obj.get("intent").and_then(Value::as_str),
            obj.get("confidence").and_then(Value::as_f64),
        ) {
            (Some(intent), Some(confidence)) => {
                vec![(intent.to_string(), String::new(), confidence)]
            }
            _ => Vec::new(),
        },
    };

    // Steps 1 to 3: real ids only, one entry per id (higher confidence wins),
    // confidence clamped. Slots follow catalogue order so the stable sort
    // below breaks ties by catalogue position.
    let mut best: Vec<Option<RouterOption>> = vec![None; candidates.len()];
    for (intent, label, confidence) in raw {
        let Some(at) = candidates.iter().position(|c| c.id == intent) else {
            continue;
        };
        let confidence = if confidence.is_finite() {
            confidence.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let better = match &best[at] {
            Some(kept) => confidence > kept.confidence,
            None => true,
        };
        if better {
            best[at] = Some(RouterOption {
                intent,
                label,
                confidence,
            });
        }
    }

    let mut options: Vec<RouterOption> = best.into_iter().flatten().collect();
    // Steps 4 to 6.
    options.retain(|o| o.confidence >= limits.min_confidence);
    options.sort_by(|a, b| b.confidence.total_cmp(&a.confidence));
    options.truncate(limits.max_options);
    // Step 7.
    for option in &mut options {
        let label = clean_model_line(&option.label, MAX_LABEL_CHARS);
        option.label = if label.is_empty() {
            candidates
                .iter()
                .find(|c| c.id == option.intent)
                .map(|c| clean_model_line(&c.description, MAX_LABEL_CHARS))
                .unwrap_or_default()
        } else {
            label
        };
    }

    Ok(RouterResult { summary, options })
}

/// Builds a fallback option: the catalogue name as its label, no confidence.
fn catalogue_option(action: &PaletteAction) -> RouterOption {
    RouterOption {
        intent: action.id.clone(),
        label: clean_model_line(&action.name, MAX_LABEL_CHARS),
        confidence: 0.0,
    }
}

/// The floor: after validation the options card must never be empty or a
/// single dead end. With no model ready the router never ran, so the answer
/// is the static list (the catalogue's model-free actions, in catalogue
/// order, capped at [`MAX_OPTIONS_CAP`]). With a model ready and fewer than
/// two options, "Explain this screen" then "Copy text from screen" are
/// appended, skipping any already present or absent from the catalogue
/// (the user may have disabled one). Labels are the catalogue names.
pub fn with_fallbacks(
    mut options: Vec<RouterOption>,
    catalogue: &[PaletteAction],
    model_ready: bool,
) -> Vec<RouterOption> {
    if !model_ready {
        return catalogue
            .iter()
            .filter(|a| !a.requires_model)
            .take(MAX_OPTIONS_CAP)
            .map(catalogue_option)
            .collect();
    }
    if options.len() < 2 {
        for id in [
            crate::actions::explain_screen::ACTION_ID,
            crate::actions::EXTRACT_TEXT_ACTION_ID,
        ] {
            if options.iter().any(|o| o.intent == id) {
                continue;
            }
            if let Some(action) = catalogue.iter().find(|a| a.id == id) {
                options.push(catalogue_option(action));
            }
        }
    }
    options
}

/// The summary shown while the router works (the options card's shell).
pub const LOOKING_SUMMARY: &str = "Looking at your screen";
/// The summary when the model gave none.
pub const PICK_SUMMARY: &str = "What would you like to do?";
/// The summary when the router failed for any reason. The reason goes to the
/// diagnostics log, never the card (the router is advisory).
pub const FAILED_SUMMARY: &str = "Could not read the screen, here are a few options";
/// The summary when no model is set up: these options work without one.
pub const NO_MODEL_SUMMARY: &str = "No AI model is set up. These work without one.";

/// The summary when Ollama runs on this PC but none of its models can read the
/// screen (#1017): the same card, saying why nothing was adopted.
pub const NO_VISION_SUMMARY: &str =
    "Ollama has no model that can read the screen. These work without one.";
/// #1017: the id of the one row on the options card that is not a catalogue
/// action. It opens Settings, runs nothing and bills nothing. It is handled
/// before the palette's dispatch, so it can never reach an action by accident.
pub const SETUP_MODEL_ID: &str = "wingman.setup-model";
/// The label of that row.
pub const SETUP_MODEL_LABEL: &str = "Set up a model";

/// What the options card shows: a summary line and the ranked options.
#[derive(Debug, Clone, PartialEq)]
pub struct OptionsContent {
    pub summary: String,
    pub options: Vec<RouterOption>,
}

/// Decides the options card's content from the router's outcome. `result` is
/// `None` when the router failed (an error, a timeout, a refusal, an unreadable
/// reply). With no model ready the router never ran and the card is the static
/// model-free list. Otherwise the shortlist is topped up by
/// [`with_fallbacks`], so the card is never empty or a single dead end, and
/// capped at [`MAX_OPTIONS_CAP`] whatever came in. Never an error: the router
/// is advisory, and its failure is the card's floor.
pub fn options_content(
    result: Option<&RouterResult>,
    catalogue: &[PaletteAction],
    model_ready: bool,
) -> OptionsContent {
    if !model_ready {
        // #1017: the model-free list gives up its last slot to the way out of
        // this card, so a user with no model is never at a dead end.
        let mut options = with_fallbacks(Vec::new(), catalogue, false);
        options.truncate(MAX_OPTIONS_CAP - 1);
        options.push(RouterOption {
            intent: SETUP_MODEL_ID.to_string(),
            label: SETUP_MODEL_LABEL.to_string(),
            confidence: 0.0,
        });
        return OptionsContent {
            summary: NO_MODEL_SUMMARY.to_string(),
            options,
        };
    }
    let Some(result) = result else {
        return OptionsContent {
            summary: FAILED_SUMMARY.to_string(),
            options: with_fallbacks(Vec::new(), catalogue, true),
        };
    };
    let summary = if result.summary.trim().is_empty() {
        PICK_SUMMARY.to_string()
    } else {
        result.summary.clone()
    };
    let mut options = with_fallbacks(result.options.clone(), catalogue, true);
    options.truncate(MAX_OPTIONS_CAP);
    OptionsContent { summary, options }
}

/// The provider name and model the router should call -- deliberately not
/// necessarily the same model the user configured for the main action (see
/// [`cheapest_router_target`]'s doc for why).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterTarget {
    pub provider: String,
    pub model: String,
}

/// Picks the cheapest ready provider/model for the router's classification
/// call.
///
/// `ready_provider_names` is the Mode-and-readiness-filtered, ordered list a
/// caller already has from `Providers::build_chain_for_mode(mode,
/// ollama_ready).ready_provider_names()` -- the SAME selection `App::ask`
/// and `App::show_options_card` already build for the real action chain, so
/// Mode (Cloud/Local/Auto/Offline) and "is this provider actually ready"
/// (has a key, or Ollama probed reachable) are both already folded in by the
/// time this function runs; it does no Mode logic of its own. An empty list
/// (nothing ready) yields `None` -- the caller's job is to skip the router
/// entirely in that case, no hint shown (issue #24's "skip entirely when no
/// provider is ready").
///
/// Only the FIRST ready provider is considered -- exactly the one the real
/// action chain would try first too -- never a second choice if its model
/// list happens to be empty; a misconfigured entry there just means no
/// router suggestion this press, not a search through the rest of the
/// chain for a worse-but-usable option.
///
/// `models_for(provider_name)` returns that provider's FULL configured model
/// list (`ProviderConfig::models` for a cloud provider; the caller passes a
/// single-element `vec![ollama_model]` for `"ollama"`, which has no list to
/// choose a smaller model from -- see [`smallest_model`]'s doc for why the
/// single Ollama-configured model is used as-is rather than substituted).
pub fn cheapest_router_target(
    ready_provider_names: &[String],
    models_for: impl Fn(&str) -> Vec<String>,
) -> Option<RouterTarget> {
    let name = ready_provider_names.first()?;
    let models = models_for(name);
    let model = smallest_model(&models)?;
    Some(RouterTarget {
        provider: name.clone(),
        model,
    })
}

/// Heuristic "cheapest-sounding" model name from a provider's configured
/// list (`ProviderConfig::models`, authored newest/flagship-first -- see
/// `config.rs`'s `Providers::default` comment on `openai.models`).
///
/// Checks, in priority order, for a name with a small/fast market marker as
/// a whole `-`/`.` separated segment (`nano`, `mini`, `haiku`, `flash` with
/// `lite`, `flash`, `lite`; never a substring, since "mini" is inside
/// "gemini") -- this
/// matches every provider's OWN naming convention for its cheapest tier as
/// configured by `Providers::default` today (`gpt-5.4-nano`,
/// `claude-haiku-4-5`, `gemini-3.8-flash`). Falls back to the LAST list
/// entry when no marker matches (Ollama's single-element list always takes
/// this path, returning the one configured model unchanged), since the
/// lists are authored newest-and-priciest-first.
///
/// A documented naming heuristic, not a `MEASURED`/`THEORY` causal claim
/// (rule 10) -- there is nothing to measure here, only a convention to
/// follow, and it degrades safely (falls back to SOME configured model,
/// never `None`, unless the list itself is empty).
fn smallest_model(models: &[String]) -> Option<String> {
    // Markers are matched as whole name segments, not substrings: "mini" is
    // inside every "gemini" id. `flash-lite` needs both segments, so it ranks
    // above plain `flash`.
    const CHEAP_MARKERS: [&[&str]; 6] = [
        &["nano"],
        &["mini"],
        &["haiku"],
        &["flash", "lite"],
        &["flash"],
        &["lite"],
    ];
    let segments = |m: &str| -> Vec<String> {
        m.to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .map(str::to_string)
            .collect()
    };
    for marker in CHEAP_MARKERS {
        let found = models.iter().find(|m| {
            let segs = segments(m);
            marker.iter().all(|w| segs.iter().any(|s| s == w))
        });
        if let Some(m) = found {
            return Some(m.clone());
        }
    }
    models.last().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, description: &str) -> RouterCandidate {
        RouterCandidate {
            id: id.to_string(),
            description: description.to_string(),
        }
    }

    fn palette_action(id: &str, name: &str) -> PaletteAction {
        PaletteAction {
            id: id.to_string(),
            name: name.to_string(),
            group: None,
            requires_model: true,
            effect: crate::ui::palette_model::Effect::None,
            takes_text: false,
        }
    }

    fn free_action(id: &str, name: &str) -> PaletteAction {
        PaletteAction {
            requires_model: false,
            ..palette_action(id, name)
        }
    }

    fn ids(list: &[RouterCandidate]) -> Vec<String> {
        list.iter().map(|c| c.id.clone()).collect()
    }

    fn option(intent: &str, label: &str, confidence: f64) -> RouterOption {
        RouterOption {
            intent: intent.to_string(),
            label: label.to_string(),
            confidence,
        }
    }

    fn five() -> Vec<RouterCandidate> {
        vec![
            candidate("a", "Action A"),
            candidate("b", "Action B"),
            candidate("c", "Action C"),
            candidate("d", "Action D"),
            candidate("e", "Action E"),
        ]
    }

    fn parse(text: &str, candidates: &[RouterCandidate]) -> RouterResult {
        parse_router_result(text, candidates, RouterLimits::default()).unwrap()
    }

    /// #792: the router's chain sends no repair request once its deadline has
    /// passed. A slow provider returns an unparseable answer after the cap;
    /// it must be asked exactly once.
    #[test]
    fn router_chain_sends_no_repair_after_the_deadline() {
        use crate::provider::{Caps, Completion, Provider, StopReason};
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::sync::Arc;

        struct SlowBadAnswer(Arc<AtomicU32>);
        impl Provider for SlowBadAnswer {
            fn id(&self) -> &'static str {
                "slow"
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps {
                    vision: true,
                    ..Caps::default()
                }
            }
            fn own_caps(&self) -> Caps {
                self.capabilities("")
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                self.0.fetch_add(1, Ordering::SeqCst);
                // Longer than the 400 ms of deadline the second chain has left,
                // so the deadline always passes during this call.
                std::thread::sleep(std::time::Duration::from_millis(450));
                Ok(Completion {
                    text: "not json".to_string(),
                    usage: None,
                    stop: StopReason::Complete,
                })
            }
        }

        let calls = Arc::new(AtomicU32::new(0));
        // Started long enough ago that the deadline has passed by the time
        // the slow answer arrives.
        let Some(started) = std::time::Instant::now().checked_sub(ROUTER_WALL_CLOCK) else {
            return; // machine up for under 10 s: no earlier instant exists
        };
        let chain = router_chain(Box::new(SlowBadAnswer(calls.clone())), started);
        let result = route(
            &chain,
            vec![1],
            &[candidate("a", "A")],
            RouterLimits::default(),
            || Err(anyhow::anyhow!("no fallback expected")),
        );
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "past the deadline: no call at all"
        );

        // 400 ms of deadline left, far more than a loaded machine stalls
        // between here and the first check, and less than the slow call.
        let nearly = ROUTER_WALL_CLOCK - std::time::Duration::from_millis(400);
        let chain = router_chain(
            Box::new(SlowBadAnswer(calls.clone())),
            std::time::Instant::now()
                .checked_sub(nearly)
                .unwrap_or(started),
        );
        let result = route(
            &chain,
            vec![1],
            &[candidate("a", "A")],
            RouterLimits::default(),
            || Err(anyhow::anyhow!("no fallback expected")),
        );
        assert!(result.is_err());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "asked once, never repaired"
        );
    }

    // -- candidates_from_catalogue ------------------------------------------

    #[test]
    fn candidates_from_catalogue_maps_id_and_name() {
        let catalogue = vec![
            palette_action("check-my-work", "Check my work"),
            palette_action("add-to-calendar", "Add to calendar"),
        ];
        let candidates = candidates_from_catalogue(&catalogue);
        assert_eq!(
            candidates,
            vec![
                candidate("check-my-work", "Check my work"),
                candidate("add-to-calendar", "Add to calendar"),
            ]
        );
    }

    // -- router_schema: property order is load-bearing (rule 3) ------------

    fn keys(v: &Value) -> Vec<&str> {
        v.as_object().unwrap().keys().map(|k| k.as_str()).collect()
    }

    #[test]
    fn router_schema_orders_summary_before_options() {
        let schema = router_schema(&["check-my-work".to_string()]);
        assert_eq!(keys(&schema["properties"]), ["summary", "options"]);
    }

    #[test]
    fn router_schema_orders_intent_before_label_before_confidence() {
        let schema = router_schema(&["check-my-work".to_string()]);
        let item = &schema["properties"]["options"]["items"];
        assert_eq!(
            keys(&item["properties"]),
            ["intent", "label", "confidence"],
            "the model names the action before it writes a label for it"
        );
    }

    #[test]
    fn router_schema_intent_enum_equals_the_candidate_ids_with_no_none() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let schema = router_schema(&ids);
        let enum_values: Vec<String> = schema["properties"]["options"]["items"]["properties"]
            ["intent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(enum_values, ids);
    }

    #[test]
    fn router_schema_forbids_additional_properties_and_has_no_array_limits() {
        let schema = router_schema(&[]);
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(schema["required"], json!(["summary", "options"]));
        let options = &schema["properties"]["options"];
        assert_eq!(options["items"]["additionalProperties"], json!(false));
        assert_eq!(
            options["items"]["required"],
            json!(["intent", "label", "confidence"])
        );
        // Providers differ on array keywords (#301): the cap lives in code.
        assert!(options.get("minItems").is_none() && options.get("maxItems").is_none());
    }

    // -- build_request -------------------------------------------------------

    #[test]
    fn build_request_carries_exactly_one_image() {
        let req = build_request(vec![1, 2, 3], &[candidate("a", "A")]);
        assert_eq!(req.images, vec![vec![1u8, 2, 3]]);
    }

    #[test]
    fn build_request_lists_every_candidate_id_and_description_in_the_system_prompt() {
        let candidates = vec![
            candidate("check-my-work", "Check my work"),
            candidate("add-to-calendar", "Add to calendar"),
        ];
        let req = build_request(vec![], &candidates);
        assert!(req.system.contains("check-my-work: Check my work"));
        assert!(req.system.contains("add-to-calendar: Add to calendar"));
    }

    /// #223: small local models echoed action ids into `summary`. The prompt
    /// must state each field's role, and the "summary never names an action"
    /// instruction must come before the candidate list.
    #[test]
    fn build_request_states_the_field_roles_before_the_candidate_list() {
        let req = build_request(vec![], &[candidate("a", "A")]);
        let list_at = req.system.find("Available actions:").unwrap();
        let rules = &req.system[..list_at];
        assert!(rules.contains("summary"), "{rules}");
        assert!(rules.contains("never an action id"), "{rules}");
        assert!(rules.contains("intent"), "{rules}");
        assert!(rules.contains("label"), "{rules}");
        assert!(rules.contains("up to four"), "{rules}");
    }

    /// #768: the screenshot is data, never instructions, for the router too.
    /// The note sits after the candidate list and the schema is untouched.
    #[test]
    fn build_request_carries_the_screen_note_and_keeps_the_schema_order() {
        let req = build_request(vec![], &[candidate("a", "A")]);
        let note = crate::actions::generic_inputs::UNTRUSTED_SCREEN_NOTE;
        assert!(req.system.ends_with(note), "{}", req.system);
        let keys: Vec<&str> = req.schema.as_ref().unwrap()["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        assert_eq!(keys, ["summary", "options"]);
    }

    #[test]
    fn build_request_with_no_candidates_still_builds_a_valid_request() {
        let req = build_request(vec![], &[]);
        assert!(req.system.contains("no actions available"));
        assert!(req.schema.is_some());
    }

    #[test]
    fn await_result_returns_a_prompt_answer_and_none_after_the_cap() {
        use std::time::Duration;
        let (tx, rx) = std::sync::mpsc::channel();
        tx.send(7).unwrap();
        assert_eq!(await_result(&rx, Duration::from_secs(5)), Some(7));
        // Nothing arrives: the sender is alive but silent (a slow model).
        let (_tx2, rx2) = std::sync::mpsc::channel::<i32>();
        let start = std::time::Instant::now();
        assert_eq!(await_result(&rx2, Duration::from_millis(40)), None);
        assert!(start.elapsed() < Duration::from_secs(5));
        // The worker died without sending.
        let (tx3, rx3) = std::sync::mpsc::channel::<i32>();
        drop(tx3);
        assert_eq!(await_result(&rx3, Duration::from_secs(5)), None);
        assert!(ROUTER_WALL_CLOCK <= Duration::from_secs(15));
    }

    /// #725: the cap covers thinking tokens, so it must stay well above the
    /// ~60 tokens of answer (200 was too tight for a reasoning model).
    #[test]
    fn the_router_token_budget_leaves_room_for_reasoning() {
        let req = build_request(vec![], &[candidate("a", "A")]);
        assert!(req.max_tokens >= 1000, "{}", req.max_tokens);
    }

    #[test]
    fn build_request_uses_low_effort_and_the_router_token_budget() {
        let req = build_request(vec![], &[candidate("a", "A")]);
        assert_eq!(req.effort, Effort::Low);
        assert_eq!(req.max_tokens, ROUTER_MAX_TOKENS);
    }

    #[test]
    fn build_request_schema_matches_router_schema_for_the_same_candidates() {
        let candidates = vec![candidate("a", "A")];
        let req = build_request(vec![], &candidates);
        assert_eq!(req.schema, Some(router_schema(&["a".to_string()])));
    }

    // -- parse_router_result: the validation pipeline (#306) -------------------

    /// The #306 fixture: five options, one unknown id, one duplicate, one
    /// below the bar, becomes exactly the expected ranked list.
    #[test]
    fn the_306_fixture_becomes_the_expected_ranked_shortlist() {
        let text = r#"{"summary":"An email to Dana","options":[
            {"intent":"c","label":"Third","confidence":0.41},
            {"intent":"a","label":"First","confidence":0.92},
            {"intent":"nope","label":"Invented","confidence":0.99},
            {"intent":"a","label":"First again","confidence":0.30},
            {"intent":"d","label":"Too low","confidence":0.12},
            {"intent":"b","label":"Second","confidence":0.65}
        ]}"#;
        let result = parse(text, &five());
        assert_eq!(result.summary, "An email to Dana");
        assert_eq!(
            result.options,
            vec![
                option("a", "First", 0.92),
                option("b", "Second", 0.65),
                option("c", "Third", 0.41),
            ]
        );
    }

    #[test]
    fn an_id_that_is_not_a_candidate_is_dropped_never_invented() {
        let text = r#"{"summary":"s","options":[{"intent":"hallucinated","label":"x","confidence":0.95}]}"#;
        assert!(parse(text, &five()).options.is_empty());
    }

    #[test]
    fn a_duplicate_keeps_the_higher_confidence_and_its_label() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"Low","confidence":0.4},
            {"intent":"a","label":"High","confidence":0.8}]}"#;
        assert_eq!(parse(text, &five()).options, vec![option("a", "High", 0.8)]);
        let reversed = r#"{"summary":"s","options":[
            {"intent":"a","label":"High","confidence":0.8},
            {"intent":"a","label":"Low","confidence":0.4}]}"#;
        assert_eq!(
            parse(reversed, &five()).options,
            vec![option("a", "High", 0.8)]
        );
    }

    #[test]
    fn confidence_is_clamped_to_zero_one() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"A","confidence":1.4},
            {"intent":"b","label":"B","confidence":-0.2}]}"#;
        let limits = RouterLimits::new(0.0, 4);
        let result = parse_router_result(text, &five(), limits).unwrap();
        assert_eq!(result.options[0].confidence, 1.0);
        assert_eq!(result.options[1].confidence, 0.0);
    }

    #[test]
    fn options_below_min_confidence_are_dropped_and_the_bar_is_inclusive() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"A","confidence":0.2},
            {"intent":"b","label":"B","confidence":0.19}]}"#;
        let ids: Vec<_> = parse(text, &five())
            .options
            .into_iter()
            .map(|o| o.intent)
            .collect();
        assert_eq!(ids, ["a"]);
    }

    #[test]
    fn ties_keep_catalogue_order_not_reply_order() {
        let text = r#"{"summary":"s","options":[
            {"intent":"c","label":"C","confidence":0.5},
            {"intent":"a","label":"A","confidence":0.5},
            {"intent":"b","label":"B","confidence":0.5}]}"#;
        let got: Vec<_> = parse(text, &five())
            .options
            .into_iter()
            .map(|o| o.intent)
            .collect();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn the_shortlist_is_capped_at_max_options_and_the_cap_is_clamped() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"A","confidence":0.9},
            {"intent":"b","label":"B","confidence":0.8},
            {"intent":"c","label":"C","confidence":0.7},
            {"intent":"d","label":"D","confidence":0.6},
            {"intent":"e","label":"E","confidence":0.5}]}"#;
        assert_eq!(parse(text, &five()).options.len(), 4);
        let two = parse_router_result(text, &five(), RouterLimits::new(0.2, 2)).unwrap();
        assert_eq!(two.options.len(), 2);
        // 1 is clamped up to 2, 99 down to 4.
        let one = parse_router_result(text, &five(), RouterLimits::new(0.2, 1)).unwrap();
        assert_eq!(one.options.len(), 2);
        let many = parse_router_result(text, &five(), RouterLimits::new(0.2, 99)).unwrap();
        assert_eq!(many.options.len(), 4);
    }

    #[test]
    fn router_limits_new_degrades_a_bad_min_confidence_safely() {
        assert_eq!(
            RouterLimits::new(f64::NAN, 4),
            RouterLimits::new(DEFAULT_MIN_CONFIDENCE, 4)
        );
        assert_eq!(RouterLimits::new(7.0, 4), RouterLimits::new(1.0, 4));
        assert_eq!(RouterLimits::new(-1.0, 4), RouterLimits::new(0.0, 4));
        assert_eq!(RouterLimits::default(), RouterLimits::new(0.2, 4));
    }

    #[test]
    fn labels_lose_markdown_and_collapse_whitespace() {
        let text = r#"{"summary":"**Bold** `code`","options":[
            {"intent":"a","label":"  **Review**   this\n_email_ #1 ","confidence":0.9}]}"#;
        let result = parse(text, &five());
        assert_eq!(result.options[0].label, "Review this email 1");
        assert_eq!(result.summary, "Bold code");
    }

    #[test]
    fn inner_underscores_and_hashes_survive_label_cleaning() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"Fix snake_case names in C#","confidence":0.9}]}"#;
        assert_eq!(
            parse(text, &five()).options[0].label,
            "Fix snake_case names in C#"
        );
    }

    #[test]
    fn router_schema_with_no_candidates_has_no_empty_enum() {
        let schema = router_schema(&[]);
        let intent = &schema["properties"]["options"]["items"]["properties"]["intent"];
        assert_eq!(intent, &json!({"type": "string"}));
    }

    #[test]
    fn an_em_dash_in_a_label_becomes_a_colon_and_a_space() {
        let text = format!(
            r#"{{"summary":"s","options":[{{"intent":"a","label":"Reply{}short","confidence":0.9}}]}}"#,
            '\u{2014}'
        );
        let label = parse(&text, &five()).options[0].label.clone();
        assert_eq!(label, "Reply: short");
        assert!(!label.contains('\u{2014}'));
    }

    #[test]
    fn an_empty_or_markdown_only_label_falls_back_to_the_action_name() {
        let text = r#"{"summary":"s","options":[
            {"intent":"a","label":"","confidence":0.9},
            {"intent":"b","label":"***","confidence":0.8},
            {"intent":"c","confidence":0.7}]}"#;
        let labels: Vec<_> = parse(text, &five())
            .options
            .into_iter()
            .map(|o| o.label)
            .collect();
        assert_eq!(labels, ["Action A", "Action B", "Action C"]);
    }

    #[test]
    fn a_multibyte_label_is_clamped_on_a_char_boundary_and_never_panics() {
        let long = "\u{1F600}\u{00E9}".repeat(60);
        let text = format!(
            r#"{{"summary":"{long}","options":[{{"intent":"a","label":"{long}","confidence":0.9}}]}}"#
        );
        let result = parse(&text, &five());
        assert!(result.options[0].label.chars().count() <= MAX_LABEL_CHARS);
        assert!(result.options[0].label.ends_with('\u{2026}'));
        assert!(result.summary.chars().count() <= MAX_SUMMARY_CHARS);
    }

    #[test]
    fn the_old_single_intent_shape_reads_as_one_option_labelled_with_the_name() {
        let text = r#"{"summary":"A physics problem","intent":"b","confidence":0.9}"#;
        let result = parse(text, &five());
        assert_eq!(result.summary, "A physics problem");
        assert_eq!(result.options, vec![option("b", "Action B", 0.9)]);
    }

    #[test]
    fn the_old_shape_with_none_or_an_unknown_id_is_an_empty_shortlist() {
        let none = r#"{"summary":"blank","intent":"none","confidence":0.1}"#;
        assert!(parse(none, &five()).options.is_empty());
        let unknown = r#"{"summary":"?","intent":"hallucinated-id","confidence":0.95}"#;
        assert!(parse(unknown, &five()).options.is_empty());
    }

    #[test]
    fn an_empty_options_array_is_an_empty_shortlist_not_an_error() {
        let result = parse(r#"{"summary":"nothing fits","options":[]}"#, &five());
        assert_eq!(result.summary, "nothing fits");
        assert!(result.options.is_empty());
    }

    #[test]
    fn a_malformed_entry_costs_only_itself() {
        let text = r#"{"summary":"s","options":[
            42, "a", {"intent":7,"label":"x","confidence":0.9},
            {"intent":"a","label":"x"},
            {"intent":"b","label":"Good","confidence":0.9}]}"#;
        assert_eq!(parse(text, &five()).options, vec![option("b", "Good", 0.9)]);
    }

    #[test]
    fn a_missing_summary_is_empty_and_no_candidates_means_no_options() {
        let text = r#"{"options":[{"intent":"a","label":"A","confidence":0.9}]}"#;
        assert_eq!(parse(text, &five()).summary, "");
        assert!(parse(text, &[]).options.is_empty());
    }

    #[test]
    fn parse_router_result_rejects_malformed_json_and_non_objects() {
        for bad in ["not json", "[]", "\"text\"", "7", ""] {
            let err = parse_router_result(bad, &five(), RouterLimits::default()).unwrap_err();
            assert!(
                err.to_string().contains("not a valid RouterResult"),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_schema_enum_and_the_parser_use_the_same_candidate_list() {
        let c = five();
        let schema = router_schema(&ids(&c));
        let enum_len = schema["properties"]["options"]["items"]["properties"]["intent"]["enum"]
            .as_array()
            .unwrap()
            .len();
        assert_eq!(enum_len, c.len());
    }

    // -- with_fallbacks: the floor ------------------------------------------

    fn catalogue() -> Vec<PaletteAction> {
        vec![
            palette_action("check-my-work", "Check my work"),
            palette_action(
                crate::actions::explain_screen::ACTION_ID,
                "Explain this screen",
            ),
            free_action(
                crate::actions::EXTRACT_TEXT_ACTION_ID,
                "Copy text from screen",
            ),
            free_action("calculate-selection", "Calculate selection"),
            free_action("copy-region", "Copy region to clipboard"),
            free_action("f1", "Free one"),
            free_action("f2", "Free two"),
        ]
    }

    fn intents(options: &[RouterOption]) -> Vec<&str> {
        options.iter().map(|o| o.intent.as_str()).collect()
    }

    #[test]
    fn an_empty_shortlist_gets_explain_then_copy_text() {
        let out = with_fallbacks(vec![], &catalogue(), true);
        assert_eq!(
            intents(&out),
            [
                crate::actions::explain_screen::ACTION_ID,
                crate::actions::EXTRACT_TEXT_ACTION_ID
            ]
        );
        assert_eq!(out[0].label, "Explain this screen");
    }

    #[test]
    fn one_option_gets_both_fallbacks_appended_after_it() {
        let out = with_fallbacks(
            vec![option("check-my-work", "Mine", 0.5)],
            &catalogue(),
            true,
        );
        assert_eq!(
            intents(&out),
            [
                "check-my-work",
                crate::actions::explain_screen::ACTION_ID,
                crate::actions::EXTRACT_TEXT_ACTION_ID
            ]
        );
        assert_eq!(out[0].label, "Mine", "model labels are untouched");
    }

    #[test]
    fn fallbacks_skip_what_is_already_present() {
        let have = vec![option(
            crate::actions::explain_screen::ACTION_ID,
            "Explain",
            0.5,
        )];
        let out = with_fallbacks(have, &catalogue(), true);
        assert_eq!(
            intents(&out),
            [
                crate::actions::explain_screen::ACTION_ID,
                crate::actions::EXTRACT_TEXT_ACTION_ID
            ]
        );
    }

    #[test]
    fn fallbacks_skip_an_action_the_user_removed_from_the_catalogue() {
        let mut cat = catalogue();
        cat.retain(|a| a.id != crate::actions::explain_screen::ACTION_ID);
        let out = with_fallbacks(vec![], &cat, true);
        assert_eq!(intents(&out), [crate::actions::EXTRACT_TEXT_ACTION_ID]);
        assert!(with_fallbacks(vec![], &[], true).is_empty());
    }

    #[test]
    fn two_or_more_options_are_left_alone() {
        let have = vec![option("x", "X", 0.9), option("y", "Y", 0.8)];
        assert_eq!(with_fallbacks(have.clone(), &catalogue(), true), have);
    }

    #[test]
    fn with_no_model_the_answer_is_the_static_model_free_list_capped_at_four() {
        let out = with_fallbacks(vec![option("x", "X", 0.9)], &catalogue(), false);
        assert_eq!(
            intents(&out),
            [
                crate::actions::EXTRACT_TEXT_ACTION_ID,
                "calculate-selection",
                "copy-region",
                "f1"
            ]
        );
        assert!(out.iter().all(|o| !o.label.is_empty()));
    }

    // -- options_content: what the options card shows (#307) -----------------

    fn routed(summary: &str, options: Vec<RouterOption>) -> RouterResult {
        RouterResult {
            summary: summary.to_string(),
            options,
        }
    }

    #[test]
    fn a_good_shortlist_is_shown_as_the_model_wrote_it() {
        let result = routed(
            "Compose window to Dana",
            vec![
                option("check-my-work", "Review", 0.9),
                option("f1", "Free", 0.5),
            ],
        );
        let c = options_content(Some(&result), &catalogue(), true);
        assert_eq!(c.summary, "Compose window to Dana");
        assert_eq!(intents(&c.options), ["check-my-work", "f1"]);
        assert_eq!(c.options[0].label, "Review");
    }

    #[test]
    fn a_thin_shortlist_is_topped_up_with_the_floor_and_keeps_the_summary() {
        let result = routed("A spreadsheet", vec![option("check-my-work", "Check", 0.4)]);
        let c = options_content(Some(&result), &catalogue(), true);
        assert_eq!(c.summary, "A spreadsheet");
        assert_eq!(
            intents(&c.options),
            [
                "check-my-work",
                crate::actions::explain_screen::ACTION_ID,
                crate::actions::EXTRACT_TEXT_ACTION_ID
            ]
        );
    }

    #[test]
    fn an_empty_shortlist_still_gives_a_card_with_a_useful_summary() {
        let result = routed("", vec![]);
        let c = options_content(Some(&result), &catalogue(), true);
        assert_eq!(c.summary, PICK_SUMMARY, "no model words: a plain prompt");
        assert_eq!(c.options.len(), 2);
    }

    #[test]
    fn a_failed_router_shows_the_floor_with_the_failure_summary() {
        let c = options_content(None, &catalogue(), true);
        assert_eq!(c.summary, FAILED_SUMMARY);
        assert_eq!(
            intents(&c.options),
            [
                crate::actions::explain_screen::ACTION_ID,
                crate::actions::EXTRACT_TEXT_ACTION_ID
            ]
        );
    }

    #[test]
    fn with_no_model_the_card_is_the_static_list_whatever_the_router_said() {
        let result = routed("ignored", vec![option("check-my-work", "Check", 0.9)]);
        for r in [Some(&result), None] {
            let c = options_content(r, &catalogue(), false);
            assert_eq!(c.summary, NO_MODEL_SUMMARY);
            // #1017: three model-free actions, then the way into Settings.
            assert_eq!(
                intents(&c.options),
                [
                    crate::actions::EXTRACT_TEXT_ACTION_ID,
                    "calculate-selection",
                    "copy-region",
                    SETUP_MODEL_ID
                ]
            );
        }
    }

    #[test]
    fn with_no_model_the_last_row_is_the_setup_row_and_only_there() {
        let c = options_content(None, &catalogue(), false);
        let last = c.options.last().expect("rows");
        assert_eq!(last.intent, SETUP_MODEL_ID);
        assert_eq!(last.label, SETUP_MODEL_LABEL);
        assert!(c.options.len() <= MAX_OPTIONS_CAP);
        // A model is ready: the row has no place on the card.
        for r in [None, Some(&routed("S", vec![option("f1", "F1", 0.9)]))] {
            let c = options_content(r, &catalogue(), true);
            assert!(c.options.iter().all(|o| o.intent != SETUP_MODEL_ID));
        }
        // With no model-free action at all the setup row still stands alone.
        let c = options_content(None, &[], false);
        assert_eq!(intents(&c.options), [SETUP_MODEL_ID]);
    }

    #[test]
    fn the_setup_row_id_is_not_a_catalogue_action() {
        assert!(catalogue().iter().all(|a| a.id != SETUP_MODEL_ID));
        assert!(!SETUP_MODEL_LABEL.contains('\u{2014}'));
        assert!(!NO_VISION_SUMMARY.contains('\u{2014}'));
        assert!(NO_VISION_SUMMARY.chars().count() <= MAX_SUMMARY_CHARS);
    }

    #[test]
    fn the_card_never_has_more_than_four_options_or_an_empty_label() {
        let many = (0..9)
            .map(|n| option(&format!("a{n}"), &format!("Label {n}"), 0.9))
            .collect::<Vec<_>>();
        let c = options_content(Some(&routed("S", many)), &catalogue(), true);
        assert!(c.options.len() <= crate::options_keys::MAX_OPTIONS);
        for model in [true, false] {
            let c = options_content(None, &catalogue(), model);
            assert!(c.options.iter().all(|o| !o.label.trim().is_empty()));
        }
    }

    #[test]
    fn card_summaries_have_no_em_dash_and_fit_the_summary_cap() {
        for text in [
            LOOKING_SUMMARY,
            PICK_SUMMARY,
            FAILED_SUMMARY,
            NO_MODEL_SUMMARY,
            NO_VISION_SUMMARY,
        ] {
            assert!(!text.contains('\u{2014}'), "{text}");
            assert!(text.chars().count() <= MAX_SUMMARY_CHARS, "{text}");
        }
    }

    // -- cheapest_router_target -----------------------------------------------

    #[test]
    fn cheapest_router_target_none_when_nothing_is_ready() {
        assert_eq!(cheapest_router_target(&[], |_| vec![]), None);
    }

    #[test]
    fn cheapest_router_target_none_when_the_first_ready_provider_has_no_models() {
        let ready = vec!["openai".to_string()];
        assert_eq!(cheapest_router_target(&ready, |_| vec![]), None);
    }

    #[test]
    fn cheapest_router_target_picks_the_nano_tier_openai_model() {
        let ready = vec!["openai".to_string()];
        let models = |name: &str| {
            assert_eq!(name, "openai");
            vec![
                "gpt-5.5".to_string(),
                "gpt-5.5-pro".to_string(),
                "gpt-5.4".to_string(),
                "gpt-5.4-mini".to_string(),
                "gpt-5.4-nano".to_string(),
                "gpt-5.2".to_string(),
            ]
        };
        assert_eq!(
            cheapest_router_target(&ready, models),
            Some(RouterTarget {
                provider: "openai".to_string(),
                model: "gpt-5.4-nano".to_string(),
            })
        );
    }

    #[test]
    fn cheapest_router_target_picks_the_haiku_tier_anthropic_model() {
        let ready = vec!["anthropic".to_string()];
        let models = |_: &str| {
            vec![
                "claude-opus-5".to_string(),
                "claude-sonnet-5".to_string(),
                "claude-opus-4-8".to_string(),
                "claude-haiku-4-5".to_string(),
                "claude-fable-5-1".to_string(),
            ]
        };
        assert_eq!(
            cheapest_router_target(&ready, models),
            Some(RouterTarget {
                provider: "anthropic".to_string(),
                model: "claude-haiku-4-5".to_string(),
            })
        );
    }

    #[test]
    fn cheapest_router_target_picks_the_flash_tier_gemini_model() {
        let ready = vec!["gemini".to_string()];
        let models = |_: &str| {
            vec![
                "gemini-3.8-flash".to_string(),
                "gemini-3.1-pro-preview".to_string(),
                "gemini-3.6-flash".to_string(),
                "gemini-2.5-pro".to_string(),
                "gemini-2.5-flash".to_string(),
            ]
        };
        assert_eq!(
            cheapest_router_target(&ready, models).unwrap().model,
            "gemini-3.8-flash"
        );
    }

    #[test]
    fn smallest_model_does_not_match_mini_inside_gemini_ids() {
        // A pro model listed first must not win through the "mini" in "gemini".
        let models = vec![
            "gemini-3.1-pro-preview".to_string(),
            "gemini-2.5-pro".to_string(),
            "gemini-2.5-flash".to_string(),
        ];
        assert_eq!(smallest_model(&models).unwrap(), "gemini-2.5-flash");
        let lite_first = vec![
            "gemini-3.1-pro-preview".to_string(),
            "gemini-2.5-flash".to_string(),
            "gemini-2.5-flash-lite".to_string(),
        ];
        assert_eq!(
            smallest_model(&lite_first).unwrap(),
            "gemini-2.5-flash-lite"
        );
        // Real "mini" and "nano" segments still match.
        let openai = vec!["gpt-5.4".to_string(), "gpt-5.4-mini".to_string()];
        assert_eq!(smallest_model(&openai).unwrap(), "gpt-5.4-mini");
    }

    #[test]
    fn cheapest_router_target_uses_the_single_configured_ollama_model_unchanged() {
        let ready = vec!["ollama".to_string()];
        let models = |name: &str| {
            assert_eq!(name, "ollama");
            vec!["gemma3:4b".to_string()]
        };
        assert_eq!(
            cheapest_router_target(&ready, models),
            Some(RouterTarget {
                provider: "ollama".to_string(),
                model: "gemma3:4b".to_string(),
            })
        );
    }

    #[test]
    fn cheapest_router_target_falls_back_to_the_last_entry_when_no_marker_matches() {
        let ready = vec!["compat:custom".to_string()];
        let models = |_: &str| {
            vec![
                "big-model-v1".to_string(),
                "medium-model-v1".to_string(),
                "small-model-v1".to_string(),
            ]
        };
        assert_eq!(
            cheapest_router_target(&ready, models).unwrap().model,
            "small-model-v1"
        );
    }

    #[test]
    fn cheapest_router_target_only_considers_the_first_ready_provider() {
        // "ollama" is ready first; a non-empty model list for the SECOND
        // entry must never be consulted, even if the first's happens to be
        // empty -- the caller's job is to skip the router that press, not
        // silently fall back to a worse provider than the real action chain
        // would have tried.
        let ready = vec!["ollama".to_string(), "openai".to_string()];
        let models = |name: &str| {
            if name == "ollama" {
                vec![]
            } else {
                vec!["gpt-5.4-nano".to_string()]
            }
        };
        assert_eq!(cheapest_router_target(&ready, models), None);
    }

    // -- provider selection by mode (#24, via mode::select_providers) --------

    #[test]
    fn provider_selection_by_mode_cloud_never_picks_ollama() {
        let configured = vec!["ollama".to_string(), "openai".to_string()];
        let ready = crate::mode::select_providers(crate::mode::Mode::Cloud, &configured, true, &[]);
        let target = cheapest_router_target(&ready, |name| {
            if name == "openai" {
                vec!["gpt-5.4-nano".to_string()]
            } else {
                vec!["gemma3:4b".to_string()]
            }
        });
        assert_eq!(target.unwrap().provider, "openai");
    }

    #[test]
    fn provider_selection_by_mode_local_only_ever_picks_ollama() {
        let configured = vec!["openai".to_string(), "ollama".to_string()];
        let ready = crate::mode::select_providers(crate::mode::Mode::Local, &configured, true, &[]);
        let target = cheapest_router_target(&ready, |name| {
            if name == "ollama" {
                vec!["gemma3:4b".to_string()]
            } else {
                vec!["gpt-5.4-nano".to_string()]
            }
        });
        assert_eq!(target.unwrap().provider, "ollama");
    }

    #[test]
    fn provider_selection_by_mode_auto_prefers_ollama_only_when_ready() {
        let configured = vec!["openai".to_string(), "ollama".to_string()];
        let models = |name: &str| {
            if name == "ollama" {
                vec!["gemma3:4b".to_string()]
            } else {
                vec!["gpt-5.4-nano".to_string()]
            }
        };

        let ollama_ready =
            crate::mode::select_providers(crate::mode::Mode::Auto, &configured, true, &[]);
        assert_eq!(
            cheapest_router_target(&ollama_ready, models)
                .unwrap()
                .provider,
            "ollama"
        );

        let ollama_not_ready =
            crate::mode::select_providers(crate::mode::Mode::Auto, &configured, false, &[]);
        assert_eq!(
            cheapest_router_target(&ollama_not_ready, models)
                .unwrap()
                .provider,
            "openai"
        );
    }

    #[test]
    fn provider_selection_by_mode_offline_with_nothing_local_configured_skips_the_router() {
        let configured = vec!["openai".to_string()];
        let ready =
            crate::mode::select_providers(crate::mode::Mode::Offline, &configured, false, &[]);
        assert_eq!(cheapest_router_target(&ready, |_| vec![]), None);
    }

    // -- constants agree with config.rs's default (see that module's own test) -

    #[test]
    fn default_threshold_is_0_7() {
        assert_eq!(DEFAULT_ROUTER_THRESHOLD, 0.7);
    }
}
