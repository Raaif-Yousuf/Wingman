//! Intent router (#24): one cheap classification call, started on a worker
//! thread the moment the Quick Ask palette opens, that guesses which
//! catalogue action the screen in front of the user is probably about.
//!
//! Pure module (CLAUDE.md rule 8): no `windows` dependency anywhere in this
//! file. `app.rs` owns every piece of Win32/network wiring this needs --
//! capturing the downscaled screenshot on the main thread (mirroring
//! `App::ask`'s own capture-before-spawn ordering), picking and constructing
//! the actual `Provider` to call, spawning the worker thread, and posting the
//! result back to the palette. `ui::palette`/`ui::palette_model` own the
//! palette-side half: pre-selecting a row and rendering the summary line.
//!
//! See the 2026-09-16 expansion plan §6 "The intent router",
//! `docs/superpowers/specs/2026-09-17-palette-design.md`, and `gh issue view 24`.
//!
//! # What this module does NOT do
//!
//! It never runs an action -- [`RouterResult`] is read-only advice the
//! palette may or may not act on (see [`should_apply`]). It never decides
//! Mode or Pause; those are `app.rs`'s job before this module's functions are
//! even called (see [`cheapest_router_target`]'s doc for how Mode already
//! folds in via the caller's `ready_provider_names`).

use serde::Deserialize;
use serde_json::{json, Value};

use crate::provider::{Effort, Request};
use crate::ui::palette_model::PaletteAction;

/// Long edge (pixels) the router's screenshot is downscaled to before
/// sending -- deliberately far below `config.capture.max_edge`'s default
/// (1568) and the per-provider vision tiers in `capture.rs`: this call only
/// needs to recognize the GENRE of what's on screen (an email compose
/// window, a calendar, a physics problem), not read every character of it,
/// and it runs on every palette open rather than only on a confirmed press.
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
/// is a handful of short fields -- generous headroom over the ~40-60 tokens
/// a typical answer needs, while still bounding worst-case latency and cost
/// far below the main action's `max_tokens: 0` (provider default).
pub const ROUTER_MAX_TOKENS: u32 = 200;

/// `palette.router_threshold`'s default (`config.rs`'s `Palette::default`
/// must match this exactly -- `router_threshold_config_default_matches_router_module`
/// below proves it). #24's Done-when doesn't name a number; 0.7 mirrors the
/// expansion plan §6's own worked example confidence (0.86) being comfortably
/// above it while leaving room for a genuinely ambiguous screen to fall
/// below and change nothing.
pub const DEFAULT_ROUTER_THRESHOLD: f64 = 0.7;

const ROUTER_SYSTEM_PREFIX: &str = "You are shown a heavily downscaled screenshot of the user's screen, just before they open an action picker. Identify which listed action (if any) the screen suggests, and describe in one short sentence what's on screen. Never explain your reasoning, only report the result. Use plain text only: no markdown and no em dashes (use a full stop, a colon, or the word \"and\" or \"but\" instead).";

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
/// is load-bearing (CLAUDE.md rule 3): `summary` FIRST, so the model
/// describes what it actually sees before it commits to an intent id or a
/// confidence number -- the same reasoning `provider::common::answer_schema`
/// already applies to `detail` before `headline`.
///
/// `intent`'s `enum` is built from `candidate_ids` plus the literal
/// `"none"`, so a schema-conformant response can only ever name a real
/// catalogue id or explicitly opt out -- never a hallucinated id
/// [`parse_router_result`] would have to filter out after the fact.
pub fn router_schema(candidate_ids: &[String]) -> Value {
    let mut intent_values: Vec<Value> = candidate_ids.iter().map(|id| json!(id)).collect();
    intent_values.push(json!("none"));
    json!({
        "type": "object",
        "properties": {
            "summary": {"type": "string"},
            "intent": {"type": "string", "enum": intent_values},
            "confidence": {"type": "number"}
        },
        "required": ["summary", "intent", "confidence"],
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
    let system = format!("{ROUTER_SYSTEM_PREFIX}\n\nAvailable actions:\n{list}");
    Request {
        system,
        user: "What does this screen suggest?".to_string(),
        images: vec![image_png],
        schema: Some(router_schema(&candidate_ids)),
        effort: Effort::Low,
        max_tokens: ROUTER_MAX_TOKENS,
    }
}

/// The wire shape of the router's completion, before `intent` is validated
/// against the real candidate list. Kept separate from [`RouterResult`] for
/// the same reason `provider::RawAnswer` is separate from `Answer`: the raw
/// field needs validation ([`parse_router_result`]) before it becomes the
/// public type's `Option<String>`.
#[derive(Deserialize)]
struct RawRouterResult {
    summary: String,
    intent: String,
    confidence: f64,
}

/// The router's parsed, validated answer. `intent` is `None` for the
/// literal `"none"` OR for any id [`parse_router_result`] doesn't recognize
/// (defensive -- the schema's `enum` should already rule this out, but a
/// provider is never trusted to actually honor a schema, same posture
/// `provider::parse_answer` takes toward `difficulty`).
#[derive(Debug, Clone, PartialEq)]
pub struct RouterResult {
    pub summary: String,
    pub intent: Option<String>,
    /// Clamped to `0.0..=1.0` -- a provider returning `1.4` or `-0.2` must
    /// never let a caller's `>= threshold` comparison misbehave.
    pub confidence: f64,
}

/// Parses a completion produced from [`build_request`]'s schema.
/// `candidate_ids` is the SAME list [`router_schema`] was built from for
/// this request -- an `intent` that doesn't match any of them (or is
/// literally `"none"`) becomes `None`, never an error: a router result is
/// advisory, so an odd answer degrades to "no suggestion", not a failure the
/// palette would have to show (rule 7's "every failure ends in a card" does
/// not apply here because the router never surfaces failures to the user at
/// all -- see `app.rs`'s router hook doc comment).
pub fn parse_router_result(text: &str, candidate_ids: &[String]) -> anyhow::Result<RouterResult> {
    let raw: RawRouterResult = serde_json::from_str(text)
        .map_err(|e| anyhow::anyhow!("router: completion text is not a valid RouterResult: {e}"))?;
    let intent = if raw.intent == "none" || !candidate_ids.iter().any(|id| id == &raw.intent) {
        None
    } else {
        Some(raw.intent)
    };
    Ok(RouterResult {
        summary: raw.summary,
        intent,
        confidence: raw.confidence.clamp(0.0, 1.0),
    })
}

/// Whether the palette should act on a router result: an `intent` was
/// actually named, its `confidence` clears `threshold`, AND the user has not
/// already typed into the query box or moved the selection since the
/// palette opened (`user_interacted`). All three, in order -- a confident
/// suggestion arriving after the user already started choosing something
/// else must never yank the selection out from under them.
pub fn should_apply(
    intent: Option<&str>,
    confidence: f64,
    threshold: f64,
    user_interacted: bool,
) -> bool {
    intent.is_some() && confidence >= threshold && !user_interacted
}

/// Whether a router result belongs to a palette session that has since
/// moved on. `result_generation` is the generation number captured at the
/// moment the request was built (right after the palette that triggered it
/// was shown); `current_generation` is read fresh at delivery time
/// (`ui::palette`'s `PaletteInner::router_generation`, bumped on every show
/// AND every hide -- see that field's doc comment). A mismatch means the
/// palette was hidden and/or reshown since this request started, so the
/// result is dropped silently, never applied to whatever is on screen now.
pub fn is_stale(result_generation: u64, current_generation: u64) -> bool {
    result_generation != current_generation
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
/// and `App::toggle_palette` already build for the real action chain, so
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
/// Checks, in priority order, for a name containing a small/fast market
/// marker (`nano`, `mini`, `haiku`, `flash-lite`, `flash`, `lite`) -- this
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
    const CHEAP_MARKERS: [&str; 6] = ["nano", "mini", "haiku", "flash-lite", "flash", "lite"];
    for marker in CHEAP_MARKERS {
        if let Some(m) = models.iter().find(|m| m.to_lowercase().contains(marker)) {
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
        }
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

    #[test]
    fn router_schema_orders_summary_before_intent_before_confidence() {
        let schema = router_schema(&["check-my-work".to_string()]);
        let serialized = serde_json::to_string(&schema["properties"]).unwrap();
        let summary_pos = serialized.find("\"summary\"").unwrap();
        let intent_pos = serialized.find("\"intent\"").unwrap();
        let confidence_pos = serialized.find("\"confidence\"").unwrap();
        assert!(
            summary_pos < intent_pos && intent_pos < confidence_pos,
            "schema property order must be summary, intent, confidence: {serialized}"
        );
    }

    #[test]
    fn router_schema_intent_enum_covers_every_candidate_plus_none() {
        let ids = vec!["a".to_string(), "b".to_string()];
        let schema = router_schema(&ids);
        let enum_values: Vec<String> = schema["properties"]["intent"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            enum_values,
            vec!["a".to_string(), "b".to_string(), "none".to_string()]
        );
    }

    #[test]
    fn router_schema_forbids_additional_properties() {
        let schema = router_schema(&[]);
        assert_eq!(schema["additionalProperties"], json!(false));
        assert_eq!(
            schema["required"],
            json!(["summary", "intent", "confidence"])
        );
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

    #[test]
    fn build_request_with_no_candidates_still_builds_a_valid_request() {
        let req = build_request(vec![], &[]);
        assert!(req.system.contains("no actions available"));
        assert!(req.schema.is_some());
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

    // -- parse_router_result --------------------------------------------------

    #[test]
    fn parse_router_result_recognizes_a_listed_intent() {
        let ids = vec!["check-my-work".to_string()];
        let text = r#"{"summary":"A physics problem","intent":"check-my-work","confidence":0.9}"#;
        let result = parse_router_result(text, &ids).unwrap();
        assert_eq!(result.intent.as_deref(), Some("check-my-work"));
        assert_eq!(result.summary, "A physics problem");
        assert_eq!(result.confidence, 0.9);
    }

    #[test]
    fn parse_router_result_maps_literal_none_to_none() {
        let ids = vec!["check-my-work".to_string()];
        let text = r#"{"summary":"A blank desktop","intent":"none","confidence":0.1}"#;
        let result = parse_router_result(text, &ids).unwrap();
        assert_eq!(result.intent, None);
    }

    #[test]
    fn parse_router_result_maps_an_unrecognized_intent_to_none_not_an_error() {
        let ids = vec!["check-my-work".to_string()];
        let text = r#"{"summary":"?","intent":"hallucinated-id","confidence":0.95}"#;
        let result = parse_router_result(text, &ids).unwrap();
        assert_eq!(result.intent, None);
    }

    #[test]
    fn parse_router_result_clamps_out_of_range_confidence() {
        let ids = vec!["check-my-work".to_string()];
        let too_high = parse_router_result(
            r#"{"summary":"s","intent":"check-my-work","confidence":1.4}"#,
            &ids,
        )
        .unwrap();
        assert_eq!(too_high.confidence, 1.0);
        let too_low = parse_router_result(
            r#"{"summary":"s","intent":"check-my-work","confidence":-0.2}"#,
            &ids,
        )
        .unwrap();
        assert_eq!(too_low.confidence, 0.0);
    }

    #[test]
    fn parse_router_result_rejects_malformed_json() {
        let err = parse_router_result("not json", &[]).unwrap_err();
        assert!(err.to_string().contains("not a valid RouterResult"));
    }

    // -- should_apply: threshold and "user already chose" (#24) --------------

    #[test]
    fn should_apply_true_when_confident_and_untouched() {
        assert!(should_apply(Some("check-my-work"), 0.9, 0.7, false));
    }

    #[test]
    fn should_apply_false_when_below_threshold() {
        assert!(!should_apply(Some("check-my-work"), 0.5, 0.7, false));
    }

    #[test]
    fn should_apply_true_at_exactly_the_threshold() {
        assert!(should_apply(Some("check-my-work"), 0.7, 0.7, false));
    }

    #[test]
    fn should_apply_false_when_no_intent_was_named() {
        assert!(!should_apply(None, 0.99, 0.7, false));
    }

    #[test]
    fn should_apply_false_when_the_user_already_interacted() {
        assert!(!should_apply(Some("check-my-work"), 0.99, 0.7, true));
    }

    // -- is_stale: generation-counter staleness (#24) -------------------------

    #[test]
    fn is_stale_false_when_generations_match() {
        assert!(!is_stale(3, 3));
    }

    #[test]
    fn is_stale_true_when_the_palette_was_hidden_or_reshown_since() {
        assert!(is_stale(3, 4));
        assert!(is_stale(4, 3));
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
                "gemini-3.5-flash".to_string(),
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
