use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::common;
use super::{Caps, Completion, Effort, ImageLimits, Provider, Request, StopReason};

/// Local loopback only -- never `localhost` (AGENTS.md rule 6: IPv6-first
/// resolution on Windows stalls ~2 s per connection, MEASURED in the
/// sibling CLAIR repo).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";

/// TCP connect to loopback should be near-instant; a wedged/absent server
/// is detected fast rather than hanging for the whole exchange budget.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// The whole exchange, including a cold model load. MEASURED 2026-09-17: a
/// first-load `gemma3:4b` call on this machine (CPU-only, stock Ollama tray
/// owns the port) took ~18 s end to end; this leaves headroom for a bigger
/// model or a slower cold start.
pub const TOTAL_TIMEOUT_SECS: u64 = 180;
const TOTAL_TIMEOUT: Duration = Duration::from_secs(TOTAL_TIMEOUT_SECS);
/// Used when `Request::max_tokens` is `0` (the caller has no opinion).
const DEFAULT_MAX_TOKENS: u32 = 2500;
/// `options.num_ctx` is always sent explicitly -- the server default is
/// 4096 (see the 2026-09-16 expansion plan, "Providers and models"), which
/// is tight once a screenshot's image tokens are added in.
pub(crate) const DEFAULT_NUM_CTX: u32 = 8192;
/// `keep_alive` is a top-level request field, not inside `options`
/// (AGENTS.md rule 6: nested there it is silently ignored). This default
/// matches the expansion plan's "default 30 m"; the live-check test
/// overrides it to unload the model immediately after use.
pub(crate) const DEFAULT_KEEP_ALIVE: &str = "30m";

/// Model family prefixes known to support vision, from the 2026-09-16
/// expansion plan's hardware table and confirmed live via `/api/tags`
/// `capabilities` on this machine (MEASURED 2026-09-17: `gemma3:4b`,
/// `gemma3:12b`, `gemma4:12b`, `qwen3.5:2b/4b/9b` all report `"vision"`;
/// `qwen3:14b`, `deepseek-r1:14b`, `llama3.1:8b` do not).
///
/// This is a static allowlist, not a capability query -- issue #14 is live
/// discovery via `/api/tags` + `/api/show`. Until #14 lands, a model outside
/// this list (including one the user pulled themselves) is reported as
/// vision: false even if it actually has vision, and a typo'd family name
/// here silently drops vision support for models that have it.
const VISION_FAMILIES: [&str; 3] = ["gemma3", "gemma4", "qwen3.5"];

/// `pub(crate)`: also used by `ollama_admin.rs` as the fallback when a
/// `/api/show` response carries no live `capabilities` array (#14).
pub(crate) fn is_vision_model(model: &str) -> bool {
    VISION_FAMILIES
        .iter()
        .any(|family| model.starts_with(family))
}

use super::ollama_admin::ShowCapabilities;

/// What one `/api/show` attempt produced (#1268, #1266).
enum Entry {
    /// An answer. `capabilities` is empty for an old server that sends no
    /// list; `vision` then already holds the name-list fallback.
    Shown(ShowCapabilities),
    /// The call failed at this time. Not asked again until
    /// [`FAILURE_WINDOW`] passes (measured with `Instant`, no timer), so a
    /// stopped Ollama costs one short attempt, not one per call.
    Failed(Instant),
}

/// Per `(base_url, model)`. Filled by [`warm_caps`] on a worker thread; read
/// by `capabilities`, which never touches the network.
static CAPS_CACHE: Mutex<BTreeMap<String, Entry>> = Mutex::new(BTreeMap::new());

/// Models Ollama refused `think: true` for (400 "does not support
/// thinking"), kept for the process. Covers the case where `/api/show`
/// could not say.
static NO_THINK: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());

const FAILURE_WINDOW: Duration = Duration::from_secs(30);

fn cache_key(base_url: &str, model: &str) -> String {
    format!("{}|{}", base_url.trim_end_matches('/'), model)
}

/// A successful answer only (the live list, or the old-server fallback).
fn cache_get(base_url: &str, model: &str) -> Option<ShowCapabilities> {
    match CAPS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key(base_url, model))
    {
        Some(Entry::Shown(shown)) => Some(shown.clone()),
        _ => None,
    }
}

/// True when there is nothing to ask: an answer, or a failure still inside
/// its window.
fn already_asked(base_url: &str, model: &str) -> bool {
    match CAPS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(&cache_key(base_url, model))
    {
        Some(Entry::Shown(_)) => true,
        Some(Entry::Failed(at)) => at.elapsed() < FAILURE_WINDOW,
        None => false,
    }
}

fn cache_store(base_url: &str, model: &str, entry: Entry) {
    CAPS_CACHE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_key(base_url, model), entry);
}

#[cfg(test)]
fn cache_put(base_url: &str, model: &str, caps: Vec<String>) {
    let vision = caps.iter().any(|c| c == "vision");
    cache_store(
        base_url,
        model,
        Entry::Shown(ShowCapabilities {
            capabilities: caps,
            vision,
        }),
    );
}

fn mark_no_think(base_url: &str, model: &str) {
    NO_THINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(cache_key(base_url, model));
}

fn is_no_think(base_url: &str, model: &str) -> bool {
    NO_THINK
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&cache_key(base_url, model))
}

/// Asks Ollama what `model` supports, once per process (a failure is
/// remembered for [`FAILURE_WINDOW`]). Blocking: call from a worker thread
/// only. With no answer, callers use the name list and the 400 retry in
/// `complete`.
fn warm_caps(base_url: &str, model: &str) {
    if already_asked(base_url, model) {
        return;
    }
    let entry = match super::ollama_admin::show_capabilities(base_url, model) {
        Ok(shown) => Entry::Shown(shown),
        Err(_) => Entry::Failed(Instant::now()),
    };
    cache_store(base_url, model, entry);
}

/// Ollama's refusal of `think` for a model without a thinking mode
/// (MEASURED 2026-10-17: HTTP 400 `"<model>" does not support thinking`).
fn is_think_unsupported(err: &anyhow::Error) -> bool {
    let text = format!("{err:#}");
    text.contains("HTTP 400") && text.contains("does not support thinking")
}

pub struct Ollama {
    pub base_url: String,
    pub model: String,
    /// The configured default, used when `Request::effort` is `Effort::Unset`.
    pub effort: Effort,
    /// Top-level `keep_alive`. Defaults to [`DEFAULT_KEEP_ALIVE`]; the
    /// live-check test sets this to `"0"` so the model unloads right after.
    pub keep_alive: String,
}

impl Ollama {
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            effort: Effort::parse(&effort.into()),
            keep_alive: DEFAULT_KEEP_ALIVE.to_string(),
        }
    }

    /// Maps [`Effort`] to Ollama's `think` field.
    ///
    /// `think` is always sent explicitly, never omitted (AGENTS.md rule 6:
    /// leaving it unset on a thinking model was MEASURED 28x slower). Most
    /// vision models Wingman talks to locally (`gemma3`) have no thinking
    /// mode at all, so `think` is ignored by them either way; it only
    /// matters for a thinking-capable model (`qwen3.5`, `gemma4`).
    ///
    /// The mapping: `Unset` and `Low` -- the "quick check" end of the dial,
    /// and the pre-existing meaning of "no configured preference" -- send
    /// `false` (skip any reasoning pass, fastest). `Medium` and `High` send
    /// `true` (let the model think if it can). Ollama's `think` field also
    /// accepts `"low"`/`"medium"`/`"high"` strings on the small number of
    /// models with graded thinking effort (e.g. `gpt-oss`), which none of
    /// this repo's configured vision models are, so the boolean form is
    /// used rather than threading that distinction through.
    fn think(effort: Effort) -> bool {
        matches!(effort, Effort::Medium | Effort::High)
    }

    /// `false` only when Ollama said this model lacks `thinking`: sending
    /// `think: true` to such a model is a 400 (#1266, MEASURED 2026-10-17).
    /// With no live answer it stays `true` (today's behaviour).
    fn may_think(&self) -> bool {
        match cache_get(&self.base_url, &self.model) {
            Some(shown) if !shown.capabilities.is_empty() => {
                shown.capabilities.iter().any(|c| c == "thinking")
            }
            // No list: try, unless this model already refused (see `complete`).
            _ => !is_no_think(&self.base_url, &self.model),
        }
    }

    /// Builds the exact `/api/chat` request body. Pure and network-free so
    /// it can be unit tested directly.
    ///
    /// - `images` are plain base64 on the user message, no `data:` prefix
    ///   (Ollama's own wire shape, unlike OpenAI/Anthropic's data URLs).
    /// - `stream: false` -- without it Ollama returns newline-delimited
    ///   JSON chunks instead of one object, and this provider (like the
    ///   others, "no chat", no streaming) reads the whole body at once.
    /// - `format` carries the full JSON Schema from `req.schema`, not a
    ///   bare `"json"` string.
    /// - `keep_alive` is top-level, `options.num_ctx` is always present.
    fn build_body(&self, req: &Request) -> Value {
        let images = common::encode_images_base64(&req.images);
        let mut user_message = json!({"role": "user", "content": req.user});
        if !images.is_empty() {
            user_message["images"] = json!(images);
        }

        let effort = if req.effort != Effort::Unset {
            req.effort
        } else {
            self.effort
        };
        let max_tokens = if req.max_tokens > 0 {
            req.max_tokens
        } else {
            DEFAULT_MAX_TOKENS
        };

        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": req.system},
                user_message
            ],
            "stream": false,
            "think": Self::think(effort) && self.may_think(),
            "keep_alive": self.keep_alive,
            "options": {
                "num_ctx": DEFAULT_NUM_CTX,
                "num_predict": max_tokens
            }
        });

        if let Some(schema) = &req.schema {
            body["format"] = common::wire_schema(schema);
        }

        body
    }

    /// Parses a successful (2xx) `/api/chat` body into a `Completion`.
    /// `message.thinking` (present when `think: true` on a thinking-capable
    /// model) is discarded -- only `message.content` is ever returned as
    /// `Completion::text`.
    pub(crate) fn parse_completion(body: &str) -> Result<Completion> {
        let value: Value =
            serde_json::from_str(body).context("ollama: response body is not valid JSON")?;

        let done_reason = value.get("done_reason").and_then(Value::as_str);

        // Unlike Anthropic/OpenAI, which have no direct "the token budget
        // ran out" signal and so infer it from an empty response,
        // `done_reason: "length"` says so explicitly (#155-style handling,
        // mirrored from the sibling providers) -- MEASURED 2026-09-17: a
        // thinking model given a tiny `num_predict` returns `done_reason:
        // "length"` with `message.content` empty (all the budget spent on
        // `message.thinking`); a non-thinking model in the same situation
        // returns `done_reason: "length"` with truncated, invalid-JSON
        // `content`. Both cases are the same user-facing problem.
        //
        // #1269: with no text the reply is the stop-the-chain `Truncated`
        // error. With text it is returned as a completion with a
        // `MaxTokens` stop (the text may be a whole valid answer); the
        // chain turns an unparseable one into `Truncated`.
        let text = value
            .get("message")
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty());
        let Some(text) = text else {
            if done_reason == Some("length") {
                return Err(super::truncated_error("ollama"));
            }
            return Err(anyhow!("ollama: response has no message.content"));
        };

        let usage = Self::usage_of(&value);

        let stop = match done_reason {
            Some("stop") => StopReason::Complete,
            Some("length") => StopReason::MaxTokens,
            _ => StopReason::Other,
        };

        Ok(Completion {
            text: text.to_string(),
            usage,
            stop,
        })
    }

    /// The usage counts of a body, whether or not it holds an answer (#730).
    pub(crate) fn parse_usage(body: &str) -> Option<super::Usage> {
        Self::usage_of(&serde_json::from_str::<Value>(body).ok()?)
    }

    fn usage_of(value: &Value) -> Option<super::Usage> {
        match (
            value.get("prompt_eval_count").and_then(Value::as_u64),
            value.get("eval_count").and_then(Value::as_u64),
        ) {
            (Some(input), Some(output)) => Some(super::Usage {
                input_tokens: input as u32,
                output_tokens: output as u32,
                ..Default::default()
            }),
            _ => None,
        }
    }
}

impl Provider for Ollama {
    fn id(&self) -> &'static str {
        "ollama"
    }

    /// #105: a request to a literal loopback address stays on this machine,
    /// so it needs no send preview. Same test `provider::common`'s send
    /// guard applies to the request URL.
    fn sends_off_machine(&self) -> bool {
        crate::mode::classify_host(&self.base_url) != crate::mode::HostClass::Loopback
    }

    fn ready(&self) -> bool {
        // No API key to gate on -- a local server either answers or the
        // request fails with a transport error, which `Chain` already
        // falls through on. Only an empty base_url (a corrupt config) is
        // treated as not-ready.
        !self.base_url.trim().is_empty()
    }

    /// No network call, so it is safe on the UI thread. `vision` and
    /// `thinking` come from the live `/api/show` list when
    /// [`resolve_caps`](Provider::resolve_caps) or `complete` has cached it
    /// (#1268); until then `vision` comes from the static [`VISION_FAMILIES`]
    /// allowlist and `thinking` is `false`. `json_schema` is `true`
    /// unconditionally: Ollama's grammar-constrained `format` works for any
    /// model.
    fn capabilities(&self, model: &str) -> Caps {
        // #1268: the live list wins when it is cached (no network here);
        // otherwise the name list.
        let live = cache_get(&self.base_url, model);
        Caps {
            vision: match &live {
                Some(shown) => shown.vision,
                None => is_vision_model(model),
            },
            json_schema: true,
            thinking: live
                .as_ref()
                .is_some_and(|shown| shown.capabilities.iter().any(|c| c == "thinking")),
            // Issue #169: `capture.rs`'s conservative default
            // (`OLLAMA_CONSERVATIVE_*`) for every model -- there is no
            // vendor-documented image limit for a user-pulled local model,
            // see that constant's doc comment (THEORY, unverified).
            image_limits: Some(ImageLimits {
                max_long_edge: crate::capture::OLLAMA_CONSERVATIVE_MAX_LONG_EDGE,
                max_pixels: crate::capture::OLLAMA_CONSERVATIVE_MAX_PIXELS,
            }),
        }
    }

    fn own_caps(&self) -> Caps {
        self.capabilities(&self.model)
    }

    /// Asks `/api/show` first (once per model), so a worker thread sees the
    /// real vision answer. Never call from the UI thread.
    fn resolve_caps(&self) -> Caps {
        warm_caps(&self.base_url, &self.model);
        self.own_caps()
    }

    fn model_label(&self) -> String {
        format!("{}:{}", self.id(), self.model)
    }

    fn complete(&self, req: &Request) -> Result<Completion> {
        warm_caps(&self.base_url, &self.model);
        let body = self.build_body(req);
        let url = format!("{}/api/chat", self.base_url.trim_end_matches('/'));
        let sent_think = body["think"] == true;

        let post = |body: Value| {
            common::post_json_with_connect_timeout(
                &url,
                &[("Content-Type", "application/json")],
                body,
                CONNECT_TIMEOUT,
                TOTAL_TIMEOUT,
                "ollama",
            )
        };
        let body_text = match post(body) {
            // #1266: `/api/show` could not say, and the model has no thinking
            // mode. Remember that and resend once with `think: false`. Local
            // and unbilled, and the 400 came before any generation.
            Err(e) if sent_think && is_think_unsupported(&e) => {
                mark_no_think(&self.base_url, &self.model);
                post(self.build_body(req))?
            }
            other => other?,
        };

        crate::usage::tally(
            self.id(),
            &self.model,
            true,
            Self::parse_completion(&body_text),
            || Self::parse_usage(&body_text),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{parse_answer, physics_request_no_note, Shot};
    use std::fs;

    fn sample_shot() -> Shot {
        Shot {
            png: vec![0x89, 0x50, 0x4E, 0x47],
            width: 100,
            height: 200,
        }
    }

    fn req(prompt: &str, want_difficulty: bool) -> Request {
        physics_request_no_note(&sample_shot(), prompt, want_difficulty)
    }

    // -- base_url / default -----------------------------------------------

    #[test]
    fn default_base_url_is_never_localhost() {
        // AGENTS.md rule 6: IPv6-first resolution of `localhost` stalls
        // ~2 s per connection on Windows. Must always be the literal IPv4
        // loopback address.
        assert_eq!(DEFAULT_BASE_URL, "http://127.0.0.1:11434");
        assert!(!DEFAULT_BASE_URL.contains("localhost"));
    }

    // -- build_body ----------------------------------------------------

    #[test]
    fn build_body_posts_plain_base64_images_no_data_prefix() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("system prompt text", false));

        let expected_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &sample_shot().png,
        );
        let images = body["messages"][1]["images"]
            .as_array()
            .expect("images array");
        assert_eq!(images.len(), 1);
        assert_eq!(images[0], expected_b64);
        // No "data:image/png;base64," prefix, unlike OpenAI/Anthropic.
        assert!(!images[0].as_str().unwrap().starts_with("data:"));
    }

    #[test]
    fn build_body_carries_system_and_user_text() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("system prompt text", false));
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "system prompt text");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"], "Check my working.");
    }

    #[test]
    fn build_body_disables_streaming() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["stream"], false);
    }

    #[test]
    fn build_body_sends_full_json_schema_as_format() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(
            body["format"],
            json!({"type": "object",
                "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                "required": ["detail", "headline"], "additionalProperties": false})
        );
        // Not the bare "json" string form.
        assert!(body["format"].is_object());
    }

    /// #719: the UI-only `editable` keyword never goes out.
    #[test]
    fn no_registered_schema_sends_the_editable_keyword() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        for kind in crate::actions::schema::PROPOSAL_KINDS {
            let mut r = req("sys", false);
            r.schema = crate::actions::schema::schema_for(kind, false);
            let body = serde_json::to_string(&provider.build_body(&r)).unwrap();
            assert!(!body.contains("editable"), "{kind}: {body}");
        }
    }

    #[test]
    fn build_body_omits_format_when_request_has_no_schema() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let mut r = req("sys", false);
        r.schema = None;
        let body = provider.build_body(&r);
        assert!(body.get("format").is_none());
    }

    /// The MEASURED-28x-slower footgun (AGENTS.md rule 6): `think` must be
    /// present on every request, never omitted -- regardless of effort.
    #[test]
    fn build_body_always_sends_think_explicitly() {
        for effort in ["", "low", "medium", "high"] {
            let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", effort);
            let body = provider.build_body(&req("sys", false));
            assert!(
                body.get("think").is_some(),
                "effort {effort:?} must still send think"
            );
            assert!(body["think"].is_boolean());
        }
    }

    #[test]
    fn think_maps_low_and_unset_to_false_and_medium_high_to_true() {
        assert!(!Ollama::think(Effort::Unset));
        assert!(!Ollama::think(Effort::Low));
        assert!(Ollama::think(Effort::Medium));
        assert!(Ollama::think(Effort::High));
    }

    #[test]
    fn request_effort_override_takes_precedence_over_configured_default() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let mut r = req("sys", false);
        r.effort = Effort::High;
        let body = provider.build_body(&r);
        assert_eq!(body["think"], true);
    }

    /// `keep_alive` must be a top-level field, never nested inside
    /// `options` (AGENTS.md rule 6: nested there it is silently ignored).
    #[test]
    fn keep_alive_is_top_level_not_inside_options() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["keep_alive"], DEFAULT_KEEP_ALIVE);
        assert!(body["options"].get("keep_alive").is_none());
    }

    #[test]
    fn keep_alive_is_configurable_for_the_live_unload_check() {
        let mut provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        provider.keep_alive = "0".to_string();
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["keep_alive"], "0");
    }

    #[test]
    fn build_body_always_sends_num_ctx_explicitly() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["options"]["num_ctx"], DEFAULT_NUM_CTX);
    }

    #[test]
    fn build_body_uses_default_max_tokens_when_request_has_none() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["options"]["num_predict"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn build_body_respects_an_explicit_max_tokens() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let mut r = req("sys", false);
        r.max_tokens = 77;
        let body = provider.build_body(&r);
        assert_eq!(body["options"]["num_predict"], 77);
    }

    // -- parse_completion (recorded fixtures) ---------------------------

    #[test]
    fn parse_completion_round_trips_a_recorded_fixture() {
        let body = fs::read_to_string("tests/fixtures/ollama_response.json")
            .expect("fixture file should exist");
        let completion = Ollama::parse_completion(&body).expect("should parse");
        let answer = parse_answer(&completion.text).expect("should parse as an Answer");
        assert_eq!(answer.headline, "Four");
        assert_eq!(answer.detail, "Basic addition");
        assert_eq!(completion.stop, StopReason::Complete);
        let usage = completion.usage.expect("fixture carries token counts");
        assert_eq!(usage.input_tokens, 46);
        assert_eq!(usage.output_tokens, 21);
    }

    #[test]
    fn parse_completion_discards_message_thinking() {
        // A fixture whose message carries both `thinking` and real
        // `content` -- `thinking` must never leak into `Completion::text`.
        let body = r#"{"message": {"role": "assistant", "content": "{\"detail\":\"d\",\"headline\":\"h\"}", "thinking": "internal reasoning that must not leak"}, "done": true, "done_reason": "stop"}"#;
        let completion = Ollama::parse_completion(body).expect("should parse");
        assert!(!completion.text.contains("internal reasoning"));
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.headline, "h");
    }

    #[test]
    fn parse_completion_reports_budget_exhaustion_for_length_done_reason() {
        let body = fs::read_to_string("tests/fixtures/ollama_response_length.json")
            .expect("fixture file should exist");
        let err = Ollama::parse_completion(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
    }

    /// #1269: a `length` stop with text is still a completion.
    #[test]
    fn parse_completion_keeps_text_when_done_reason_is_length() {
        let body = r#"{"message": {"role": "assistant", "content": "{\"detail\":\"d\",\"headline\":\"h\"}"}, "done": true, "done_reason": "length"}"#;
        let c = Ollama::parse_completion(body).unwrap();
        assert_eq!(c.stop, StopReason::MaxTokens);
        assert!(crate::provider::parse_answer(&c.text).is_ok());
    }

    /// #1269: a `length` stop with no text is the stop-the-chain `Truncated`
    /// error.
    #[test]
    fn parse_completion_reports_truncated_for_length_with_no_text() {
        let body = r#"{"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "length"}"#;
        let err = Ollama::parse_completion(body).unwrap_err();
        assert!(
            err.downcast_ref::<super::super::Truncated>().is_some(),
            "{err:#}"
        );
        assert!(err.to_string().contains("too long and got cut off"));
    }

    /// Neighbour: `done_reason: "length"` with non-empty (but truncated,
    /// invalid-JSON) content must still be reported as budget exhaustion,
    /// not passed through to fail confusingly inside `parse_answer`
    /// instead. MEASURED 2026-09-17: this is the actual shape a
    /// non-thinking model returns when cut short, not just the
    /// thinking-model case the other fixture covers.
    #[test]
    fn parse_completion_returns_truncated_content_with_a_max_tokens_stop() {
        // #1269: the text is returned and the chain decides after parsing.
        let body = r#"{"message": {"role": "assistant", "content": "{\n  "}, "done": true, "done_reason": "length"}"#;
        let c = Ollama::parse_completion(body).unwrap();
        assert_eq!(c.stop, StopReason::MaxTokens);
        assert!(crate::provider::parse_answer(&c.text).is_err());
    }

    #[test]
    fn parse_completion_rejects_missing_message_content_when_not_length() {
        let body = r#"{"done": true, "done_reason": "stop"}"#;
        let err = Ollama::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no message.content"));
    }

    /// #730: usage is readable from a `length` body that is an error.
    #[test]
    fn parse_usage_reads_a_length_body_that_is_an_error() {
        let body = r#"{"message": {"role": "assistant", "content": ""}, "done": true, "done_reason": "length", "prompt_eval_count": 33, "eval_count": 64}"#;
        assert!(Ollama::parse_completion(body).is_err());
        let u = Ollama::parse_usage(body).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (33, 64));
        assert!(Ollama::parse_usage("not json").is_none());
    }

    #[test]
    fn parse_completion_rejects_invalid_json() {
        let err = Ollama::parse_completion("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    // -- ready / capabilities -------------------------------------------

    #[test]
    fn ready_does_not_require_an_api_key() {
        // Unlike OpenAI/Anthropic, an empty-string "key" concept doesn't
        // exist for Ollama -- readiness is about having a base_url at all.
        assert!(Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low").ready());
    }

    #[test]
    fn ready_is_false_for_an_empty_base_url() {
        assert!(!Ollama::new("", "gemma3:4b", "low").ready());
        assert!(!Ollama::new("   ", "gemma3:4b", "low").ready());
    }

    #[test]
    fn capabilities_reports_vision_for_known_families() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        for model in [
            "gemma3:4b",
            "gemma3:12b",
            "gemma4:12b",
            "qwen3.5:2b",
            "qwen3.5:4b",
            "qwen3.5:9b",
        ] {
            assert!(
                provider.capabilities(model).vision,
                "{model} should report vision"
            );
        }
    }

    #[test]
    fn capabilities_reports_no_vision_for_known_text_only_families() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        for model in ["qwen3:14b", "deepseek-r1:14b", "llama3.1:8b"] {
            assert!(
                !provider.capabilities(model).vision,
                "{model} should not report vision"
            );
        }
        // Guards the prefix match: "qwen3:14b" must not false-positive on
        // the "qwen3.5" family check.
        assert!(!provider.capabilities("qwen3:14b").vision);
    }

    #[test]
    fn capabilities_reports_json_schema_always_and_thinking_never_yet() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        let caps = provider.capabilities("gemma3:4b");
        assert!(caps.json_schema);
        assert!(!caps.thinking);
    }

    // -- image_limits (issue #169) ---------------------------------------

    #[test]
    fn capabilities_reports_the_conservative_default_regardless_of_model() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        for model in ["gemma3:4b", "qwen3:14b"] {
            let limits = provider
                .capabilities(model)
                .image_limits
                .unwrap_or_else(|| panic!("expected image_limits for {model}"));
            assert_eq!(
                limits.max_long_edge,
                crate::capture::OLLAMA_CONSERVATIVE_MAX_LONG_EDGE
            );
            assert_eq!(
                limits.max_pixels,
                crate::capture::OLLAMA_CONSERVATIVE_MAX_PIXELS
            );
        }
    }

    #[test]
    fn own_caps_matches_capabilities_for_the_configured_model() {
        let provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        assert_eq!(provider.own_caps(), provider.capabilities("gemma3:4b"));
    }

    // -- live capabilities (#1268, #1266) ---------------------------------

    /// A one-shot fake Ollama on loopback that answers one request with
    /// `body` as JSON. Returns the base URL (a fresh port, so the process
    /// cache key is unique per test).
    fn fake_show_server(status: &'static str, body: &'static str) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.ends_with(b"}") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
            }
        });
        format!("http://127.0.0.1:{port}")
    }

    type SeenRequests = std::sync::Arc<std::sync::Mutex<Vec<String>>>;

    /// A fake Ollama that answers each connection in turn with the next
    /// `(status, body)`, then stops listening. Returns the base URL and
    /// every request seen as `"<path> <json body>"`.
    fn fake_server(responses: Vec<(&'static str, &'static str)>) -> (String, SeenRequests) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().unwrap().port();
        let seen: SeenRequests = Default::default();
        let log = seen.clone();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut s, _)) = listener.accept() else {
                    return;
                };
                let mut req = Vec::new();
                let mut buf = [0u8; 1024];
                while !req.ends_with(b"}") {
                    match s.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => req.extend_from_slice(&buf[..n]),
                    }
                }
                let text = String::from_utf8_lossy(&req).to_string();
                let path = text
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                let json = text.split("\r\n\r\n").nth(1).unwrap_or_default();
                log.lock().unwrap().push(format!("{path} {json}"));
                let head = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                );
                let _ = s.write_all(head.as_bytes());
                let _ = s.write_all(body.as_bytes());
            }
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    #[test]
    fn a_failed_show_is_asked_once_across_resolve_and_complete() {
        let (base, seen) = fake_server(vec![("500 Internal Server Error", r#"{"error":"x"}"#)]);
        let provider = Ollama::new(&base, "gemma3:4b", "low");
        let started = std::time::Instant::now();
        assert!(provider.resolve_caps().vision, "name list still applies");
        let _ = provider.complete(&req("sys", false));
        let _ = provider.resolve_caps();
        let seen = seen.lock().unwrap();
        let shows = seen.iter().filter(|r| r.starts_with("/api/show")).count();
        assert_eq!(shows, 1, "seen: {seen:?}");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "no backoff sleeps: {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_show_without_capabilities_is_asked_once_across_two_presses() {
        let (base, seen) = fake_server(vec![("200 OK", "{}")]);
        let provider = Ollama::new(&base, "gemma3:4b", "high");
        assert!(provider.resolve_caps().vision, "old server: name list");
        assert!(provider.resolve_caps().vision);
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_400_does_not_support_thinking_is_resent_with_think_false_and_remembered() {
        let (base, seen) = fake_server(vec![
            ("200 OK", "{}"),
            (
                "400 Bad Request",
                r#"{"error":"\"gemma3:4b\" does not support thinking"}"#,
            ),
            (
                "200 OK",
                r#"{"message":{"role":"assistant","content":"ok"},"done_reason":"stop"}"#,
            ),
        ]);
        let provider = Ollama::new(&base, "gemma3:4b", "high");
        let done = provider.complete(&req("sys", false)).expect("resent");
        assert_eq!(done.text, "ok");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 3, "show, chat, chat again: {seen:?}");
        let think_of = |r: &str| -> Value {
            let (path, json) = r.split_once(' ').unwrap();
            assert_eq!(path, "/api/chat");
            serde_json::from_str::<Value>(json).unwrap()["think"].clone()
        };
        assert_eq!(think_of(&seen[1]), true);
        assert_eq!(think_of(&seen[2]), false);
        // Remembered: the next body skips the 400 altogether.
        assert_eq!(provider.build_body(&req("sys", false))["think"], false);
    }

    #[test]
    fn a_different_400_is_not_resent() {
        let (base, seen) = fake_server(vec![
            ("200 OK", "{}"),
            ("400 Bad Request", r#"{"error":"bad format"}"#),
            ("200 OK", "{}"),
        ]);
        let provider = Ollama::new(&base, "gemma3:4b", "high");
        assert!(provider.complete(&req("sys", false)).is_err());
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    /// Shaped like a real 0.34 `/api/show` answer (extra fields kept).
    const SHOW_VISION_ONLY: &str = r#"{"license":"x","modelfile":"FROM y","details":{"family":"gemma3","parameter_size":"4B"},"model_info":{"general.architecture":"gemma3"},"capabilities":["completion","vision"]}"#;

    fn caps_of(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn high_effort_on_a_model_without_thinking_sends_think_false() {
        let base = "http://127.0.0.1:1";
        cache_put(base, "gemma3:4b", caps_of(&["completion", "vision"]));
        let provider = Ollama::new(base, "gemma3:4b", "high");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["think"], false);
    }

    #[test]
    fn high_effort_on_a_thinking_model_sends_think_true() {
        let base = "http://127.0.0.1:2";
        cache_put(
            base,
            "qwen3.5:4b",
            caps_of(&["completion", "vision", "thinking"]),
        );
        let provider = Ollama::new(base, "qwen3.5:4b", "high");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["think"], true);
        // Low effort on the same model is still an explicit false.
        let provider = Ollama::new(base, "qwen3.5:4b", "low");
        assert_eq!(provider.build_body(&req("sys", false))["think"], false);
    }

    #[test]
    fn vision_from_live_capabilities_beats_the_name_list() {
        let base = "http://127.0.0.1:3";
        cache_put(base, "llama3.2-vision", caps_of(&["completion", "vision"]));
        cache_put(base, "gemma3:1b", caps_of(&["completion"]));
        let a = Ollama::new(base, "llama3.2-vision", "low");
        let b = Ollama::new(base, "gemma3:1b", "low");
        assert!(a.own_caps().vision, "llama3.2-vision reports vision");
        assert!(!b.own_caps().vision, "gemma3:1b reports no vision");
        assert!(b.own_caps().json_schema);
    }

    #[test]
    fn an_unknown_model_falls_back_to_the_name_list_without_network() {
        let provider = Ollama::new("http://127.0.0.1:4", "gemma3:4b", "low");
        assert!(provider.own_caps().vision);
        assert!(!provider.own_caps().thinking);
    }

    #[test]
    fn resolve_caps_asks_show_once_and_caches_the_answer() {
        let base = fake_show_server("200 OK", SHOW_VISION_ONLY);
        let provider = Ollama::new(&base, "llama3.2-vision", "high");
        // The name list says no vision; the server says yes.
        assert!(provider.resolve_caps().vision);
        // The one-shot server is gone now, so these come from the cache.
        assert!(provider.resolve_caps().vision);
        assert_eq!(provider.build_body(&req("sys", false))["think"], false);
    }

    #[test]
    fn resolve_caps_on_a_failed_show_falls_back_to_the_name_list() {
        let base = fake_show_server("500 Internal Server Error", r#"{"error":"boom"}"#);
        let provider = Ollama::new(&base, "gemma3:4b", "low");
        assert!(provider.resolve_caps().vision);
        let provider = Ollama::new(&base, "llama3.2-vision", "low");
        assert!(!provider.resolve_caps().vision);
    }

    #[test]
    fn own_caps_never_calls_the_network() {
        // Nothing listens here, and the call returns at once.
        let provider = Ollama::new("http://127.0.0.1:9", "llama3.2-vision", "low");
        let _ = provider.own_caps();
        assert!(cache_get("http://127.0.0.1:9", "llama3.2-vision").is_none());
    }

    #[test]
    fn id_is_ollama() {
        assert_eq!(
            Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low").id(),
            "ollama"
        );
    }

    // -- live check (#13 Done-when) --------------------------------------
    //
    // Not run by default (`cargo test` / `cargo test provider` never touch
    // the network). Run explicitly: `cargo test ollama_live -- --ignored`.
    // Requires Ollama running locally with `gemma3:4b` pulled.
    #[test]
    #[ignore]
    fn ollama_live_answers_a_synthetic_arithmetic_screenshot() {
        use image::{ImageBuffer, Rgb};

        // A tiny synthetic image, not a real screenshot: a white canvas
        // with a black arithmetic line drawn as filled rectangles (no font
        // rendering dependency). Simple enough for a small vision model to
        // read reliably: "2 + 2 = " as blocks would be unreadable, so
        // instead this draws large black bars spelling out a single-digit
        // sum in a big, blocky 7-segment-style font is overkill for a
        // smoke test -- the point of this test is that the provider's
        // wire format round-trips through a real server, not OCR accuracy,
        // so the image is a plain high-contrast panel with an unambiguous
        // text label rendered via `image`'s basic primitives, and the
        // prompt below carries the actual question in words the model is
        // told to trust the same as any other on-screen text.
        let width = 400u32;
        let height = 200u32;
        let mut img = ImageBuffer::from_pixel(width, height, Rgb([255u8, 255, 255]));
        // A thick black bar as a stand-in "problem statement" marker so
        // the PNG is not a blank white square (some vision models refuse
        // or hallucinate on a fully empty image).
        for y in 80..120 {
            for x in 40..360 {
                img.put_pixel(x, y, Rgb([0, 0, 0]));
            }
        }
        let mut png = Vec::new();
        {
            use image::ImageEncoder;
            let encoder = image::codecs::png::PngEncoder::new(&mut png);
            encoder
                .write_image(&img, width, height, image::ExtendedColorType::Rgb8)
                .expect("encode synthetic PNG");
        }

        let mut provider = Ollama::new(DEFAULT_BASE_URL, "gemma3:4b", "low");
        // Unload the model after this one-off check, per #13's Done-when.
        provider.keep_alive = "0".to_string();

        let request = Request {
            system: "You are shown a small test image with a black bar on it. Reply with exactly two JSON fields: detail (a short sentence) and headline (at most ten words). This is a wiring smoke test, not a real problem -- just describe what you see.".to_string(),
            user: "What is 2 + 2? Answer as if this were the problem shown.".to_string(),
            images: vec![png],
            schema: Some(json!({
                "type": "object",
                "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                "required": ["detail", "headline"],
                "additionalProperties": false
            })),
            effort: Effort::Unset,
            max_tokens: 0,
        };

        let started = std::time::Instant::now();
        let completion = provider
            .complete(&request)
            .expect("live ollama request should succeed");
        let elapsed = started.elapsed();

        let answer = parse_answer(&completion.text).expect("response should parse as an Answer");
        assert!(!answer.headline.is_empty());
        // MEASURED result and latency are recorded in the commit message
        // and the #13 closing comment, not asserted here (the model's
        // exact wording is not a stable thing to assert on).
        eprintln!(
            "ollama_live: model=gemma3:4b elapsed={:?} headline={:?}",
            elapsed, answer.headline
        );
    }
}
