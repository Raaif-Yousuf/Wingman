// #19: `pub(crate)` (not private) so `mode::probe_ollama_ready` can reach
// `common::get_text_with_timeout` -- the Offline guard's enforcement lives
// in `common.rs` (the one file every provider's HTTP send already funnels
// through), and the Ollama-readiness probe needs to funnel through the
// exact same guarded path rather than opening its own socket.
pub mod anthropic;
pub(crate) mod common;
pub mod gemini;
pub mod ollama;
pub mod ollama_admin;
pub mod openai;
pub mod openai_compat;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

pub use anthropic::Anthropic;
pub use gemini::Gemini;
pub use ollama::Ollama;
pub use openai::OpenAi;
pub use openai_compat::OpenAiCompat;
/// The system prompt. The user solves physics and statistics problems on paper,
/// then screenshots the on-screen assignment to check their result before
/// entering it. The screenshot shows the problem — not, usually, their working.
pub const DEFAULT_PROMPT: &str = "You are shown a screenshot of the user's screen. They are working through a physics or statistics problem and want a second opinion before committing an answer. Usually the problem statement is on screen (often an online assignment) while the user has done the working on paper, so their derivation is generally NOT visible to you. Sometimes a value they are about to submit is already typed into an input field, and sometimes their working is on screen too.

Work the problem out yourself from what is visible, then:
- If a candidate answer is visible (typed into a field, or written in on-screen working), compare it against your own result. Say plainly whether it matches. If it does not, give the correct value and name the specific mistake you can infer (e.g. \"that is cos 30, not sin 30\" or \"you used the population variance formula, not the sample one\").
- If only the problem is visible, just give your answer.
- If something essential is unreadable or missing from the screenshot, say exactly what you need instead of guessing at it.

Carry units through and give the final value to a sensible number of significant figures.

Respond with exactly two fields, and write them in this order:
- detail: FIRST. At most 700 characters, plain text. Show the worked solution step by step, so the user can check it against their own. Write this out in full before you write the headline, so that the headline states the conclusion this working actually reaches.
- headline: SECOND, and it must be the conclusion of the working you just wrote. At most 90 characters, plain text. Lead with the final value, or with the correction if a visible answer is wrong. Never state a verdict in the headline that your own detail contradicts; if the working changed your mind, the headline follows the working.

Use plain text only in both fields: no markdown (no asterisks, backticks, headers or bullet characters) and no LaTeX. This renders in a plain GDI text window that can display neither. Write powers as m/s^2 and fractions inline.";

/// Appended programmatically to the system prompt when the difficulty toggle
/// is on — never folded into `DEFAULT_PROMPT` itself, because the user edits
/// that text in Settings and it must stay exactly theirs.
///
/// Worded to rate the PROBLEM shown on screen, not the model's own answer and
/// not its confidence, and states the 1/3/5/7/9/10/Ultra anchors explicitly
/// so the model uses the full range instead of clustering on a few values.
pub const DIFFICULTY_RUBRIC: &str = "

Also rate how difficult the PROBLEM ON SCREEN is for a HUMAN STUDENT. Add a third field:
- difficulty: THIRD, after headline, once you have actually worked the problem through. Exactly one of \"1\" through \"10\", or \"U\".

Calibration is the hard part, so read this carefully. You solve nearly all of these easily; that is NOT the scale. Do not rate your own confidence, your own effort, or how quickly you found the answer. Rate how hard the problem would be for a student at the level it is aimed at. Rating by your own effort compresses everything into 1-5 and makes the whole scale useless.

Anchors:
1 = an easy high-school question. One step, one formula. (speed = distance / time)
2 = high-school, a couple of steps.
3 = easy university intro-course level. (a block on an incline; moment of inertia of a disk)
4 = intro university, several steps or a small subtlety.
5 = medium university level. Mid-degree material: multi-step, and you must choose the method rather than being told it.
6 = upper-undergraduate, harder than routine homework.
7 = hard university level. Typically GRADUATE coursework: quantum perturbation theory, Lagrangian mechanics with constraints, a non-obvious statistical derivation.
8 = graduate coursework that most of the class would get wrong.
9 = very hard for an undergraduate. Qualifying-exam standard.
10 = a PhD student in the field would struggle. Open-ended derivations and proofs requiring a specialist technique, not just more algebra.
U = Ultra: a professor would struggle. Research-level, or a known-hard proof.

Use the WHOLE range. Most routine homework is 2-5. If the problem is recognisably graduate-level, it starts at 7, not 5. If it asks you to PROVE a general theorem rather than compute a value, it is almost never below 8.

If there is no problem to rate at all -- the screen shows no question, or you are asking for something to be made visible -- answer \"N\". Do NOT reach for \"U\" in that case: \"U\" means the problem is extraordinarily hard, not that you could not find one.";

/// A 1-10 rating, or `Ultra` for "a professor would struggle". Pure data —
/// the colour mapping lives in the card, not here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Difficulty {
    /// Invariant: 1..=10.
    Level(u8),
    Ultra,
}

impl Difficulty {
    /// Parses what the model returns: "1".."10" or "U"/"ultra"
    /// (case-insensitive, surrounding whitespace tolerated). Returns `None`
    /// for anything else — a bad value must degrade to "no badge", never to
    /// a wrong badge.
    /// `"N"` is the model's explicit "nothing to rate here" answer and maps
    /// to `None`, like any other unrecognised value. It exists because the
    /// schema makes `difficulty` required: without an escape hatch a screen
    /// with no problem on it still gets a rating, and the model reached for
    /// `"U"` -- a purple "a professor would struggle" badge on a screenshot
    /// of a terminal.
    pub fn parse(s: &str) -> Option<Difficulty> {
        let t = s.trim();
        if t.eq_ignore_ascii_case("u") || t.eq_ignore_ascii_case("ultra") {
            return Some(Difficulty::Ultra);
        }
        // Parse as u32 first so a huge number (would overflow a u8) fails
        // cleanly via the range check below rather than via a silent
        // wrapping cast.
        let n: u32 = t.parse().ok()?;
        if (1..=10).contains(&n) {
            Some(Difficulty::Level(n as u8))
        } else {
            None
        }
    }

    /// Badge text: "1".."10", "U".
    pub fn label(&self) -> &'static str {
        match self {
            Difficulty::Ultra => "U",
            Difficulty::Level(n) => match n {
                1 => "1",
                2 => "2",
                3 => "3",
                4 => "4",
                5 => "5",
                6 => "6",
                7 => "7",
                8 => "8",
                9 => "9",
                10 => "10",
                _ => unreachable!("Difficulty::Level invariant is 1..=10"),
            },
        }
    }

    /// 1..=11, where Ultra is 11. Lets the card position a colour on the
    /// gradient without matching on the variant.
    pub fn rank(&self) -> u8 {
        match self {
            Difficulty::Level(n) => *n,
            Difficulty::Ultra => 11,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Answer {
    /// <= 700 chars of plain-text working. May be empty.
    pub detail: String,
    /// <= 90 chars. Leads with the final value or the correction.
    pub headline: String,
    /// `None` when the rating was not requested, or when the model returned
    /// something unparseable.
    pub difficulty: Option<Difficulty>,
}

#[derive(Debug)]
pub struct Shot {
    pub png: Vec<u8>,
    // Carried alongside the bytes because a provider that wants to reason
    // about resolution shouldn't have to re-decode the PNG to get it.
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

/// Raw PNG bytes for one image in a [`Request`].
pub type Png = Vec<u8>;

/// How hard a provider should think about a [`Request`]. `Unset` means "use
/// whatever this provider is configured with" -- both Phase 0 providers keep
/// their own configured default (`ProviderConfig::effort` in `config.rs`,
/// parsed by [`Effort::parse`]) and only honour an explicit override here.
/// Phase 2 actions that want to spend more or less on a particular request
/// than the user's global default will set this directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Effort {
    #[default]
    Unset,
    Low,
    Medium,
    High,
}

impl Effort {
    /// Parses a config string. Anything unrecognised -- including empty or
    /// whitespace-only, a plausible hand-edit of config.toml -- is `Unset`,
    /// matching the pre-existing "empty effort is never sent" behaviour of
    /// both providers (#154).
    pub fn parse(s: &str) -> Effort {
        match s.trim().to_ascii_lowercase().as_str() {
            "low" => Effort::Low,
            "medium" => Effort::Medium,
            "high" => Effort::High,
            _ => Effort::Unset,
        }
    }

    /// The wire string, or `None` for `Unset` (meaning: omit the field
    /// entirely, never send an empty string -- see #154).
    pub fn as_str(&self) -> Option<&'static str> {
        match self {
            Effort::Unset => None,
            Effort::Low => Some("low"),
            Effort::Medium => Some("medium"),
            Effort::High => Some("high"),
        }
    }
}

/// What a provider (for a given model) can do. Read by the intent router and
/// action picker in Phase 2 to route around a provider that can't serve a
/// given action -- e.g. a text-only local model still runs a screen action
/// by falling back to OCR text plus the UIA tree instead of the screenshot
/// (see the 2026-09-16 expansion plan, "Provider trait, extended").
///
/// `image_limits` is issue #169's addition and IS read in production today
/// (via [`Provider::own_caps`] / [`Chain::first_ready_caps`] and
/// `App::ask`'s capture call site) -- everything else on this type is still
/// only read by tests, unused until Phase 2's router and action picker exist
/// to consult it (same status as `Shot`'s `width`/`height` below and
/// `Chain::provider_names`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub vision: bool,
    pub json_schema: bool,
    pub thinking: bool,
    /// This model's real image-size limit, when known -- see
    /// [`ImageLimits`]. `None` means "not known for this provider/model",
    /// which the caller (`capture::resolve_limits`) treats as "fall back to
    /// the user's own `config.capture.max_edge` heuristic", never as "no
    /// limit at all".
    pub image_limits: Option<ImageLimits>,
}

/// A provider's real per-model image-size limit (issue #169): the same
/// two-constraint shape [`crate::capture::fit_for_model`] already fits
/// against -- a maximum long edge in pixels, and a maximum total pixel
/// count (the `u64` avoids an overflow computing `width * height` for a
/// generous budget). Each provider's `capabilities()` fills this in from
/// the constants in `capture.rs` (Anthropic's standard/high-res tiers,
/// OpenAI's tile budget) or a documented/conservative default -- see each
/// provider file's `capabilities()` doc comment for the source and its
/// MEASURED/THEORY status (CLAUDE.md rule 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageLimits {
    pub max_long_edge: u32,
    pub max_pixels: u64,
}

/// Token accounting, when a provider's response reports it. Feeds
/// `usage.rs` (Phase 2, not built yet); `None` for a provider or response
/// that doesn't carry it, never a guessed value.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// Why the provider stopped. A refusal, or an empty completion caused by
/// running out of budget, is surfaced as `Err` before a `Completion` ever
/// exists (see each provider's response parsing), so in practice this only
/// distinguishes a clean finish from "hit the token budget but still said
/// something".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    Complete,
    MaxTokens,
    Other,
}

/// A single-turn request. There is no multi-turn: Wingman is "no chat"
/// (CLAUDE.md, owner decision 2026-09-16) — every `Request` is the whole
/// conversation, always exactly one user turn, and a `Provider` never sees
/// history.
#[derive(Debug, Clone)]
pub struct Request {
    /// The system/instructions text.
    pub system: String,
    /// The one user turn.
    pub user: String,
    /// Zero or more images alongside `user`. Built once per press with the
    /// real screenshot attached. Issue #18/#206:
    /// [`Chain::complete_parsed_with_fallback`] is what actually decides,
    /// PER PROVIDER inside its own fallback loop, whether a given provider
    /// gets this image or a text-only variant built by
    /// [`non_vision_request`] instead -- nothing upstream of `Chain` needs
    /// to know in advance that a provider lacks vision.
    pub images: Vec<Png>,
    /// The JSON Schema the completion text must satisfy, or `None` for a
    /// plain-text answer. Never hard-coded inside a provider -- see
    /// [`physics_request`] for the one schema Wingman uses today, and the
    /// 2026-09-16 expansion plan's "Provider trait, extended" for why this
    /// has to be a `Request` field rather than provider-side knowledge.
    pub schema: Option<Value>,
    /// `Unset` defers to the provider's own configured default effort.
    pub effort: Effort,
    /// `0` defers to the provider's own default token budget.
    pub max_tokens: u32,
}

/// What a provider returns for one [`Request`]. `text` satisfies
/// `Request::schema` when one was given, and is the plain answer otherwise.
/// A provider never attempts to interpret `text` itself -- see
/// [`parse_answer`] for the one place that happens today.
///
/// `usage`/`stop` are populated by both providers already (see each
/// `parse_completion`) but nothing reads them back yet -- `usage.rs` and the
/// action-preview "hit the token budget" card are Phase 2.
#[derive(Debug, Clone)]
pub struct Completion {
    pub text: String,
    #[allow(dead_code)]
    pub usage: Option<Usage>,
    #[allow(dead_code)]
    pub stop: StopReason,
}

/// Issue #18/#206: the ingredients [`non_vision_request`] substitutes for
/// the screenshot when a provider in a [`Chain`] reports `Caps.vision ==
/// false` -- a text-only local model, or a compat endpoint configured
/// without vision. `ocr_text` is the recognized on-screen text (see
/// `crate::ocr::recognize` / `crate::ocr::serialize_lines`); `uia_fields`
/// is the compact field-list serialization of the foreground window's
/// editable controls (see `crate::inputs::uia::format_compact`), or an
/// empty string when no UIA snapshot could be taken -- a missing UIA
/// snapshot degrades gracefully (the OCR text alone is still useful) rather
/// than failing the whole fallback the way an OCR failure does (see
/// [`Chain::complete_parsed_with_fallback`]'s doc comment).
#[derive(Debug, Clone, Default)]
pub struct NonVisionInputs {
    pub ocr_text: String,
    pub uia_fields: String,
}

/// Size bound (characters, not bytes -- see [`truncate_chars`]) on the OCR
/// text folded into a [`non_vision_request`]. A large document or a very
/// text-dense screen could otherwise blow well past a small local model's
/// context window; 12k characters is generous for the single on-screen
/// problem this action is about, while still bounding worst-case token
/// cost and latency for a local model this fallback path exists to serve.
pub const MAX_NON_VISION_CHARS: usize = 12_000;

/// Prefaced onto `system` by [`non_vision_request`], explaining the input
/// shape to a model that is not shown the screenshot. Describes the INPUT
/// format only -- it never asks the model to explain itself or its own
/// reasoning, which is CLAUDE.md rule 10's reasoning-extraction trap
/// (Anthropic's classifier refuses a prompt that reads that way,
/// `stop_reason: "refusal"`, MEASURED 2026-09-15); that trap is about
/// wording that asks FOR reasoning, not about describing what data the
/// model was given, so this prefix does not trigger it.
const NON_VISION_PREFACE: &str = "This model has no image input. Instead of a screenshot, you are given the on-screen text recognized by OCR, plus a compact list of the foreground window's editable fields (label and current value, where known). Layout and visual structure are lost; treat the text and fields below as everything visible on screen.";

/// Truncates `s` to at most `max_chars` **characters** (never splitting a
/// multi-byte UTF-8 codepoint, unlike a raw byte-length truncation), and
/// reports whether truncation actually happened. `s.chars().count() <=
/// max_chars` is the untruncated case -- checked up front so the common
/// case (well under the bound) allocates nothing beyond the one owned
/// `String` every caller needs anyway.
pub fn truncate_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_string(), false);
    }
    (s.chars().take(max_chars).collect(), true)
}

/// Issue #18/#206: builds the non-vision variant of `base` -- the image(s)
/// are dropped entirely and `system`/`user` are rewritten to carry OCR text
/// and the compact UIA field list instead. `schema`, `effort` and
/// `max_tokens` carry over unchanged (mirrors [`repair_request`]'s same
/// choice): the answer shape a provider must produce does not change just
/// because its input did.
///
/// `ocr_text` is bounded to [`MAX_NON_VISION_CHARS`] characters (see
/// [`truncate_chars`]); when that truncates, the system prompt says so
/// explicitly, so the model does not mistake a cut-off screen for the whole
/// picture. `uia_fields` is not size-bounded here -- `crate::inputs::uia`'s
/// own `DEFAULT_MAX_ELEMENTS` walk cap already bounds it upstream.
pub fn non_vision_request(base: &Request, ocr_text: &str, uia_fields: &str) -> Request {
    let (ocr_text, truncated) = truncate_chars(ocr_text, MAX_NON_VISION_CHARS);

    let mut system = format!("{NON_VISION_PREFACE}\n\n{}", base.system);
    if truncated {
        system.push_str(
            "\n\nThe OCR text below was truncated to fit a length limit; it may be incomplete.",
        );
    }

    let mut user = base.user.clone();
    user.push_str("\n\n-- OCR text of the screen --\n");
    user.push_str(if ocr_text.is_empty() {
        "(no text recognized)"
    } else {
        &ocr_text
    });
    user.push_str("\n\n-- Foreground window fields --\n");
    user.push_str(if uia_fields.is_empty() {
        "(no fields found)"
    } else {
        uia_fields
    });

    Request {
        system,
        user,
        images: Vec::new(),
        schema: base.schema.clone(),
        effort: base.effort,
        max_tokens: base.max_tokens,
    }
}

pub trait Provider: Send + Sync {
    /// Short, stable identifier ("openai", "anthropic", ...): used in
    /// config (`providers.order`), the tray tooltip and diagnostics. Not
    /// user-facing copy.
    fn id(&self) -> &'static str;

    /// Whether this provider is usable, e.g. has a non-empty API key.
    ///
    /// Defaults to `true`. A provider backed by a key the user has not
    /// configured overrides this to return `false` so that `Chain` can skip
    /// it silently instead of trying it and recording a failure.
    fn ready(&self) -> bool {
        true
    }

    /// What this provider can do for `model`. Vision, structured JSON
    /// output and extended thinking/effort support all vary per model, not
    /// just per provider. `image_limits` (issue #169) is read in production
    /// via [`own_caps`](Provider::own_caps); the rest is still only read by
    /// tests until Phase 2's router and action picker exist to consult it.
    fn capabilities(&self, model: &str) -> Caps;

    /// This provider's own `Caps` -- i.e. `capabilities()` for whatever
    /// model it is actually configured with. Lets a caller holding only a
    /// `Chain` (not the concrete provider objects; see
    /// [`Chain::first_ready_caps`]) learn what the first ready provider will
    /// actually do, without needing to know its model string itself (issue
    /// #169: the capture step needs the image limits of the model the chain
    /// will use). Defaults to `Caps::default()` -- every real provider
    /// (`Anthropic`, `OpenAi`, `Gemini`, `Ollama`, `OpenAiCompat`) overrides
    /// this with `self.capabilities(&self.model)`; only test doubles that
    /// have no single configured model to report on rely on the default.
    fn own_caps(&self) -> Caps {
        Caps::default()
    }

    /// Runs one single-turn request and returns the whole result. No
    /// streaming (owner decision, 2026-09-16, "No chat": every response is
    /// a whole structured result or a short card, so the full response is
    /// needed before anything can be shown, and streaming would only add a
    /// thread-crossing path and a class of partial-state bugs).
    fn complete(&self, req: &Request) -> anyhow::Result<Completion>;
}

/// Runs a list of providers in order, falling through to the next on any
/// failure (transport error, non-2xx, or unparseable body). A provider that
/// is not `ready()` (e.g. its API key is empty) is skipped entirely, rather
/// than being tried and failing. If every provider fails or is skipped, the
/// *first* encountered error is surfaced.
pub struct Chain {
    providers: Vec<Box<dyn Provider>>,
}

impl Chain {
    pub fn new(providers: Vec<Box<dyn Provider>>) -> Self {
        Self { providers }
    }

    /// Tries each ready provider in order, returning the first success. See
    /// the type-level doc for the exact fallback semantics.
    ///
    /// Issue #176: production code now always goes through
    /// [`complete_parsed`](Self::complete_parsed) instead (worker's caller
    /// needs `parse_answer` to run inside the fallback loop, not after it),
    /// so this plain form is unused outside tests -- same
    /// unused-until-a-caller-needs-it status as `provider_names` below.
    /// Kept as the simple entry point for a future caller that has no
    /// content to validate.
    #[allow(dead_code)]
    pub fn complete(&self, req: &Request) -> anyhow::Result<Completion> {
        self.complete_parsed(req, |c| Ok(c.clone()))
    }

    /// Like [`complete`](Self::complete), but a provider's success is not
    /// just "returned HTTP 200" -- it also has to satisfy `parse`. A 200
    /// response whose text fails `parse` (a schema-invalid answer, say) is
    /// treated exactly like a transport error or a non-2xx: this provider is
    /// skipped and the chain falls through to the next ready one, and the
    /// *first* such failure (transport or parse) is what a full-chain
    /// failure surfaces. `Chain` stays action-agnostic -- it never knows
    /// what `parse` checks -- so the physics-check schema lives entirely in
    /// [`parse_answer`], not here (issue #176: before the #12 trait refactor
    /// this fallback-on-invalid-schema behaviour lived inside each
    /// provider's own `ask`; `complete_parsed` restores it without teaching
    /// `Chain` about any one action's schema again).
    ///
    /// Issue #99: before falling through, a `parse` failure gets exactly
    /// ONE repair attempt against the SAME provider -- see
    /// [`repair_request`] for what that second request carries (the bad
    /// text and the parse error, never the screenshot again, to save
    /// tokens). No repair is attempted when `completion.stop ==
    /// StopReason::MaxTokens`: a provider that already reported "ran out of
    /// budget" asked to "try again" with the same budget would just fail
    /// the same way, and a genuine refusal never reaches this point at all
    /// -- every provider's own `complete` already turns a refusal into
    /// `Err` before a `Completion` exists (see e.g. `anthropic.rs`'s
    /// `stop_reason: "refusal"` handling), so it takes the plain
    /// transport-error branch above, which this function has never retried.
    /// If the repair attempt also fails to parse (or errors outright), the
    /// *original* parse error is what counts as this provider's failure for
    /// `first_err` purposes, and the chain falls through exactly as before.
    pub fn complete_parsed<T>(
        &self,
        req: &Request,
        mut parse: impl FnMut(&Completion) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut first_err: Option<anyhow::Error> = None;

        for provider in &self.providers {
            if !provider.ready() {
                continue;
            }
            match attempt_provider(provider.as_ref(), req, &mut parse) {
                Ok(value) => return Ok(value),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no providers configured")))
    }

    /// Issue #18/#206: like [`complete_parsed`](Self::complete_parsed), but
    /// a provider whose `own_caps().vision` is `false` never sees `req`'s
    /// image(s) at all -- it is sent [`non_vision_request`]'s rewrite (OCR
    /// text plus the compact UIA field list) instead. A provider later in
    /// the chain that DOES have vision still gets `req` untouched, image
    /// included; the substitution is decided fresh for each provider, not
    /// once for the whole chain.
    ///
    /// `fallback` computes the [`NonVisionInputs`] (real callers run OCR and
    /// a UIA snapshot inside it -- see `app.rs`'s `non_vision_inputs`). It
    /// is called **lazily and at most once**: never at all if every ready
    /// provider has vision (or `req` carries no image to begin with), and
    /// only on the first ready provider that actually needs it -- every
    /// later non-vision provider in the same call reuses that one result.
    /// This is the "never OCR for a vision-only chain" / "token and time
    /// efficiency" requirement from issue #18: OCR and a UIA snapshot both
    /// cost real wall-clock time, so neither runs unless something is
    /// actually going to use it.
    ///
    /// If `fallback` itself errors (OCR engine unavailable, no language
    /// pack installed, timeout -- see `ocr::recognize`'s error paths), every
    /// provider that needed it is skipped, exactly like a transport error:
    /// the chain still falls through to the next ready provider, and the
    /// *first* such failure is what a full-chain failure surfaces, worded to
    /// name both the provider and the reason (rule 7: a full-chain failure
    /// must still end in a card naming why).
    pub fn complete_parsed_with_fallback<T>(
        &self,
        req: &Request,
        mut fallback: impl FnMut() -> anyhow::Result<NonVisionInputs>,
        mut parse: impl FnMut(&Completion) -> anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        let mut first_err: Option<anyhow::Error> = None;
        // Cached as a `String` error (not `anyhow::Error`) purely so this
        // can be cloned/reused across every provider that needs it without
        // `anyhow::Error: Clone` (it is not) forcing `fallback` to be
        // called again per provider -- which would defeat the "at most
        // once" guarantee this method promises.
        let mut cached_fallback: Option<Result<NonVisionInputs, String>> = None;

        for provider in &self.providers {
            if !provider.ready() {
                continue;
            }

            let needs_fallback = !req.images.is_empty() && !provider.own_caps().vision;
            let effective: std::borrow::Cow<'_, Request> = if needs_fallback {
                let cached =
                    cached_fallback.get_or_insert_with(|| fallback().map_err(|e| format!("{e:#}")));
                match cached {
                    Ok(inputs) => std::borrow::Cow::Owned(non_vision_request(
                        req,
                        &inputs.ocr_text,
                        &inputs.uia_fields,
                    )),
                    Err(msg) => {
                        if first_err.is_none() {
                            first_err = Some(anyhow::anyhow!(
                                "{}: no vision, and the OCR/UIA fallback failed: {msg}",
                                provider.id()
                            ));
                        }
                        continue;
                    }
                }
            } else {
                std::borrow::Cow::Borrowed(req)
            };

            match attempt_provider(provider.as_ref(), effective.as_ref(), &mut parse) {
                Ok(value) => return Ok(value),
                Err(e) => {
                    if first_err.is_none() {
                        first_err = Some(e);
                    }
                }
            }
        }

        Err(first_err.unwrap_or_else(|| anyhow::anyhow!("no providers configured")))
    }

    /// Ids of every provider in the chain, in order, regardless of
    /// readiness. Mainly useful for introspection/testing and for surfacing
    /// the active provider in UI (e.g. a tray tooltip).
    /// `ready_provider_names` is what the tooltip uses; this one exists for
    /// diagnostics.
    #[allow(dead_code)]
    pub fn provider_names(&self) -> Vec<&'static str> {
        self.providers.iter().map(|p| p.id()).collect()
    }

    /// Ids of only the providers that are currently `ready()` (e.g. have a
    /// non-empty API key), in order.
    pub fn ready_provider_names(&self) -> Vec<&'static str> {
        self.providers
            .iter()
            .filter(|p| p.ready())
            .map(|p| p.id())
            .collect()
    }

    /// The `Caps` -- notably [`Caps::image_limits`] -- of the first `ready()`
    /// provider in this chain, i.e. the one [`complete_parsed`](Self::complete_parsed)
    /// will actually try first. `None` when no provider in the chain is
    /// ready. Issue #169: lets the capture step downscale for whichever
    /// provider is really going to be asked, instead of a provider-agnostic
    /// heuristic.
    pub fn first_ready_caps(&self) -> Option<Caps> {
        self.providers
            .iter()
            .find(|p| p.ready())
            .map(|p| p.own_caps())
    }
}

/// Builds the `Request` for the one action Wingman has today: checking a
/// physics/statistics screenshot. The structured-answer schema (`detail`,
/// `headline`, optionally `difficulty`) is built here, not inside a
/// provider, and handed over as `Request::schema` -- see the 2026-09-16
/// expansion plan's "Provider trait, extended". This is what keeps the door
/// open for Phase 2 actions, each with its own proposal schema, to reuse
/// `Anthropic`/`OpenAi`/future providers without another trait change.
pub fn physics_request(shot: &Shot, prompt: &str, want_difficulty: bool) -> Request {
    Request {
        system: common::augmented_system_prompt(prompt, want_difficulty),
        user: "Check my working.".to_string(),
        images: vec![shot.png.clone()],
        // #23: routed through the proposal schema registry instead of
        // calling `common::answer_schema` directly -- the registry's
        // `"verdict"` arm is that same call, so this is a no-op swap,
        // proven byte-identical by the golden tests above (written before
        // this line changed).
        schema: Some(
            crate::actions::schema::schema_for("verdict", want_difficulty)
                .expect("\"verdict\" is always registered in actions::schema"),
        ),
        effort: Effort::Unset,
        max_tokens: 0,
    }
}

/// Builds the `Request` for #39's "Add event from screen" action: the
/// `calendar_event` proposal schema (registered in `actions::schema`,
/// #26), a system prompt that already carries today's local date and UTC
/// offset (`actions::calendar::build_prompt` -- `prompt` here is expected
/// to be its output, not the bare action prompt), and no difficulty
/// rubric (`common::augmented_system_prompt` is `physics_request`'s own
/// concern; a calendar proposal has no difficulty field to rate). Mirrors
/// `physics_request`'s shape exactly otherwise, so it reuses the same
/// provider chain, retry, repair and non-vision fallback machinery with no
/// changes to any of that code.
pub fn calendar_request(shot: &Shot, prompt: &str) -> Request {
    Request {
        system: prompt.to_string(),
        user: "Find the event on screen.".to_string(),
        images: vec![shot.png.clone()],
        schema: Some(
            crate::actions::schema::schema_for("calendar_event", false)
                .expect("\"calendar_event\" is always registered in actions::schema"),
        ),
        effort: Effort::Unset,
        max_tokens: 0,
    }
}

/// Builds the `text_review` `Request` (#38) for a `ComposeBody`- or
/// `Selection`-sourced review: `captured_text` becomes the whole user turn,
/// with no image at all. Every provider (vision-capable or not) handles a
/// text-only request uniformly, so `app.rs`'s `review_worker` still routes
/// this through `Chain::complete_parsed_with_fallback` (never the plain
/// `complete_parsed`) purely for call-site symmetry with `review_request_from_screen`
/// below -- the fallback closure is simply never invoked, since
/// `complete_parsed_with_fallback` only calls it when `req.images` is
/// non-empty.
pub fn review_request_from_text(prompt: &str, captured_text: &str) -> Request {
    Request {
        system: prompt.to_string(),
        user: captured_text.to_string(),
        images: vec![],
        schema: Some(
            crate::actions::schema::schema_for("text_review", false)
                .expect("\"text_review\" is always registered in actions::schema"),
        ),
        effort: Effort::Unset,
        max_tokens: 0,
    }
}

/// Builds the `text_review` `Request` (#38) for the `Screen` fallback
/// source: same shape as [`calendar_request`], a screenshot with no
/// separately-captured text.
pub fn review_request_from_screen(prompt: &str, shot: &Shot) -> Request {
    Request {
        system: prompt.to_string(),
        user: "Find the email on screen and review it.".to_string(),
        images: vec![shot.png.clone()],
        schema: Some(
            crate::actions::schema::schema_for("text_review", false)
                .expect("\"text_review\" is always registered in actions::schema"),
        ),
        effort: Effort::Unset,
        max_tokens: 0,
    }
}

/// One provider's whole attempt at `req`: the call, the #200 local cleanup,
/// and the #99 one-shot repair pass, exactly as [`Chain::complete_parsed`]
/// always ran them inline. Factored out so
/// [`Chain::complete_parsed_with_fallback`] can run the identical sequence
/// against a per-provider `req` (the original, or [`non_vision_request`]'s
/// rewrite) without duplicating this logic -- see each `Chain` method's own
/// doc comment for what a `Err` return means to its caller (always "this
/// provider failed", never "the whole chain failed", which only the caller
/// decides once every provider has been tried).
fn attempt_provider<T>(
    provider: &dyn Provider,
    req: &Request,
    parse: &mut impl FnMut(&Completion) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let completion = provider.complete(req)?;

    match parse(&completion) {
        Ok(value) => Ok(value),
        Err(parse_err) => {
            // Issue #200: before spending a #99 network repair round trip,
            // try a local, zero-network cleanup of the near-JSON text -- a
            // fenced or prose-wrapped response is cheap and deterministic
            // to fix with string surgery, so this is tried first and the
            // network repair pass below is reserved for genuinely wrong
            // JSON (missing/malformed fields), which is what it is
            // actually good at.
            let cleaned = strip_near_json(&completion.text);
            if cleaned != completion.text {
                let cleaned_completion = Completion {
                    text: cleaned,
                    usage: completion.usage,
                    stop: completion.stop,
                };
                if let Ok(value) = parse(&cleaned_completion) {
                    return Ok(value);
                }
            }

            if completion.stop != StopReason::MaxTokens {
                let repair_req = repair_request(req, &completion.text, &parse_err.to_string());
                if let Ok(repaired) = provider.complete(&repair_req) {
                    if let Ok(value) = parse(&repaired) {
                        return Ok(value);
                    }
                }
            }
            Err(parse_err)
        }
    }
}

/// Issue #99's repair-request system prompt. Worded to ask only for
/// corrected JSON, never for the model's reasoning or "how" it got the
/// answer -- CLAUDE.md rule 10's `reasoning_extraction` trap (Anthropic's
/// classifier refuses a prompt that reads as extracting internal reasoning,
/// `stop_reason: "refusal"`, MEASURED 2026-09-15) applies just as much to a
/// repair prompt as to the original one that rule 3's `detail`-before-
/// `headline` ordering already works around.
const REPAIR_SYSTEM: &str =
    "The previous response to this request did not match the required JSON output. Return corrected JSON only, matching the same schema as before. Do not include any explanation, markdown formatting, or code fences.";

/// Issue #99: builds the ONE-shot repair request sent back to the SAME
/// provider that produced `bad_text`, after it fails `parse`. Carries the
/// bad output and the parse error, but deliberately drops `original.images`
/// -- the model already saw the screenshot on the first attempt, and a
/// repair is about fixing the JSON shape, not re-reading the screen, so
/// resending it would only cost tokens for no benefit (issue #99's "no
/// screenshot re-sent, to save tokens"). `schema`, `effort` and
/// `max_tokens` carry over unchanged, so the repair is held to the same
/// contract as the original request.
fn repair_request(original: &Request, bad_text: &str, parse_error: &str) -> Request {
    Request {
        system: REPAIR_SYSTEM.to_string(),
        user: format!(
            "The previous response was:\n\n{bad_text}\n\nIt did not parse: {parse_error}\n\nReturn corrected JSON only."
        ),
        images: Vec::new(),
        schema: original.schema.clone(),
        effort: original.effort,
        max_tokens: original.max_tokens,
    }
}

/// Issue #200: a cheap, deterministic, zero-network cleanup of near-JSON
/// model output, tried in [`Chain::complete_parsed`] before the first
/// `parse` failure escalates to a #99 network repair round trip. Strips a
/// single surrounding markdown code fence (` ```json ... ``` ` or
/// ` ``` ... ``` `), then trims any prose before the first `{` and after
/// that object's matching closing `}` (found with [`extract_balanced_object`],
/// a scan that respects strings and escapes so a `{`/`}` inside a JSON
/// string value never miscounts depth). Returns the input unchanged (as an
/// owned `String`, for a uniform return type) when neither cleanup applies,
/// or when the text has no balanced top-level object at all -- an unbalanced
/// input is left exactly as-is so the caller's next `parse` attempt still
/// fails and falls through to the network repair pass unchanged.
fn strip_near_json(text: &str) -> String {
    let fenced = strip_code_fence(text.trim());
    match extract_balanced_object(fenced) {
        Some(obj) => obj.to_string(),
        None => fenced.to_string(),
    }
}

/// Strips one surrounding ` ``` ` fence, with or without a language tag
/// (` ```json `) on the opening line. Returns `text` unchanged if it is not
/// fenced (no leading/trailing ` ``` `) -- `text` is expected already
/// trimmed by the caller.
fn strip_code_fence(text: &str) -> &str {
    let Some(after_open) = text.strip_prefix("```") else {
        return text;
    };
    let Some(body) = after_open.strip_suffix("```") else {
        return text;
    };
    match body.split_once('\n') {
        // The first line is a bare alphabetic tag ("json"): drop it and the
        // newline that ends it. Guarded on "letters only" so a NO-tag fence
        // whose content happens to start on the very first line is never
        // mistaken for one (its first line would contain `{`, `"`, digits,
        // etc., none of which are ASCII-alphabetic-only).
        Some((first_line, rest))
            if !first_line.is_empty() && first_line.chars().all(|c| c.is_ascii_alphabetic()) =>
        {
            rest.trim()
        }
        _ => body.trim(),
    }
}

/// Finds the first `{` in `text` and scans forward tracking brace depth to
/// find ITS matching `}` (where depth returns to zero), skipping over the
/// contents of any JSON string (so a brace inside a string value, or an
/// escaped `"`, never affects depth). Returns the slice from the first `{`
/// through that matching `}` inclusive -- i.e. exactly the first balanced
/// top-level object, discarding anything before or after it (including a
/// second top-level object, if the text happens to contain two). Returns
/// `None` when the text has no `{` at all, or the braces never balance.
fn extract_balanced_object(text: &str) -> Option<&str> {
    let start = text.find('{')?;
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escaped = false;
    for (rel_i, c) in text[start..].char_indices() {
        if in_string {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    let end = start + rel_i + c.len_utf8();
                    return Some(&text[start..end]);
                }
            }
            _ => {}
        }
    }
    None
}

/// The wire shape of the model's JSON payload for the physics-check answer.
/// Kept separate from the public `Answer` because `difficulty` arrives as a
/// bare string ("7", "U", ...) that is not a `Difficulty`'s natural
/// `Deserialize` form -- it is parsed explicitly below, and a bad/missing
/// value must degrade to `None` rather than fail the whole parse.
#[derive(Deserialize)]
struct RawAnswer {
    detail: String,
    headline: String,
    #[serde(default)]
    difficulty: Option<String>,
}

/// Parses a `Completion::text` produced from a [`physics_request`] (i.e.
/// matching `common::answer_schema`) into an `Answer`. This is the one place
/// the physics-check schema is interpreted -- providers only ever hand back
/// raw text.
pub fn parse_answer(text: &str) -> Result<Answer> {
    let raw: RawAnswer =
        serde_json::from_str(text).context("provider: completion text is not a valid Answer")?;
    Ok(Answer {
        detail: raw.detail,
        headline: raw.headline,
        // A missing or unparseable difficulty must yield `None`, never an
        // error -- the answer itself is what matters.
        difficulty: raw.difficulty.as_deref().and_then(Difficulty::parse),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct MockProvider {
        id: &'static str,
        ready: bool,
        calls: AtomicU32,
        result: fn() -> anyhow::Result<Completion>,
    }

    impl Provider for MockProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn ready(&self) -> bool {
            self.ready
        }

        fn capabilities(&self, _model: &str) -> Caps {
            Caps::default()
        }

        fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            (self.result)()
        }
    }

    /// #99: a provider whose `complete` result varies call by call, so a
    /// "bad then good" (repair succeeds) or "bad then bad" (repair also
    /// fails) sequence can be scripted. `MockProvider` above always returns
    /// the same thing every call, which can't express either shape.
    type ScriptedResult = fn() -> anyhow::Result<Completion>;

    struct SequencedProvider {
        id: &'static str,
        results: std::sync::Mutex<std::collections::VecDeque<ScriptedResult>>,
        /// `Arc` (not a bare `AtomicU32`) so a test can clone the counter
        /// *before* moving this provider into `Chain::new` (which takes
        /// ownership via `Box<dyn Provider>`) and still read it back
        /// afterwards -- see `SequencedProvider::new_counted`.
        calls: std::sync::Arc<AtomicU32>,
    }

    impl SequencedProvider {
        fn new(id: &'static str, results: Vec<ScriptedResult>) -> Self {
            Self::new_counted(id, results).0
        }

        /// Like `new`, but also hands back a clone of the call counter so
        /// the caller can inspect it after the provider has been moved into
        /// a `Chain`.
        fn new_counted(
            id: &'static str,
            results: Vec<ScriptedResult>,
        ) -> (Self, std::sync::Arc<AtomicU32>) {
            let calls = std::sync::Arc::new(AtomicU32::new(0));
            (
                Self {
                    id,
                    results: std::sync::Mutex::new(results.into()),
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl Provider for SequencedProvider {
        fn id(&self) -> &'static str {
            self.id
        }

        fn capabilities(&self, _model: &str) -> Caps {
            Caps::default()
        }

        fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let mut queue = self.results.lock().unwrap();
            let next = queue
                .pop_front()
                .expect("SequencedProvider called more times than scripted");
            next()
        }
    }

    fn repaired_completion() -> anyhow::Result<Completion> {
        Ok(Completion {
            text: r#"{"detail":"fixed on repair","headline":"repaired"}"#.to_string(),
            usage: None,
            stop: StopReason::Complete,
        })
    }

    /// A 200 whose text is schema-invalid AND whose `stop` already says the
    /// model ran out of budget -- the case #99 says must never get a repair
    /// attempt (retrying with the same budget would just fail the same way).
    fn max_tokens_completion() -> anyhow::Result<Completion> {
        Ok(Completion {
            text: r#"{"not":"an answer"}"#.to_string(),
            usage: None,
            stop: StopReason::MaxTokens,
        })
    }

    fn ok_completion() -> anyhow::Result<Completion> {
        Ok(Completion {
            text: r#"{"detail":"because reasons","headline":"42"}"#.to_string(),
            usage: None,
            stop: StopReason::Complete,
        })
    }

    /// A 200 response whose body doesn't match the physics-check schema --
    /// the case #176 is about: a schema-invalid 200 must be treated as this
    /// provider failing, not as the whole request failing.
    fn unparseable_completion() -> anyhow::Result<Completion> {
        Ok(Completion {
            text: r#"{"not":"an answer"}"#.to_string(),
            usage: None,
            stop: StopReason::Complete,
        })
    }

    fn shot() -> Shot {
        Shot {
            png: vec![],
            width: 1,
            height: 1,
        }
    }

    fn req() -> Request {
        physics_request(&shot(), "prompt", false)
    }

    #[test]
    fn tries_providers_in_order_and_returns_first_success() {
        let calls_a = AtomicU32::new(0);
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: calls_a,
            result: || Err(anyhow::anyhow!("a failed")),
        };
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let completion = chain.complete(&req()).unwrap();
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn skips_provider_with_empty_key_without_failing() {
        struct PanicsIfCalled;
        impl Provider for PanicsIfCalled {
            fn id(&self) -> &'static str {
                "skip-me"
            }
            fn ready(&self) -> bool {
                false
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps::default()
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                panic!("unready provider must not be asked");
            }
        }

        let good = MockProvider {
            id: "good",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(PanicsIfCalled), Box::new(good)]);
        let completion = chain.complete(&req()).unwrap();
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn all_unready_surfaces_no_providers_configured() {
        struct NeverReady;
        impl Provider for NeverReady {
            fn id(&self) -> &'static str {
                "never"
            }
            fn ready(&self) -> bool {
                false
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps::default()
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                unreachable!("should never be called when not ready")
            }
        }

        let chain = Chain::new(vec![Box::new(NeverReady), Box::new(NeverReady)]);
        let err = chain.complete(&req()).unwrap_err();
        assert_eq!(err.to_string(), "no providers configured");
    }

    #[test]
    fn surfaces_first_error_when_all_fail() {
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("first error")),
        };
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("second error")),
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let err = chain.complete(&req()).unwrap_err();
        assert_eq!(err.to_string(), "first error");
    }

    #[test]
    fn skip_does_not_become_first_error() {
        struct NeverReady;
        impl Provider for NeverReady {
            fn id(&self) -> &'static str {
                "never"
            }
            fn ready(&self) -> bool {
                false
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps::default()
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                unreachable!("should never be called when not ready")
            }
        }
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("real error")),
        };
        let chain = Chain::new(vec![Box::new(NeverReady), Box::new(a)]);
        let err = chain.complete(&req()).unwrap_err();
        assert_eq!(err.to_string(), "real error");
    }

    #[test]
    fn difficulty_parses_every_valid_level() {
        for n in 1..=10u8 {
            assert_eq!(
                Difficulty::parse(&n.to_string()),
                Some(Difficulty::Level(n)),
                "failed for {n}"
            );
        }
    }

    #[test]
    fn difficulty_parses_ultra_case_insensitively() {
        for s in ["u", "U", "ultra", "Ultra", "ULTRA"] {
            assert_eq!(
                Difficulty::parse(s),
                Some(Difficulty::Ultra),
                "failed for {s:?}"
            );
        }
    }

    #[test]
    fn difficulty_tolerates_surrounding_whitespace() {
        assert_eq!(Difficulty::parse("  7  "), Some(Difficulty::Level(7)));
        assert_eq!(Difficulty::parse("  U  "), Some(Difficulty::Ultra));
    }

    #[test]
    fn difficulty_rejects_out_of_range_and_garbage() {
        for s in [
            "0",
            "11",
            "-1",
            "",
            "seven",
            "3.5",
            "999999999999999999999999999999", // overflows a u32, let alone a u8
        ] {
            assert_eq!(Difficulty::parse(s), None, "expected None for {s:?}");
        }
    }

    #[test]
    fn difficulty_label_and_rank() {
        assert_eq!(Difficulty::Level(1).label(), "1");
        assert_eq!(Difficulty::Level(9).label(), "9");
        assert_eq!(Difficulty::Level(10).label(), "10");
        assert_eq!(Difficulty::Ultra.label(), "U");

        for n in 1..=10u8 {
            assert_eq!(Difficulty::Level(n).rank(), n);
        }
        assert_eq!(Difficulty::Ultra.rank(), 11);
    }
    #[test]
    fn not_applicable_yields_no_badge() {
        // The model answers "N" when there is nothing on screen to rate.
        // It must render as no badge, never as a difficulty.
        assert_eq!(Difficulty::parse("N"), None);
        assert_eq!(Difficulty::parse("n"), None);
        // And it must not be confused with Ultra.
        assert_eq!(Difficulty::parse("U"), Some(Difficulty::Ultra));
    }

    // -- Effort --------------------------------------------------------

    #[test]
    fn effort_parses_known_values_case_insensitively() {
        for (s, expected) in [
            ("low", Effort::Low),
            ("LOW", Effort::Low),
            (" Low ", Effort::Low),
            ("medium", Effort::Medium),
            ("high", Effort::High),
        ] {
            assert_eq!(Effort::parse(s), expected, "failed for {s:?}");
        }
    }

    #[test]
    fn effort_parse_degrades_unknown_and_empty_to_unset() {
        for s in ["", "   ", "extreme", "none"] {
            assert_eq!(Effort::parse(s), Effort::Unset, "failed for {s:?}");
        }
    }

    #[test]
    fn effort_as_str_omits_unset() {
        assert_eq!(Effort::Unset.as_str(), None);
        assert_eq!(Effort::Low.as_str(), Some("low"));
        assert_eq!(Effort::Medium.as_str(), Some("medium"));
        assert_eq!(Effort::High.as_str(), Some("high"));
    }

    // -- Chain::complete_parsed (issue #176) -----------------------------

    #[test]
    fn complete_parsed_falls_through_when_first_providers_answer_fails_validation() {
        // Provider 1 returns a 200 that doesn't parse as an Answer; the
        // chain must move on to provider 2 rather than surfacing the parse
        // failure as the whole request's outcome.
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: unparseable_completion,
        };
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn complete_parsed_surfaces_first_parse_error_when_all_fail_validation() {
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: unparseable_completion,
        };
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: unparseable_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let err = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap_err();
        assert!(
            err.to_string().contains("not a valid Answer"),
            "expected the first provider's parse error, got: {err}"
        );
    }

    #[test]
    fn complete_parsed_still_skips_unready_providers() {
        struct NeverReady;
        impl Provider for NeverReady {
            fn id(&self) -> &'static str {
                "never"
            }
            fn ready(&self) -> bool {
                false
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps::default()
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                unreachable!("should never be called when not ready")
            }
        }
        let good = MockProvider {
            id: "good",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(NeverReady), Box::new(good)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn complete_parsed_falls_through_on_transport_error_too() {
        // A plain transport/HTTP failure must still fall through exactly as
        // `complete` does -- complete_parsed generalizes complete, it
        // doesn't change its existing behaviour.
        let a = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: || Err(anyhow::anyhow!("a failed")),
        };
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    // -- Chain::complete_parsed repair pass (issue #99) --------------------

    #[test]
    fn repair_succeeds_bad_then_good_returns_the_repaired_answer_without_a_second_provider() {
        let a = SequencedProvider::new("a", vec![unparseable_completion, repaired_completion]);
        let chain = Chain::new(vec![Box::new(a)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "repaired");
    }

    #[test]
    fn repair_bad_then_bad_falls_through_to_the_next_provider() {
        let a = SequencedProvider::new("a", vec![unparseable_completion, unparseable_completion]);
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(
            answer.headline, "42",
            "must have come from provider b, not a repaired a"
        );
    }

    #[test]
    fn repair_is_attempted_exactly_once_a_third_call_would_panic() {
        // Only two results are scripted; if the repair pass ever attempted
        // a second repair (or called the provider a third time for any
        // reason), `SequencedProvider::complete` panics on the empty queue.
        // `a` alone (no fallback provider) proves the chain doesn't recover
        // by some other path if that happened.
        let a = SequencedProvider::new("a", vec![unparseable_completion, unparseable_completion]);
        let chain = Chain::new(vec![Box::new(a)]);
        let err = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap_err();
        assert!(err.to_string().contains("not a valid Answer"));
    }

    #[test]
    fn repair_records_exactly_two_calls_for_bad_then_good() {
        let (a, calls) =
            SequencedProvider::new_counted("a", vec![unparseable_completion, repaired_completion]);
        let chain = Chain::new(vec![Box::new(a)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "repaired");
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "exactly one original call plus one repair call"
        );
    }

    #[test]
    fn no_repair_when_the_provider_stop_reason_is_max_tokens() {
        // Only one result scripted: if a repair were attempted, the second
        // `complete()` call would panic on the empty queue instead of
        // falling through cleanly to provider b.
        let a = SequencedProvider::new("a", vec![max_tokens_completion]);
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn no_repair_on_a_refusal_the_provider_error_path_is_never_retried() {
        // A refusal never reaches `parse` at all -- every provider turns it
        // into `Err` from `complete()` itself (mirrors `anthropic.rs`'s
        // `stop_reason: "refusal"` handling). Only one result scripted:
        // `SequencedProvider` panics on a second call, so a passing test
        // here proves no repair (retry) was attempted on the error path.
        let a = SequencedProvider::new(
            "a",
            vec![|| Err(anyhow::anyhow!("anthropic: model refused to answer"))],
        );
        let b = MockProvider {
            id: "b",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn repair_request_drops_images_but_keeps_schema_effort_and_max_tokens() {
        let mut original = req();
        original.images = vec![vec![1, 2, 3]];
        original.effort = Effort::High;
        original.max_tokens = 999;

        let repair = repair_request(&original, "bad text", "parse error text");

        assert!(repair.images.is_empty(), "no screenshot re-sent (#99)");
        assert_eq!(repair.schema, original.schema);
        assert_eq!(repair.effort, Effort::High);
        assert_eq!(repair.max_tokens, 999);
        assert!(repair.user.contains("bad text"));
        assert!(repair.user.contains("parse error text"));
    }

    /// CLAUDE.md rule 10's trap: a repair prompt that reads as asking for
    /// the model's internal reasoning gets refused (MEASURED 2026-09-15).
    /// The repair wording must never ask "why" or for a scratchpad -- only
    /// for corrected JSON.
    #[test]
    fn repair_request_wording_never_asks_for_reasoning() {
        let repair = repair_request(&req(), "bad", "err");
        for banned in [
            "scratchpad",
            "reason it out",
            "explain your reasoning",
            "think step by step",
        ] {
            assert!(
                !repair.system.to_lowercase().contains(banned),
                "repair system prompt must not read as reasoning extraction: {:?}",
                repair.system
            );
        }
    }

    // -- physics_request / parse_answer ---------------------------------

    // -- #23: golden test, written BEFORE physics_request was switched to
    // build its schema through actions::schema::schema_for, to prove the
    // switch is a byte-identical no-op. Locks in the exact wire schema (not
    // just "a schema" -- the literal JSON, property order included, since
    // that order is load-bearing per rule 3) for both the with- and
    // without-difficulty cases. If this test ever needs to change, the
    // wire request changed, which is the one thing this refactor must not
    // do.
    #[test]
    fn golden_physics_request_schema_without_difficulty_is_byte_identical() {
        let req = physics_request(&shot(), "irrelevant prompt", false);
        let schema = req.schema.expect("schema always present");
        assert_eq!(
            serde_json::to_string(&schema).unwrap(),
            r#"{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false}"#
        );
    }

    #[test]
    fn golden_physics_request_schema_with_difficulty_is_byte_identical() {
        let req = physics_request(&shot(), "irrelevant prompt", true);
        let schema = req.schema.expect("schema always present");
        assert_eq!(
            serde_json::to_string(&schema).unwrap(),
            r#"{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"},"difficulty":{"type":"string","enum":["1","2","3","4","5","6","7","8","9","10","U","N"]}},"required":["detail","headline","difficulty"],"additionalProperties":false}"#
        );
    }

    #[test]
    fn physics_request_carries_the_screenshot_and_prompt_unmodified_without_difficulty() {
        let s = shot();
        let req = physics_request(&s, "system prompt text", false);
        assert_eq!(req.system, "system prompt text");
        assert_eq!(req.user, "Check my working.");
        assert_eq!(req.images, vec![s.png]);
        assert_eq!(req.effort, Effort::Unset);
        assert_eq!(req.max_tokens, 0);
        let schema = req
            .schema
            .expect("schema is always present for the physics check");
        assert!(schema["properties"].get("difficulty").is_none());
    }

    #[test]
    fn physics_request_appends_the_rubric_and_difficulty_property_when_requested() {
        let req = physics_request(&shot(), "system prompt text", true);
        assert!(req.system.starts_with("system prompt text"));
        assert!(req.system.contains("difficulty"));
        let schema = req.schema.unwrap();
        assert_eq!(
            schema["required"],
            serde_json::json!(["detail", "headline", "difficulty"])
        );
    }

    // -- calendar_request (#39) -----------------------------------------

    #[test]
    fn calendar_request_carries_the_screenshot_and_prompt_unmodified() {
        let s = shot();
        let req = calendar_request(&s, "system prompt text with today's date baked in");
        assert_eq!(req.system, "system prompt text with today's date baked in");
        assert_eq!(req.user, "Find the event on screen.");
        assert_eq!(req.images, vec![s.png]);
        assert_eq!(req.effort, Effort::Unset);
        assert_eq!(req.max_tokens, 0);
    }

    #[test]
    fn calendar_request_schema_matches_the_calendar_event_registry_entry() {
        let req = calendar_request(&shot(), "prompt");
        assert_eq!(
            req.schema,
            crate::actions::schema::schema_for("calendar_event", false)
        );
    }

    #[test]
    fn golden_calendar_request_schema_is_byte_identical() {
        let req = calendar_request(&shot(), "irrelevant prompt");
        let schema = req.schema.expect("schema always present");
        assert_eq!(
            serde_json::to_string(&schema).unwrap(),
            r#"{"type":"object","properties":{"title":{"type":"string","editable":true},"start":{"type":"string","editable":true},"end":{"type":"string"},"location":{"type":"string"},"notes":{"type":"string"}},"required":["title","start","end","location","notes"],"additionalProperties":false}"#
        );
    }

    // -- review_request_from_text / review_request_from_screen (#38) ------

    #[test]
    fn review_request_from_text_has_no_image_and_carries_the_captured_text() {
        let req = review_request_from_text("system prompt", "captured email body");
        assert_eq!(req.system, "system prompt");
        assert_eq!(req.user, "captured email body");
        assert!(req.images.is_empty());
        assert_eq!(req.effort, Effort::Unset);
        assert_eq!(req.max_tokens, 0);
    }

    #[test]
    fn review_request_from_text_schema_matches_the_text_review_registry_entry() {
        let req = review_request_from_text("prompt", "text");
        assert_eq!(
            req.schema,
            crate::actions::schema::schema_for("text_review", false)
        );
    }

    #[test]
    fn review_request_from_screen_carries_the_screenshot() {
        let s = shot();
        let req = review_request_from_screen("system prompt", &s);
        assert_eq!(req.system, "system prompt");
        assert_eq!(req.images, vec![s.png]);
        assert_eq!(
            req.schema,
            crate::actions::schema::schema_for("text_review", false)
        );
    }

    #[test]
    fn parse_answer_extracts_detail_headline_and_difficulty() {
        let answer = parse_answer(r#"{"detail":"d","headline":"h","difficulty":"7"}"#).unwrap();
        assert_eq!(answer.detail, "d");
        assert_eq!(answer.headline, "h");
        assert_eq!(answer.difficulty, Some(Difficulty::Level(7)));
    }

    #[test]
    fn parse_answer_degrades_unparseable_difficulty_to_none() {
        let answer =
            parse_answer(r#"{"detail":"d","headline":"h","difficulty":"way too hard"}"#).unwrap();
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_answer_missing_difficulty_key_is_none() {
        let answer = parse_answer(r#"{"detail":"d","headline":"h"}"#).unwrap();
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_answer_rejects_invalid_json() {
        let err = parse_answer("not json").unwrap_err();
        assert!(err.to_string().contains("not a valid Answer"));
    }

    // -- Caps / ImageLimits / Chain::first_ready_caps (issue #169) -------

    #[test]
    fn caps_default_has_no_image_limits() {
        assert_eq!(Caps::default().image_limits, None);
    }

    #[test]
    fn provider_default_own_caps_is_caps_default() {
        // MockProvider does not override `own_caps`, so it must fall back
        // to the trait's default (`Caps::default()`), proving the default
        // method itself works without every test double needing its own
        // impl.
        let p = MockProvider {
            id: "a",
            ready: true,
            calls: AtomicU32::new(0),
            result: ok_completion,
        };
        assert_eq!(p.own_caps(), Caps::default());
    }

    /// A provider whose `own_caps` is a fixed, non-default value -- lets
    /// `Chain::first_ready_caps` be tested against a real, distinguishable
    /// `Caps` rather than only the trait's default.
    struct FakeCapsProvider {
        id: &'static str,
        ready: bool,
        caps: Caps,
    }

    impl Provider for FakeCapsProvider {
        fn id(&self) -> &'static str {
            self.id
        }
        fn ready(&self) -> bool {
            self.ready
        }
        fn capabilities(&self, _model: &str) -> Caps {
            self.caps
        }
        fn own_caps(&self) -> Caps {
            self.caps
        }
        fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
            unreachable!("first_ready_caps must never call complete")
        }
    }

    #[test]
    fn first_ready_caps_returns_the_first_ready_providers_caps() {
        let limits = ImageLimits {
            max_long_edge: 1234,
            max_pixels: 5678,
        };
        let a = FakeCapsProvider {
            id: "a",
            ready: true,
            caps: Caps {
                image_limits: Some(limits),
                ..Caps::default()
            },
        };
        let b = FakeCapsProvider {
            id: "b",
            ready: true,
            caps: Caps::default(),
        };
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        assert_eq!(
            chain.first_ready_caps().and_then(|c| c.image_limits),
            Some(limits)
        );
    }

    #[test]
    fn first_ready_caps_skips_unready_providers() {
        let limits = ImageLimits {
            max_long_edge: 42,
            max_pixels: 99,
        };
        let unready = FakeCapsProvider {
            id: "unready",
            ready: false,
            caps: Caps {
                image_limits: Some(ImageLimits {
                    max_long_edge: 1,
                    max_pixels: 1,
                }),
                ..Caps::default()
            },
        };
        let ready = FakeCapsProvider {
            id: "ready",
            ready: true,
            caps: Caps {
                image_limits: Some(limits),
                ..Caps::default()
            },
        };
        let chain = Chain::new(vec![Box::new(unready), Box::new(ready)]);
        assert_eq!(
            chain.first_ready_caps().and_then(|c| c.image_limits),
            Some(limits)
        );
    }

    #[test]
    fn first_ready_caps_is_none_when_nothing_is_ready() {
        let unready = FakeCapsProvider {
            id: "unready",
            ready: false,
            caps: Caps::default(),
        };
        let chain = Chain::new(vec![Box::new(unready)]);
        assert_eq!(chain.first_ready_caps(), None);
    }

    // -- strip_near_json (issue #200) ------------------------------------

    #[test]
    fn strip_near_json_table() {
        let cases: &[(&str, &str)] = &[
            // Already valid: passes through unchanged.
            (r#"{"a":1}"#, r#"{"a":1}"#),
            // A ```json ... ``` fence.
            ("```json\n{\"a\":1}\n```", r#"{"a":1}"#),
            // A bare ``` ... ``` fence with no language tag.
            ("```\n{\"a\":1}\n```", r#"{"a":1}"#),
            // A fence with no surrounding newlines at all.
            ("```{\"a\":1}```", r#"{"a":1}"#),
            // Prose before and after the object.
            (
                "Sure, here is the JSON you asked for:\n{\"a\":1}\nHope that helps!",
                r#"{"a":1}"#,
            ),
            // Nested braces inside a string value must not confuse the
            // balanced scan -- both the depth-affecting chars and the
            // escaped quote inside the string are on one side of it.
            (r#"{"a":"a { b } \" c { d"}"#, r#"{"a":"a { b } \" c { d"}"#),
            // Nested (legitimate) object structure.
            (r#"{"a":{"b":2}}"#, r#"{"a":{"b":2}}"#),
            // Two top-level JSON objects: only the first is kept.
            (r#"{"a":1} {"b":2}"#, r#"{"a":1}"#),
            // Fenced AND prose-wrapped together.
            ("Here you go:\n```json\n{\"a\":1}\n```\nDone.", r#"{"a":1}"#),
        ];
        for (input, expected) in cases {
            assert_eq!(
                strip_near_json(input),
                *expected,
                "failed for input {input:?}"
            );
        }
    }

    #[test]
    fn strip_near_json_unbalanced_is_returned_unchanged() {
        // No matching close brace: strip_near_json must not fabricate one,
        // so the caller's subsequent parse attempt still fails and the
        // network repair pass still runs.
        let input = r#"{"a":1"#;
        assert_eq!(strip_near_json(input), input);
    }

    #[test]
    fn strip_near_json_no_object_at_all_is_returned_unchanged() {
        let input = "I cannot see a problem on the screen.";
        assert_eq!(strip_near_json(input), input);
    }

    #[test]
    fn complete_parsed_local_cleanup_succeeds_no_repair_request_sent() {
        // Only ONE result is scripted: a fenced-JSON response. If a repair
        // round trip were attempted (the pre-#200 behaviour), the second
        // `complete()` call would panic on the empty queue. A passing test
        // here proves the local cleanup alone recovered the answer.
        let fenced = SequencedProvider::new(
            "a",
            vec![|| {
                Ok(Completion {
                    text: "```json\n{\"detail\":\"d\",\"headline\":\"h\"}\n```".to_string(),
                    usage: None,
                    stop: StopReason::Complete,
                })
            }],
        );
        let chain = Chain::new(vec![Box::new(fenced)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.detail, "d");
        assert_eq!(answer.headline, "h");
    }

    // -- truncate_chars (issue #18/#206) ---------------------------------

    #[test]
    fn truncate_chars_is_a_no_op_under_the_limit() {
        assert_eq!(truncate_chars("hello", 10), ("hello".to_string(), false));
    }

    #[test]
    fn truncate_chars_is_a_no_op_exactly_at_the_limit() {
        assert_eq!(truncate_chars("hello", 5), ("hello".to_string(), false));
    }

    #[test]
    fn truncate_chars_cuts_one_char_over_the_limit_and_reports_truncated() {
        assert_eq!(truncate_chars("hello", 4), ("hell".to_string(), true));
    }

    #[test]
    fn truncate_chars_splits_on_char_boundaries_not_bytes() {
        // Each of these is a multi-byte UTF-8 character; a byte-length
        // truncation would split one in half and either panic or produce
        // invalid UTF-8. Taking 2 *characters* must keep both whole.
        let s = "héllo"; // 'é' is 2 bytes in UTF-8
        let (out, truncated) = truncate_chars(s, 2);
        assert_eq!(out, "hé");
        assert!(truncated);
    }

    #[test]
    fn truncate_chars_empty_input_is_never_truncated() {
        assert_eq!(truncate_chars("", 10), ("".to_string(), false));
    }

    #[test]
    fn truncate_chars_zero_limit_on_nonempty_input_truncates_to_empty() {
        assert_eq!(truncate_chars("hello", 0), ("".to_string(), true));
    }

    // -- non_vision_request: golden request-building (issue #18/#206) ----

    fn base_req_with_image() -> Request {
        physics_request(
            &Shot {
                png: vec![1, 2, 3],
                width: 10,
                height: 10,
            },
            "irrelevant system prompt",
            false,
        )
    }

    #[test]
    fn non_vision_request_drops_every_image() {
        let base = base_req_with_image();
        assert!(
            !base.images.is_empty(),
            "test setup: base must carry an image"
        );
        let nv = non_vision_request(&base, "some ocr text", "");
        assert!(
            nv.images.is_empty(),
            "image absent from the non-vision request"
        );
    }

    #[test]
    fn non_vision_request_preserves_schema_effort_and_max_tokens() {
        let mut base = base_req_with_image();
        base.effort = Effort::High;
        base.max_tokens = 777;
        let nv = non_vision_request(&base, "text", "fields");
        assert_eq!(nv.schema, base.schema, "schema order/content unchanged");
        assert_eq!(nv.effort, Effort::High);
        assert_eq!(nv.max_tokens, 777);
    }

    #[test]
    fn non_vision_request_schema_is_byte_identical_to_the_original() {
        // The golden schema (#23) must not shift just because the request
        // went through the non-vision rewrite -- property order is
        // load-bearing (rule 3).
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "text", "fields");
        assert_eq!(
            serde_json::to_string(&nv.schema.unwrap()).unwrap(),
            r#"{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false}"#
        );
    }

    #[test]
    fn non_vision_request_system_carries_the_preface_before_the_original_system() {
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "text", "fields");
        assert!(nv.system.contains("no image input"));
        assert!(nv.system.contains(&base.system));
        assert!(
            nv.system.find("no image input").unwrap() < nv.system.find(&base.system).unwrap(),
            "the preface must come before the original system prompt"
        );
    }

    #[test]
    fn non_vision_request_preface_never_asks_for_reasoning() {
        // CLAUDE.md rule 10's trap: wording that reads as asking the model
        // to explain its own reasoning gets refused (MEASURED 2026-09-15).
        // This describes the INPUT format only.
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "text", "fields");
        for banned in [
            "scratchpad",
            "reason it out",
            "explain your reasoning",
            "think step by step",
        ] {
            assert!(!nv.system.to_lowercase().contains(banned));
        }
    }

    #[test]
    fn non_vision_request_user_contains_the_ocr_text_and_uia_fields() {
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "the quick brown fox", "Edit \"Name\": Jane");
        assert!(nv.user.contains("the quick brown fox"));
        assert!(nv.user.contains("Edit \"Name\": Jane"));
        // The original user turn is preserved too, not replaced.
        assert!(nv.user.contains(&base.user));
    }

    #[test]
    fn non_vision_request_empty_ocr_and_uia_get_explicit_placeholders() {
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "", "");
        assert!(nv.user.contains("no text recognized"));
        assert!(nv.user.contains("no fields found"));
    }

    #[test]
    fn non_vision_request_truncates_long_ocr_text_and_notes_it_in_the_system_prompt() {
        let base = base_req_with_image();
        let long_text: String = "a".repeat(MAX_NON_VISION_CHARS + 500);
        let nv = non_vision_request(&base, &long_text, "");
        // The user text must not carry the untruncated blob.
        assert!(!nv.user.contains(&long_text));
        let kept: String = "a".repeat(MAX_NON_VISION_CHARS);
        assert!(nv.user.contains(&kept));
        assert!(
            nv.system.contains("truncated"),
            "system prompt must note the truncation: {:?}",
            nv.system
        );
    }

    #[test]
    fn non_vision_request_short_ocr_text_is_not_flagged_as_truncated() {
        let base = base_req_with_image();
        let nv = non_vision_request(&base, "short text", "");
        assert!(!nv.system.contains("truncated"));
    }

    // -- Chain::complete_parsed_with_fallback (issue #18/#206) -----------

    /// A [`Provider`] test double whose vision capability is set directly
    /// (not derived from a model string) and which records every [`Request`]
    /// it was actually asked to complete, so a test can assert on the exact
    /// request a given provider received -- whether the original (image
    /// included) or [`non_vision_request`]'s rewrite. `seen` is an `Arc` so
    /// a test can keep a handle to it after the provider is moved into a
    /// `Chain` (mirrors `SequencedProvider::new_counted`'s same reason for
    /// using `Arc` above).
    struct VisionAwareProvider {
        id: &'static str,
        vision: bool,
        ready: bool,
        seen: std::sync::Arc<std::sync::Mutex<Vec<Request>>>,
        result: fn() -> anyhow::Result<Completion>,
    }

    impl VisionAwareProvider {
        fn new(id: &'static str, vision: bool, result: fn() -> anyhow::Result<Completion>) -> Self {
            Self {
                id,
                vision,
                ready: true,
                seen: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                result,
            }
        }

        fn seen_handle(&self) -> std::sync::Arc<std::sync::Mutex<Vec<Request>>> {
            self.seen.clone()
        }
    }

    impl Provider for VisionAwareProvider {
        fn id(&self) -> &'static str {
            self.id
        }
        fn ready(&self) -> bool {
            self.ready
        }
        fn capabilities(&self, _model: &str) -> Caps {
            Caps {
                vision: self.vision,
                ..Caps::default()
            }
        }
        fn own_caps(&self) -> Caps {
            Caps {
                vision: self.vision,
                ..Caps::default()
            }
        }
        fn complete(&self, req: &Request) -> anyhow::Result<Completion> {
            self.seen.lock().unwrap().push(req.clone());
            (self.result)()
        }
    }

    fn non_vision_inputs_for_test() -> NonVisionInputs {
        NonVisionInputs {
            ocr_text: "OCR SAW: 17 + 25".to_string(),
            uia_fields: "Edit \"Answer\": 42".to_string(),
        }
    }

    #[test]
    fn fallback_is_never_invoked_when_every_ready_provider_has_vision() {
        let calls = std::cell::Cell::new(0u32);
        let a = VisionAwareProvider::new("a", true, ok_completion);
        let chain = Chain::new(vec![Box::new(a)]);

        let answer = chain
            .complete_parsed_with_fallback(
                &req(),
                || {
                    calls.set(calls.get() + 1);
                    Ok(non_vision_inputs_for_test())
                },
                |c| parse_answer(&c.text),
            )
            .unwrap();

        assert_eq!(answer.headline, "42");
        assert_eq!(
            calls.get(),
            0,
            "fallback must never run for a vision-only chain"
        );
    }

    #[test]
    fn fallback_is_never_invoked_when_the_request_carries_no_image() {
        let calls = std::cell::Cell::new(0u32);
        let mut no_image_req = req();
        no_image_req.images = Vec::new();
        let a = VisionAwareProvider::new("a", false, ok_completion);
        let chain = Chain::new(vec![Box::new(a)]);

        let answer = chain
            .complete_parsed_with_fallback(
                &no_image_req,
                || {
                    calls.set(calls.get() + 1);
                    Ok(non_vision_inputs_for_test())
                },
                |c| parse_answer(&c.text),
            )
            .unwrap();

        assert_eq!(answer.headline, "42");
        assert_eq!(
            calls.get(),
            0,
            "nothing to replace when there is no image to begin with"
        );
    }

    #[test]
    fn fallback_is_invoked_exactly_once_and_reused_across_two_non_vision_providers() {
        let calls = std::cell::Cell::new(0u32);
        let a = VisionAwareProvider::new("a", false, || Err(anyhow::anyhow!("a transport failed")));
        let b = VisionAwareProvider::new("b", false, ok_completion);
        let seen_b = b.seen_handle();
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);

        let answer = chain
            .complete_parsed_with_fallback(
                &req(),
                || {
                    calls.set(calls.get() + 1);
                    Ok(non_vision_inputs_for_test())
                },
                |c| parse_answer(&c.text),
            )
            .unwrap();

        assert_eq!(answer.headline, "42");
        assert_eq!(
            calls.get(),
            1,
            "computed once, reused for the second provider"
        );

        let b_requests = seen_b.lock().unwrap();
        assert_eq!(b_requests.len(), 1);
        assert!(b_requests[0].images.is_empty());
        assert!(b_requests[0].user.contains("OCR SAW: 17 + 25"));
        assert!(b_requests[0].user.contains("Edit \"Answer\": 42"));
    }

    #[test]
    fn a_vision_provider_later_in_the_chain_still_receives_the_real_image() {
        let a = VisionAwareProvider::new("a", false, || Err(anyhow::anyhow!("a transport failed")));
        let b = VisionAwareProvider::new("b", true, ok_completion);
        let seen_b = b.seen_handle();
        let chain = Chain::new(vec![Box::new(a), Box::new(b)]);
        let base = req();

        let answer = chain
            .complete_parsed_with_fallback(
                &base,
                || Ok(non_vision_inputs_for_test()),
                |c| parse_answer(&c.text),
            )
            .unwrap();

        assert_eq!(answer.headline, "42");
        let b_requests = seen_b.lock().unwrap();
        assert_eq!(b_requests.len(), 1);
        assert_eq!(
            b_requests[0].images, base.images,
            "the vision provider must see the real, unmodified image"
        );
    }

    #[test]
    fn ocr_fallback_failure_skips_every_provider_that_needed_it_with_a_named_reason() {
        struct PanicsIfCalled;
        impl Provider for PanicsIfCalled {
            fn id(&self) -> &'static str {
                "panics"
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps {
                    vision: false,
                    ..Caps::default()
                }
            }
            fn own_caps(&self) -> Caps {
                Caps {
                    vision: false,
                    ..Caps::default()
                }
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                panic!("a provider that needed the failed fallback must never be asked");
            }
        }

        let calls = std::cell::Cell::new(0u32);
        let chain = Chain::new(vec![Box::new(PanicsIfCalled), Box::new(PanicsIfCalled)]);

        let err = chain
            .complete_parsed_with_fallback(
                &req(),
                || {
                    calls.set(calls.get() + 1);
                    Err(anyhow::anyhow!("no OCR language pack installed"))
                },
                |c| parse_answer(&c.text),
            )
            .unwrap_err();

        assert_eq!(
            calls.get(),
            1,
            "the failed fallback is cached too -- never retried per provider"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("panics"),
            "must name the skipped provider: {msg:?}"
        );
        assert!(
            msg.contains("no OCR language pack installed"),
            "must carry the OCR failure reason: {msg:?}"
        );
    }

    #[test]
    fn ocr_fallback_failure_still_falls_through_to_a_provider_that_has_vision() {
        struct PanicsIfCalled;
        impl Provider for PanicsIfCalled {
            fn id(&self) -> &'static str {
                "no-vision"
            }
            fn capabilities(&self, _model: &str) -> Caps {
                Caps {
                    vision: false,
                    ..Caps::default()
                }
            }
            fn own_caps(&self) -> Caps {
                Caps {
                    vision: false,
                    ..Caps::default()
                }
            }
            fn complete(&self, _req: &Request) -> anyhow::Result<Completion> {
                panic!("must never be asked: its fallback failed");
            }
        }

        let vision_provider = VisionAwareProvider::new("has-vision", true, ok_completion);
        let chain = Chain::new(vec![Box::new(PanicsIfCalled), Box::new(vision_provider)]);

        let answer = chain
            .complete_parsed_with_fallback(
                &req(),
                || Err(anyhow::anyhow!("ocr unavailable")),
                |c| parse_answer(&c.text),
            )
            .unwrap();
        assert_eq!(answer.headline, "42");
    }

    #[test]
    fn complete_parsed_local_cleanup_fails_falls_through_to_network_repair() {
        // Prose with no JSON object at all: local cleanup cannot recover
        // it, so the existing #99 repair pass must still run. Two results
        // scripted (bad, then a valid repair) -- if local cleanup wrongly
        // "succeeded" on garbage, only one call would happen and the
        // second scripted result would be left unused (still fine), but if
        // local cleanup wrongly reported success with WRONG content the
        // assertion below would catch it.
        let a = SequencedProvider::new(
            "a",
            vec![
                || {
                    Ok(Completion {
                        text: "I don't know what you mean.".to_string(),
                        usage: None,
                        stop: StopReason::Complete,
                    })
                },
                repaired_completion,
            ],
        );
        let chain = Chain::new(vec![Box::new(a)]);
        let answer = chain
            .complete_parsed(&req(), |c| parse_answer(&c.text))
            .unwrap();
        assert_eq!(answer.headline, "repaired");
    }

    // -- live check: OCR -> real chain -> local text-only Ollama (#18/#206) --
    //
    // Not run by default (`cargo test` / `cargo test provider` never touch
    // the network or OCR). Run explicitly:
    // `cargo test provider::tests::non_vision_fallback_live -- --ignored --nocapture`
    // Requires Ollama running locally on 127.0.0.1:11434 with `llama3.2:3b`
    // pulled (a text-only model -- see `ollama::is_vision_model`, which does
    // not list the `llama3.2` family).
    #[test]
    #[ignore = "live: OCR + a real local Ollama call; run manually, see this test's doc comment"]
    fn non_vision_fallback_live_ocr_to_text_only_ollama_answers_arithmetic() {
        use crate::provider::ollama::{Ollama, DEFAULT_BASE_URL};

        // A synthetic "screenshot": black text on a white canvas rendered
        // with GDI, the same technique `ocr.rs`'s own live test uses
        // (duplicated here, deliberately small, rather than imported --
        // this module's task is read-only USE of `crate::ocr`'s public
        // API, not a dependency on that module's private test helpers).
        let (width, height) = (900u32, 160u32);
        let rgba = unsafe { render_gdi_text_rgba_for_ocr("What is 17 + 25 ?", width, height) };

        let fallback_calls = std::cell::Cell::new(0u32);
        let fallback = || -> anyhow::Result<NonVisionInputs> {
            fallback_calls.set(fallback_calls.get() + 1);
            let ocr_out = crate::ocr::recognize(&rgba, width, height, crate::ocr::DEFAULT_TIMEOUT)
                .context("OCR unavailable for the live non-vision fallback check")?;
            Ok(NonVisionInputs {
                ocr_text: crate::ocr::serialize_lines(&ocr_out.lines),
                uia_fields: String::new(),
            })
        };

        // The base request carries a placeholder image -- never actually
        // sent, since `llama3.2:3b` has no vision and the fallback above
        // replaces it -- proving the SUBSTITUTION happens, not merely that
        // a text-only chat call works.
        let base_req = Request {
            system: "Reply with exactly two JSON fields: headline (the numeric answer, at most ten words) and detail (a one-sentence explanation of the arithmetic).".to_string(),
            user: "Solve the arithmetic question in the OCR text below.".to_string(),
            images: vec![vec![0u8; 4]],
            schema: Some(serde_json::json!({
                "type": "object",
                "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                "required": ["detail", "headline"],
                "additionalProperties": false
            })),
            effort: Effort::Unset,
            max_tokens: 0,
        };

        let mut provider = Ollama::new(DEFAULT_BASE_URL, "llama3.2:3b", "low");
        // Unload the model right after this one-off check (mirrors
        // `ollama.rs`'s own live-check convention).
        provider.keep_alive = "0".to_string();
        let chain = Chain::new(vec![Box::new(provider)]);

        let started = std::time::Instant::now();
        let answer = chain
            .complete_parsed_with_fallback(&base_req, fallback, |c| parse_answer(&c.text))
            .expect("live chain call with the non-vision fallback should succeed");
        let elapsed = started.elapsed();

        assert_eq!(
            fallback_calls.get(),
            1,
            "OCR/UIA fallback runs exactly once"
        );
        assert!(!answer.headline.is_empty());
        eprintln!(
            "MEASURED 2026-09-17: non_vision_fallback_live: model=llama3.2:3b elapsed={elapsed:?} headline={:?}",
            answer.headline
        );
    }

    // -- live check: #39's "Add event from screen", real chain -> gemma3:4b -
    //
    // Not run by default. Run explicitly:
    // `cargo test provider::tests::calendar_add_live -- --ignored --nocapture`
    // Requires Ollama running locally on 127.0.0.1:11434 with `gemma3:4b`
    // pulled (vision model, per `ollama::VISION_FAMILIES`).
    #[test]
    #[ignore = "live: a real local Ollama vision call; run manually, see this test's doc comment"]
    fn calendar_add_live_extracts_a_friday_event_from_a_gdi_rendered_image() {
        use crate::provider::ollama::{Ollama, DEFAULT_BASE_URL};

        // GDI-rendered black-on-white text, the same technique
        // `render_gdi_text_rgba_for_ocr` (below) and `ocr.rs`'s own live
        // test use -- realistic enough for #39's Done-when to measure the
        // real extraction path, not just wire-format round-tripping.
        let (width, height) = (1000u32, 160u32);
        let rgba =
            unsafe { render_gdi_text_rgba_for_ocr("Team sync Friday 3pm, Room 4B", width, height) };
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            image::codecs::png::PngEncoder::new(&mut png)
                .write_image(&rgba, width, height, image::ExtendedColorType::Rgba8)
                .expect("encode synthetic PNG");
        }
        let shot = Shot { png, width, height };

        // A fixed reference date (2026-09-17, a Thursday -- MEASURED via
        // `date -d 2026-09-17 +%A`), not "now": keeps the "Friday" -> a
        // specific expected calendar date assertion below meaningful
        // regardless of what day this is actually run on.
        let today = crate::connectors::civil_time::CivilDate {
            year: 2026,
            month: 9,
            day: 17,
        };
        let prompt = crate::actions::calendar::build_prompt(
            crate::actions::calendar::BASE_PROMPT,
            today,
            0, // states UTC, so the model's answer is easy to check verbatim
        );
        let req = calendar_request(&shot, &prompt);

        let mut provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        // Unload the model right after this one-off check (mirrors
        // `ollama.rs`'s own live-check convention, and #13's Done-when).
        provider.keep_alive = "0".to_string();

        let started = std::time::Instant::now();
        let completion = provider
            .complete(&req)
            .expect("live ollama request should succeed");
        let elapsed = started.elapsed();

        let proposal = crate::actions::calendar::parse_calendar_proposal(&completion.text)
            .expect("response should parse as a calendar_event proposal");
        eprintln!(
            "MEASURED 2026-09-17: calendar_add_live: model=gemma3:4b elapsed={elapsed:?} proposal={proposal}"
        );

        assert!(
            !crate::actions::calendar::is_no_event(&proposal),
            "expected a real event, got the no-event sentinel: {proposal}"
        );
        assert!(!proposal["title"].as_str().unwrap_or_default().is_empty());
        let start = proposal["start"].as_str().unwrap_or_default();
        assert!(
            start.starts_with("2026-09-18"),
            "expected a Friday (2026-09-18, the day after the fixed 2026-09-17 Thursday \
             reference date) start, got {start:?}"
        );
    }

    /// Test-only GDI text renderer for the live check above -- black text on
    /// a white canvas, returned as opaque RGBA8 (matching what a real
    /// screenshot's alpha channel always is). Deliberately duplicated from
    /// (not imported from) `ocr.rs`'s own private test helper of the same
    /// shape: this module's task scope is read-only USE of `crate::ocr`'s
    /// public API, never a dependency on another module's `#[cfg(test)]`
    /// internals.
    #[cfg(test)]
    unsafe fn render_gdi_text_rgba_for_ocr(text: &str, width: u32, height: u32) -> Vec<u8> {
        use windows::Win32::Foundation::{COLORREF, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
            DeleteObject, FillRect, SelectObject, SetBkColor, SetTextColor, ANSI_CHARSET,
            BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DEFAULT_QUALITY,
            DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, OUT_DEFAULT_PRECIS,
        };

        let hdc = CreateCompatibleDC(None);
        assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0,
                ..Default::default()
            },
            ..Default::default()
        };

        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let hbitmap = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .expect("CreateDIBSection failed");
        assert!(!bits.is_null(), "CreateDIBSection returned a null buffer");

        let old_bitmap = SelectObject(hdc, hbitmap.into());

        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        FillRect(hdc, &rect, white);
        let _ = DeleteObject(white.into());

        let hfont = CreateFontW(
            -40,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            ANSI_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            DEFAULT_PITCH.0 as u32,
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, hfont.into());
        SetTextColor(hdc, COLORREF(0x0000_0000));
        SetBkColor(hdc, COLORREF(0x00FF_FFFF));

        // Routed through the shared guard (issue #221) rather than a raw
        // `DrawTextW` call -- see `ocr.rs`'s identical comment on its own
        // (deliberately duplicated) copy of this test helper.
        crate::ui::text::draw_text_line(hdc, text, rect, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

        let pixel_count = (width as usize) * (height as usize) * 4;
        let bgra = std::slice::from_raw_parts(bits as *const u8, pixel_count).to_vec();

        SelectObject(hdc, old_font);
        let _ = DeleteObject(hfont.into());
        SelectObject(hdc, old_bitmap);
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(hdc);

        let mut rgba = Vec::with_capacity(pixel_count);
        for px in bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        rgba
    }
}
