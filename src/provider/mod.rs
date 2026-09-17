mod common;
pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod ollama_admin;
pub mod openai;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::Value;

pub use anthropic::Anthropic;
pub use gemini::Gemini;
pub use ollama::Ollama;
pub use openai::OpenAi;
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
/// Nothing in Phase 1 reads `capabilities()` yet -- there is no router or
/// action picker to consult it -- so it and this type are unused outside
/// tests until Phase 2 lands (`#[allow(dead_code)]`, same as `Shot`'s
/// `width`/`height` below and `Chain::provider_names`).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Caps {
    pub vision: bool,
    pub json_schema: bool,
    pub thinking: bool,
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
    /// Zero or more images alongside `user`. A provider without vision for
    /// the requested model should still attempt a text-only completion
    /// rather than error outright -- the caller is expected to have already
    /// substituted OCR text or the UIA tree into `user` when that matters
    /// (Phase 2; today `images` is always exactly the one screenshot).
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
    /// just per provider. Unused outside tests until Phase 2's router and
    /// action picker exist to consult it.
    #[allow(dead_code)]
    fn capabilities(&self, model: &str) -> Caps;

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
    pub fn complete(&self, req: &Request) -> anyhow::Result<Completion> {
        let mut first_err: Option<anyhow::Error> = None;

        for provider in &self.providers {
            if !provider.ready() {
                continue;
            }
            match provider.complete(req) {
                Ok(completion) => return Ok(completion),
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
        schema: Some(common::answer_schema(want_difficulty)),
        effort: Effort::Unset,
        max_tokens: 0,
    }
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
    let raw: RawAnswer = serde_json::from_str(text).context("provider: completion text is not a valid Answer")?;
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

    fn ok_completion() -> anyhow::Result<Completion> {
        Ok(Completion {
            text: r#"{"detail":"because reasons","headline":"42"}"#.to_string(),
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
            assert_eq!(Difficulty::parse(s), Some(Difficulty::Ultra), "failed for {s:?}");
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

    // -- physics_request / parse_answer ---------------------------------

    #[test]
    fn physics_request_carries_the_screenshot_and_prompt_unmodified_without_difficulty() {
        let s = shot();
        let req = physics_request(&s, "system prompt text", false);
        assert_eq!(req.system, "system prompt text");
        assert_eq!(req.user, "Check my working.");
        assert_eq!(req.images, vec![s.png]);
        assert_eq!(req.effort, Effort::Unset);
        assert_eq!(req.max_tokens, 0);
        let schema = req.schema.expect("schema is always present for the physics check");
        assert!(schema["properties"].get("difficulty").is_none());
    }

    #[test]
    fn physics_request_appends_the_rubric_and_difficulty_property_when_requested() {
        let req = physics_request(&shot(), "system prompt text", true);
        assert!(req.system.starts_with("system prompt text"));
        assert!(req.system.contains("difficulty"));
        let schema = req.schema.unwrap();
        assert_eq!(schema["required"], serde_json::json!(["detail", "headline", "difficulty"]));
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
        let answer = parse_answer(r#"{"detail":"d","headline":"h","difficulty":"way too hard"}"#).unwrap();
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
}
