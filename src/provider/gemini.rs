//! Gemini provider (#17): `POST
//! https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent`.
//!
//! Sources, fetched 2026-09-17:
//! - Endpoint shape, `x-goog-api-key` header (never `?key=` in the URL --
//!   the URL ends up in ureq error strings and any future request log, and
//!   CLAUDE.md rule 1 treats a secret leaking into either the same as a
//!   leak into the repo): <https://ai.google.dev/gemini-api/docs/generate-content/text-generation>
//! - `inline_data`/`mime_type` Part shape, `candidates[].content.parts[].text`,
//!   `candidates[].finishReason`, `promptFeedback.blockReason`,
//!   `usageMetadata` fields: <https://ai.google.dev/api/generate-content>
//! - `responseJsonSchema` preserves the schema's own key order for Gemini
//!   2.5+ models, no `propertyOrdering` needed (that field/gotcha is only a
//!   property of the older, deprecated `responseSchema`, an OpenAPI-subset
//!   schema carried over a protobuf `Struct`, which does not preserve key
//!   order on its own): <https://blog.google/innovation-and-ai/technology/developers-tools/gemini-api-structured-outputs/>
//!   ("the API now preserves the same order as the ordering of keys in the
//!   schema"). This is what CLAUDE.md rule 3 needs -- see
//!   `response_json_schema_preserves_property_order` below for the actual
//!   proof (a serialized-string check, matching #156's note in
//!   `anthropic.rs`/`openai.rs`: `assert_eq!` on two `Value`s does not
//!   check key order under `preserve_order`).
//! - `thinkingConfig.thinkingLevel` (Gemini 3.x) vs `thinkingConfig.thinkingBudget`
//!   (Gemini 2.5.x, a different, integer-budget shape):
//!   <https://ai.google.dev/gemini-api/docs/generate-content/thinking>
//! - `finishReason` values `SAFETY`/`RECITATION`/`PROHIBITED_CONTENT` as
//!   refusal shapes, `promptFeedback.blockReason` when there are no
//!   candidates at all: <https://ai.google.dev/api/generate-content>,
//!   cross-checked against community reports of the full enum
//!   (`STOP`, `MAX_TOKENS`, `SAFETY`, `RECITATION`, `LANGUAGE`, `BLOCKLIST`,
//!   `PROHIBITED_CONTENT`, `SPII`, `MALFORMED`, `OTHER`) since Google's own
//!   reference page did not render a single exhaustive enum table for this
//!   fetch.
//!
//! `thinkingConfig.thinkingLevel` is only sent to the Gemini 3.x line (see
//! [`supports_thinking`]) -- THEORY (unverified): whether sending it to a
//! 2.5-series model 400s (like Anthropic's 4.5-generation `effort`,
//! MEASURED 2026-09-15) or is silently ignored has not been checked live,
//! and this task makes no live Gemini calls (#17's "Done when" -- a live
//! check is left for issue #166).

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::common;
use super::{Caps, Completion, Effort, Provider, Request, StopReason, Usage};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT_BASE: &str = "https://generativelanguage.googleapis.com/v1beta/models";
/// Used when `Request::max_tokens` is `0` (the caller has no opinion).
const DEFAULT_MAX_TOKENS: u32 = 4000;

pub struct Gemini {
    pub api_key: String,
    pub model: String,
    /// The configured default, used when `Request::effort` is `Effort::Unset`.
    pub effort: Effort,
}

impl Gemini {
    pub fn new(
        api_key: impl Into<String>,
        model: impl Into<String>,
        effort: impl Into<String>,
    ) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            effort: Effort::parse(&effort.into()),
        }
    }

    /// `{ENDPOINT_BASE}/{model}:generateContent`. Deliberately carries no
    /// query string: the API key goes in the `x-goog-api-key` header (see
    /// [`Gemini::complete`]), never in the URL, where it would leak into a
    /// ureq transport-error string or any future request log (CLAUDE.md
    /// rule 1).
    fn endpoint_url(&self) -> String {
        format!("{ENDPOINT_BASE}/{}:generateContent", self.model)
    }

    /// Builds the exact request body for `generateContent`. Pure and
    /// network-free so it can be unit tested directly, mirroring
    /// `Anthropic::build_body`/`OpenAi::build_body`.
    ///
    /// - Images are `inline_data` parts (`mime_type: "image/png"`, base64
    ///   `data`), placed before the text part -- see
    ///   `parts_place_images_before_text`.
    /// - The system prompt goes in `systemInstruction`, never folded into
    ///   `contents` (there is no separate "system" role on this API).
    /// - `generationConfig.responseMimeType` + `.responseJsonSchema` (never
    ///   the deprecated `responseSchema`) carry `req.schema` verbatim, so
    ///   its key order -- `detail` before `headline` before `difficulty`,
    ///   rule 3 -- reaches the wire unchanged (Gemini 2.5+ preserves it,
    ///   see the module doc).
    /// - `generationConfig.thinkingConfig.thinkingLevel` is set from
    ///   `effort` only for models `supports_thinking` allows and only when
    ///   `effort.as_str()` is `Some` -- an empty/unset effort is a
    ///   plausible hand-edit of config.toml and must never be sent as an
    ///   empty string, mirroring `#154`'s guard in the other two providers.
    fn build_body(&self, req: &Request) -> Value {
        let mut parts: Vec<Value> = common::encode_images_base64(&req.images)
            .into_iter()
            .map(|b64| json!({"inline_data": {"mime_type": "image/png", "data": b64}}))
            .collect();
        parts.push(json!({"text": req.user}));

        let mut generation_config = json!({});
        if let Some(schema) = &req.schema {
            generation_config["responseMimeType"] = json!("application/json");
            generation_config["responseJsonSchema"] = schema.clone();
        }

        let effort = if req.effort != Effort::Unset {
            req.effort
        } else {
            self.effort
        };
        if supports_thinking(&self.model) {
            if let Some(level) = effort.as_str() {
                generation_config["thinkingConfig"] = json!({"thinkingLevel": level});
            }
        }

        let max_tokens = if req.max_tokens > 0 {
            req.max_tokens
        } else {
            DEFAULT_MAX_TOKENS
        };
        generation_config["maxOutputTokens"] = json!(max_tokens);

        json!({
            "systemInstruction": {"parts": [{"text": req.system}]},
            "contents": [{"role": "user", "parts": parts}],
            "generationConfig": generation_config
        })
    }

    /// Parses a successful (2xx) `generateContent` body into a
    /// `Completion`. Returns a clear error for a refusal (`SAFETY`,
    /// `RECITATION`, `PROHIBITED_CONTENT`) or a prompt-level block
    /// (`promptFeedback.blockReason` with no candidates at all) rather than
    /// treating either as a malformed response.
    fn parse_completion(body: &str) -> Result<Completion> {
        let value: Value =
            serde_json::from_str(body).context("gemini: response body is not valid JSON")?;

        let candidates = value
            .get("candidates")
            .and_then(Value::as_array)
            .filter(|c| !c.is_empty());

        let candidates = match candidates {
            Some(c) => c,
            None => {
                // No candidates at all: the prompt itself was blocked
                // before any generation happened. `promptFeedback` names
                // why; without it this is a genuinely malformed response.
                if let Some(reason) = value
                    .get("promptFeedback")
                    .and_then(|f| f.get("blockReason"))
                    .and_then(Value::as_str)
                {
                    return Err(anyhow!("gemini: the prompt was blocked ({reason})"));
                }
                return Err(anyhow!("gemini: response has no candidates[]"));
            }
        };

        let candidate = &candidates[0];
        let finish_reason = candidate.get("finishReason").and_then(Value::as_str);

        if matches!(
            finish_reason,
            Some("SAFETY") | Some("RECITATION") | Some("PROHIBITED_CONTENT")
        ) {
            return Err(anyhow!(
                "gemini: the model declined to answer ({})",
                finish_reason.unwrap_or("unknown reason")
            ));
        }

        let mut text = String::new();
        if let Some(parts) = candidate
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for part in parts {
                if let Some(t) = part.get("text").and_then(Value::as_str) {
                    text.push_str(t);
                }
            }
        }

        if text.is_empty() {
            // `finishReason: "MAX_TOKENS"` with no text part means the
            // model spent its whole output-token budget before emitting an
            // answer -- a specific, reachable shape (#155's pattern in the
            // sibling providers), not a generic malformed response.
            if finish_reason == Some("MAX_TOKENS") {
                return Err(anyhow!(
                    "gemini: The model ran out of room before answering. Lower the effort setting or raise the token limit."
                ));
            }
            return Err(anyhow!("gemini: no text part found in candidate content"));
        }

        let stop = match finish_reason {
            Some("MAX_TOKENS") => StopReason::MaxTokens,
            Some("STOP") => StopReason::Complete,
            _ => StopReason::Other,
        };

        let usage = value.get("usageMetadata").map(|u| Usage {
            input_tokens: u
                .get("promptTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
            output_tokens: u
                .get("candidatesTokenCount")
                .and_then(Value::as_u64)
                .unwrap_or(0) as u32,
        });

        Ok(Completion { text, usage, stop })
    }
}

/// Whether a model accepts `generationConfig.thinkingConfig.thinkingLevel`.
///
/// The Gemini 3.x line takes `thinkingLevel` (`"low"`/`"medium"`/`"high"`/
/// `"minimal"`); the 2.5-series models think by default but are controlled
/// by the differently-shaped `thinkingConfig.thinkingBudget` (an integer
/// token budget) instead -- see the module doc. `thinkingLevel` is only
/// sent to the 3.x line here, mirroring `anthropic::supports_effort`'s
/// unsupported-model gate; `thinkingBudget` support is not implemented
/// (scope cut, not a bug -- #17 does not ask for it).
///
/// Also used as this provider's `thinking` capability.
fn supports_thinking(model: &str) -> bool {
    model.starts_with("gemini-3")
}

impl Provider for Gemini {
    fn id(&self) -> &'static str {
        "gemini"
    }

    fn ready(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn capabilities(&self, model: &str) -> Caps {
        Caps {
            vision: true,
            json_schema: true,
            thinking: supports_thinking(model),
        }
    }

    fn complete(&self, req: &Request) -> Result<Completion> {
        let body = self.build_body(req);
        let url = self.endpoint_url();

        // The key goes in a header, never the URL (see `endpoint_url`'s
        // doc and `endpoint_url_never_contains_the_api_key` below) --
        // CLAUDE.md rule 1: a key in the URL leaks into ureq's own
        // transport-error strings and any future request log.
        let body_text = common::post_json(
            &url,
            &[
                ("x-goog-api-key", self.api_key.as_str()),
                ("Content-Type", "application/json"),
            ],
            &body,
            REQUEST_TIMEOUT,
            "gemini",
        )?;

        Self::parse_completion(&body_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{parse_answer, physics_request, Difficulty, Shot};
    use std::fs;

    fn sample_shot() -> Shot {
        Shot {
            png: vec![0x89, 0x50, 0x4E, 0x47],
            width: 100,
            height: 200,
        }
    }

    fn req(prompt: &str, want_difficulty: bool) -> Request {
        physics_request(&sample_shot(), prompt, want_difficulty)
    }

    // -- endpoint_url: the key must never reach the URL --------------------

    #[test]
    fn endpoint_url_matches_the_generate_content_shape() {
        let p = Gemini::new("AIza-test", "gemini-3.8-flash", "low");
        assert_eq!(
            p.endpoint_url(),
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.8-flash:generateContent"
        );
    }

    #[test]
    fn endpoint_url_never_contains_the_api_key() {
        let p = Gemini::new("AIza-super-secret-value", "gemini-3.8-flash", "low");
        let url = p.endpoint_url();
        assert!(!url.contains("AIza-super-secret-value"), "{url}");
        assert!(!url.contains("key="), "{url}");
    }

    // -- build_body: shape -------------------------------------------------

    #[test]
    fn build_body_matches_the_documented_shape() {
        let provider = Gemini::new("AIza-test", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("system prompt text", false));

        let expected_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &sample_shot().png,
        );

        let expected = json!({
            "systemInstruction": {"parts": [{"text": "system prompt text"}]},
            "contents": [{"role": "user", "parts": [
                {"inline_data": {"mime_type": "image/png", "data": expected_b64}},
                {"text": "Check my working."}
            ]}],
            "generationConfig": {
                "responseMimeType": "application/json",
                "responseJsonSchema": {"type": "object",
                    "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                    "required": ["detail", "headline"], "additionalProperties": false},
                "thinkingConfig": {"thinkingLevel": "low"},
                "maxOutputTokens": 4000
            }
        });

        assert_eq!(body, expected);

        // Guard against the deprecated/wrong spellings.
        assert!(body["generationConfig"].get("responseSchema").is_none());
        assert!(body["generationConfig"]["responseJsonSchema"]
            .get("propertyOrdering")
            .is_none());
    }

    #[test]
    fn parts_place_images_before_text() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("sys", false));
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        assert!(
            parts[0].get("inline_data").is_some(),
            "image part must come first: {parts:?}"
        );
        assert_eq!(parts.last().unwrap()["text"], "Check my working.");
    }

    #[test]
    fn multiple_images_all_become_inline_data_parts_in_order() {
        let mut r = req("sys", false);
        r.images = vec![vec![1, 2, 3], vec![4, 5, 6]];
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let body = provider.build_body(&r);
        let parts = body["contents"][0]["parts"].as_array().unwrap();
        // Two images + one text part.
        assert_eq!(parts.len(), 3);
        assert!(parts[0].get("inline_data").is_some());
        assert!(parts[1].get("inline_data").is_some());
        assert!(parts[2].get("text").is_some());
        assert_ne!(
            parts[0]["inline_data"]["data"],
            parts[1]["inline_data"]["data"]
        );
    }

    #[test]
    fn build_body_adds_difficulty_property_when_requested() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("system prompt text", true));

        let schema = &body["generationConfig"]["responseJsonSchema"];
        assert_eq!(
            schema["properties"]["difficulty"],
            json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U","N"]})
        );
        assert_eq!(
            schema["required"],
            json!(["detail", "headline", "difficulty"])
        );

        let system = body["systemInstruction"]["parts"][0]["text"]
            .as_str()
            .unwrap();
        assert!(system.starts_with("system prompt text"));
        assert!(system.contains("difficulty"));
    }

    #[test]
    fn build_body_is_unchanged_when_difficulty_off() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let with_flag = provider.build_body(&req("system prompt text", false));
        assert_eq!(
            with_flag["systemInstruction"]["parts"][0]["text"],
            "system prompt text"
        );
        assert!(
            with_flag["generationConfig"]["responseJsonSchema"]["properties"]
                .get("difficulty")
                .is_none()
        );
    }

    #[test]
    fn no_schema_request_omits_response_schema_fields() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let mut r = req("sys", false);
        r.schema = None;
        let body = provider.build_body(&r);
        assert!(body["generationConfig"].get("responseMimeType").is_none());
        assert!(body["generationConfig"].get("responseJsonSchema").is_none());
    }

    // -- rule 3: schema property order is load-bearing ----------------------
    //
    // #156's note (see the identical comment in anthropic.rs/openai.rs):
    // `assert_eq!` on two `Value`s does NOT check key order under
    // `preserve_order` -- `Value::Object` is backed by an `IndexMap` and its
    // `PartialEq` compares as a set. Only a *serialized string* check proves
    // the order the model actually receives.
    #[test]
    fn response_json_schema_preserves_property_order() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("sys", true));
        let schema_str =
            serde_json::to_string(&body["generationConfig"]["responseJsonSchema"]).unwrap();

        let detail_pos = schema_str.find("\"detail\"").expect("detail present");
        let headline_pos = schema_str.find("\"headline\"").expect("headline present");
        let difficulty_pos = schema_str
            .find("\"difficulty\"")
            .expect("difficulty present");
        assert!(detail_pos < headline_pos, "{schema_str}");
        assert!(headline_pos < difficulty_pos, "{schema_str}");
    }

    #[test]
    fn top_level_and_generation_config_key_order_is_stable() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("sys", false));
        let body_str = serde_json::to_string(&body).unwrap();

        let system_pos = body_str.find("\"systemInstruction\"").unwrap();
        let contents_pos = body_str.find("\"contents\"").unwrap();
        let config_pos = body_str.find("\"generationConfig\"").unwrap();
        assert!(system_pos < contents_pos, "{body_str}");
        assert!(contents_pos < config_pos, "{body_str}");

        let mime_pos = body_str.find("\"responseMimeType\"").unwrap();
        let schema_pos = body_str.find("\"responseJsonSchema\"").unwrap();
        let max_pos = body_str.find("\"maxOutputTokens\"").unwrap();
        assert!(mime_pos < schema_pos, "{body_str}");
        assert!(schema_pos < max_pos, "{body_str}");
    }

    // A byte-identical golden, captured from this implementation's own
    // output (there is no prior Gemini provider to diff against) --
    // regression protection for the exact wire shape, same role as the
    // golden tests in openai.rs/anthropic.rs.
    #[test]
    fn golden_request_body_no_difficulty_is_byte_identical() {
        let provider = Gemini::new("AIza-test", "gemini-3.8-flash", "low");
        let body = provider.build_body(&req("system prompt text", false));
        let golden = r#"{"systemInstruction":{"parts":[{"text":"system prompt text"}]},"contents":[{"role":"user","parts":[{"inline_data":{"mime_type":"image/png","data":"iVBORw=="}},{"text":"Check my working."}]}],"generationConfig":{"responseMimeType":"application/json","responseJsonSchema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false},"thinkingConfig":{"thinkingLevel":"low"},"maxOutputTokens":4000}}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
    }

    // -- effort / thinkingConfig --------------------------------------------

    #[test]
    fn empty_effort_is_never_sent() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "");
        let body = provider.build_body(&req("sys", false));
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn whitespace_only_effort_is_never_sent() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "   ");
        let body = provider.build_body(&req("sys", false));
        assert!(body["generationConfig"].get("thinkingConfig").is_none());
    }

    #[test]
    fn non_empty_effort_is_still_sent_for_a_3x_model() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "high");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn thinking_config_is_omitted_for_a_2_5_series_model() {
        let provider = Gemini::new("k", "gemini-2.5-pro", "high");
        let body = provider.build_body(&req("sys", false));
        assert!(
            body["generationConfig"].get("thinkingConfig").is_none(),
            "gemini-2.5-pro must not carry thinkingLevel (see supports_thinking's doc)"
        );
    }

    #[test]
    fn request_effort_override_takes_precedence_over_configured_default() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let mut r = req("sys", false);
        r.effort = Effort::High;
        let body = provider.build_body(&r);
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    // -- ready / capabilities ------------------------------------------------

    #[test]
    fn ready_reflects_api_key_presence() {
        assert!(Gemini::new("AIza-real", "gemini-3.8-flash", "low").ready());
        assert!(!Gemini::new("", "gemini-3.8-flash", "low").ready());
        assert!(!Gemini::new("   ", "gemini-3.8-flash", "low").ready());
    }

    #[test]
    fn capabilities_report_vision_and_json_schema_always_and_thinking_per_model() {
        let provider = Gemini::new("k", "gemini-3.8-flash", "low");
        let caps = provider.capabilities("gemini-3.8-flash");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(caps.thinking);

        let caps = provider.capabilities("gemini-2.5-pro");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(!caps.thinking);
    }

    // -- parse_completion: success -------------------------------------------

    #[test]
    fn parse_completion_extracts_text_and_usage_from_the_fixture() {
        let body = fs::read_to_string("tests/fixtures/gemini_response.json")
            .expect("fixture file should exist");
        let completion = Gemini::parse_completion(&body).expect("should parse");
        let answer = parse_answer(&completion.text).expect("should parse as an Answer");
        assert_eq!(answer.headline, "42 m/s is correct");
        assert_eq!(
            answer.detail,
            "v = u + at = 0 + 9.8*4.3 = 42.1, rounds to 42."
        );
        assert_eq!(answer.difficulty, None);
        assert_eq!(completion.stop, StopReason::Complete);
        let usage = completion.usage.expect("usageMetadata present in fixture");
        assert_eq!(usage.input_tokens, 1234);
        assert_eq!(usage.output_tokens, 56);
    }

    #[test]
    fn parse_completion_parses_a_valid_difficulty() {
        let body = r#"{"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"7\"}"}
        ]}}]}"#;
        let completion = Gemini::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.difficulty, Some(Difficulty::Level(7)));
    }

    #[test]
    fn parse_completion_degrades_unparseable_difficulty_to_none_without_erroring() {
        let body = r#"{"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"way too hard\"}"}
        ]}}]}"#;
        let completion = Gemini::parse_completion(body).expect("should still parse");
        let answer = parse_answer(&completion.text).expect("should still parse the answer");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_missing_difficulty_key_is_none() {
        let body = r#"{"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\"}"}
        ]}}]}"#;
        let completion = Gemini::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_rejects_invalid_json() {
        let err = Gemini::parse_completion("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn parse_completion_concatenates_multiple_text_parts() {
        let body = r#"{"candidates": [{"finishReason": "STOP", "content": {"parts": [
            {"text": "{\"detail\": \"d\", "},
            {"text": "\"headline\": \"h\"}"}
        ]}}]}"#;
        let completion = Gemini::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.detail, "d");
        assert_eq!(answer.headline, "h");
    }

    // -- parse_completion: MAX_TOKENS (#155 pattern) -------------------------

    #[test]
    fn parse_completion_reports_budget_exhaustion_for_max_tokens_with_no_text() {
        let body = fs::read_to_string("tests/fixtures/gemini_response_max_tokens.json")
            .expect("fixture file should exist");
        let err = Gemini::parse_completion(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
        assert!(!msg.contains("no text part found"), "{msg}");
    }

    /// Neighbour: a missing text part with a finishReason other than
    /// `MAX_TOKENS` is genuinely malformed, not budget exhaustion.
    #[test]
    fn parse_completion_rejects_missing_text_when_not_max_tokens() {
        let body = r#"{"candidates": [{"finishReason": "OTHER", "content": {"parts": []}}]}"#;
        let err = Gemini::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no text part found"));
    }

    // -- parse_completion: refusal finishReasons -----------------------------

    #[test]
    fn parse_completion_rejects_safety_finish_reason() {
        let body = fs::read_to_string("tests/fixtures/gemini_response_safety.json")
            .expect("fixture file should exist");
        let err = Gemini::parse_completion(&body).unwrap_err();
        assert!(err.to_string().contains("declined to answer"), "{err}");
        assert!(err.to_string().contains("SAFETY"), "{err}");
    }

    #[test]
    fn parse_completion_rejects_recitation_finish_reason() {
        let body = r#"{"candidates": [{"finishReason": "RECITATION", "content": {"parts": []}}]}"#;
        let err = Gemini::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("declined to answer"), "{err}");
        assert!(err.to_string().contains("RECITATION"), "{err}");
    }

    #[test]
    fn parse_completion_rejects_prohibited_content_finish_reason() {
        let body =
            r#"{"candidates": [{"finishReason": "PROHIBITED_CONTENT", "content": {"parts": []}}]}"#;
        let err = Gemini::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("declined to answer"), "{err}");
        assert!(err.to_string().contains("PROHIBITED_CONTENT"), "{err}");
    }

    // -- parse_completion: prompt-level block (no candidates at all) --------

    #[test]
    fn parse_completion_reports_prompt_feedback_block_reason_when_no_candidates() {
        let body = fs::read_to_string("tests/fixtures/gemini_response_prompt_blocked.json")
            .expect("fixture file should exist");
        let err = Gemini::parse_completion(&body).unwrap_err();
        assert!(err.to_string().contains("prompt was blocked"), "{err}");
        assert!(err.to_string().contains("SAFETY"), "{err}");
    }

    #[test]
    fn parse_completion_rejects_a_response_with_no_candidates_and_no_prompt_feedback() {
        let body = r#"{}"#;
        let err = Gemini::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no candidates"), "{err}");
    }

    #[test]
    fn parse_completion_treats_an_empty_candidates_array_like_no_candidates() {
        let body = r#"{"candidates": [], "promptFeedback": {"blockReason": "OTHER"}}"#;
        let err = Gemini::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("prompt was blocked"), "{err}");
    }
}
