//! Issue #16: a generic OpenAI-compatible `/chat/completions` endpoint.
//! Covers any server that speaks the OpenAI Chat Completions wire shape --
//! OpenRouter, Groq, Mistral, DeepSeek, xAI, Together, LM Studio,
//! llama.cpp, vLLM, Azure OpenAI, and a local Ollama server's own `/v1`
//! surface. One `[[providers.compat]]` config entry (`config.rs`) per
//! endpoint; this file only builds/parses the wire shape and never reads
//! config directly (same separation as `openai.rs`/`anthropic.rs`).
//!
//! Local-vs-cloud classification for mode selection is NOT decided here or
//! by this provider's name -- see `mode::is_local_provider`'s doc and
//! `Providers::build_chain_for_mode` (config.rs), which classify by the
//! configured `base_url`'s host via `mode::classify_host`.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::common;
use super::{Caps, Completion, Provider, Request, StopReason, Usage};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
/// Used when `Request::max_tokens` is `0` (the caller has no opinion).
const DEFAULT_MAX_TOKENS: u32 = 2500;

/// How a compat endpoint authenticates. TOML strings (see `config.rs`):
/// `"bearer"`, `"api-key-header"`, `"none"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CompatAuth {
    /// `Authorization: Bearer <api_key>`.
    Bearer,
    /// A caller-named header (`CompatConfig::auth_header`) carrying the raw
    /// `api_key` (e.g. Mistral's `X-Api-Key`).
    ApiKeyHeader,
    /// No credential sent at all -- a local server with nothing to
    /// authenticate with (LM Studio, llama.cpp, vLLM with no key set).
    #[default]
    None,
}

/// How a compat endpoint is asked for structured JSON. TOML strings:
/// `"json_schema"`, `"json_object"`, `"prompt"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Structured {
    /// `response_format: {"type": "json_schema", "json_schema": {...}}`,
    /// schema-enforced by the server.
    JsonSchema,
    /// `response_format: {"type": "json_object"}` -- valid JSON is
    /// guaranteed, the specific shape is not, so the schema is also
    /// described in the system prompt (#99's repair pass covers the rest).
    JsonObject,
    /// No `response_format` field at all -- the schema is described in the
    /// system prompt only. The safest default: every OpenAI-compatible
    /// server accepts a request with no `response_format`, not every one
    /// recognizes the field, and an unrecognized field is a 400 on some
    /// stricter servers.
    #[default]
    Prompt,
}

pub struct OpenAiCompat {
    pub base_url: String,
    pub model: String,
    pub auth: CompatAuth,
    pub auth_header: String,
    pub api_key: String,
    pub structured: Structured,
}

impl OpenAiCompat {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        base_url: impl Into<String>,
        model: impl Into<String>,
        auth: CompatAuth,
        auth_header: impl Into<String>,
        api_key: impl Into<String>,
        structured: Structured,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            model: model.into(),
            auth,
            auth_header: auth_header.into(),
            api_key: api_key.into(),
            structured,
        }
    }

    fn endpoint(&self) -> String {
        format!("{}/chat/completions", self.base_url.trim_end_matches('/'))
    }

    /// Appended to the system prompt whenever `structured` does not itself
    /// force the shape (`JsonObject`, `Prompt`) -- describes the required
    /// JSON Schema in words instead. Never used for `JsonSchema`, where the
    /// server enforces it and repeating it in prose would only cost tokens.
    fn schema_instruction(schema: &Value) -> String {
        format!(
            "\n\nRespond with a single JSON object only, matching exactly this JSON Schema, and nothing else (no markdown, no code fences, no explanation):\n{schema}"
        )
    }

    /// Builds the exact `/chat/completions` request body. Pure and
    /// network-free so it can be unit tested directly.
    ///
    /// - `image_url` parts carry a `data:image/png;base64,` URL (#16), the
    ///   OpenAI Chat Completions convention -- distinct from both OpenAI's
    ///   own Responses API (`input_image`) and Ollama's plain base64.
    /// - The schema, system text and image bytes all come from `req`: this
    ///   provider never builds the physics-check schema itself (#12).
    /// - `serde_json` keeps `preserve_order` (CLAUDE.md rule 3), so a
    ///   schema handed to `response_format.json_schema.schema` here keeps
    ///   the property order the caller built it with.
    fn build_body(&self, req: &Request) -> Value {
        let mut content = vec![json!({"type": "text", "text": req.user})];
        for b64 in common::encode_images_base64(&req.images) {
            content.push(json!({
                "type": "image_url",
                "image_url": {"url": format!("data:image/png;base64,{b64}")}
            }));
        }

        let mut system_text = req.system.clone();
        if let Some(schema) = &req.schema {
            if !matches!(self.structured, Structured::JsonSchema) {
                system_text.push_str(&Self::schema_instruction(schema));
            }
        }

        let max_tokens = if req.max_tokens > 0 {
            req.max_tokens
        } else {
            DEFAULT_MAX_TOKENS
        };

        let mut body = json!({
            "model": self.model,
            "messages": [
                {"role": "system", "content": system_text},
                {"role": "user", "content": content}
            ],
            "max_tokens": max_tokens,
        });

        if let Some(schema) = &req.schema {
            match self.structured {
                Structured::JsonSchema => {
                    body["response_format"] = json!({
                        "type": "json_schema",
                        "json_schema": {"name": "answer", "schema": schema, "strict": true}
                    });
                }
                Structured::JsonObject => {
                    body["response_format"] = json!({"type": "json_object"});
                }
                Structured::Prompt => {}
            }
        }

        body
    }

    /// Parses a successful (2xx) `/chat/completions` body into a
    /// `Completion`. The text is returned as-is -- this provider never
    /// interprets it against a schema (#12).
    fn parse_completion(body: &str) -> Result<Completion> {
        let value: Value =
            serde_json::from_str(body).context("openai-compat: response body is not valid JSON")?;

        let choice = value
            .get("choices")
            .and_then(Value::as_array)
            .and_then(|a| a.first())
            .ok_or_else(|| anyhow!("openai-compat: response has no choices[]"))?;

        let message = choice.get("message");

        // Mirrors Anthropic's `stop_reason: "refusal"` handling: a refusal
        // must never reach the repair pass (#99) or be treated as
        // schema-invalid output.
        if let Some(refusal) = message
            .and_then(|m| m.get("refusal"))
            .and_then(Value::as_str)
        {
            if !refusal.is_empty() {
                return Err(anyhow!("openai-compat: model refused to answer"));
            }
        }

        let finish_reason = choice.get("finish_reason").and_then(Value::as_str);

        // `finish_reason: "length"` means the token budget ran out before
        // the model finished -- the shared "ran out of room" message,
        // mirrored from `openai.rs`/`anthropic.rs`/`ollama.rs`'s identical
        // handling (#155-style), checked before the content is read so a
        // truncated, invalid-JSON body is reported as budget exhaustion
        // rather than a generic parse failure.
        if finish_reason == Some("length") {
            return Err(anyhow!(
                "openai-compat: The model ran out of room before answering. Lower the effort setting or raise the token limit."
            ));
        }

        let text = message
            .and_then(|m| m.get("content"))
            .and_then(Value::as_str);
        let text = match text {
            Some(t) if !t.is_empty() => t,
            _ => return Err(anyhow!("openai-compat: response has no message.content")),
        };

        let stop = match finish_reason {
            Some("stop") => StopReason::Complete,
            Some("length") => StopReason::MaxTokens, // unreachable: handled above
            _ => StopReason::Other,
        };

        let usage = match (
            value
                .get("usage")
                .and_then(|u| u.get("prompt_tokens"))
                .and_then(Value::as_u64),
            value
                .get("usage")
                .and_then(|u| u.get("completion_tokens"))
                .and_then(Value::as_u64),
        ) {
            (Some(input), Some(output)) => Some(Usage {
                input_tokens: input as u32,
                output_tokens: output as u32,
            }),
            _ => None,
        };

        Ok(Completion {
            text: text.to_string(),
            usage,
            stop,
        })
    }
}

impl Provider for OpenAiCompat {
    fn id(&self) -> &'static str {
        "openai-compat"
    }

    fn ready(&self) -> bool {
        if self.base_url.trim().is_empty() {
            return false;
        }
        match self.auth {
            CompatAuth::None => true,
            CompatAuth::Bearer | CompatAuth::ApiKeyHeader => !self.api_key.trim().is_empty(),
        }
    }

    /// No live discovery (mirrors Ollama pre-#14, Gemini, OpenAI). `vision`
    /// and `json_schema` are reported from what the endpoint is configured
    /// to accept, not probed -- a compat endpoint the user pointed at a
    /// text-only model, or one configured `structured = "prompt"`, is not
    /// distinguishable from here.
    ///
    /// `image_limits` (issue #169) is deliberately `None`, unlike the other
    /// four providers: this covers ANY OpenAI-compatible endpoint
    /// (OpenRouter, Groq, Mistral, DeepSeek, xAI, LM Studio, llama.cpp,
    /// vLLM, Azure, ...), each potentially proxying a different model with
    /// a different real limit, so there is no single conservative default
    /// that is more honest than "unknown" here. A caller sees `None` and
    /// falls back to the user's own `config.capture.max_edge` heuristic
    /// (`capture::resolve_limits`), which is exactly the pre-#169 behaviour
    /// for this provider.
    fn capabilities(&self, _model: &str) -> Caps {
        Caps {
            vision: true,
            json_schema: matches!(self.structured, Structured::JsonSchema),
            thinking: false,
            image_limits: None,
        }
    }

    fn own_caps(&self) -> Caps {
        self.capabilities(&self.model)
    }

    fn complete(&self, req: &Request) -> Result<Completion> {
        let body = self.build_body(req);

        // Built as owned `String`s first so the borrow handed to
        // `common::post_json` (which wants `&[(&str, &str)]`) outlives the
        // call -- mirrors the `let auth = format!(...)` pattern in
        // `openai.rs`.
        let auth_header: Option<(String, String)> = match self.auth {
            CompatAuth::Bearer => Some((
                "Authorization".to_string(),
                format!("Bearer {}", self.api_key),
            )),
            CompatAuth::ApiKeyHeader => Some((self.auth_header.clone(), self.api_key.clone())),
            CompatAuth::None => None,
        };

        let mut headers: Vec<(&str, &str)> = vec![("Content-Type", "application/json")];
        if let Some((name, value)) = &auth_header {
            headers.push((name.as_str(), value.as_str()));
        }

        // The key never goes in the URL (#16's test list): `endpoint()` is
        // built from `base_url` alone, and the credential is carried only
        // as a header above.
        let body_text = common::post_json(
            &self.endpoint(),
            &headers,
            &body,
            REQUEST_TIMEOUT,
            "openai-compat",
        )?;

        Self::parse_completion(&body_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{parse_answer, physics_request, Effort, Shot};
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

    fn provider(auth: CompatAuth, structured: Structured) -> OpenAiCompat {
        OpenAiCompat::new(
            "https://example.invalid/v1",
            "some-model",
            auth,
            "X-Api-Key",
            "sk-test-key",
            structured,
        )
    }

    // -- endpoint ---------------------------------------------------------

    #[test]
    fn endpoint_appends_chat_completions_and_strips_a_trailing_slash() {
        let p = OpenAiCompat::new(
            "https://example.invalid/v1/",
            "m",
            CompatAuth::None,
            "",
            "",
            Structured::Prompt,
        );
        assert_eq!(p.endpoint(), "https://example.invalid/v1/chat/completions");
    }

    /// #16's test list: the key must never appear in the URL, only in a
    /// header. True for every auth mode, including the two that carry a key
    /// at all.
    #[test]
    fn the_api_key_never_appears_in_the_endpoint_url() {
        for auth in [
            CompatAuth::Bearer,
            CompatAuth::ApiKeyHeader,
            CompatAuth::None,
        ] {
            let p = provider(auth, Structured::Prompt);
            assert!(
                !p.endpoint().contains("sk-test-key"),
                "auth {auth:?} leaked the key into the URL"
            );
        }
    }

    // -- build_body: shared shape ------------------------------------------

    #[test]
    fn build_body_carries_model_system_and_user_text() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let body = p.build_body(&req("system prompt text", false));
        assert_eq!(body["model"], "some-model");
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "system prompt text");
        assert_eq!(body["messages"][1]["role"], "user");
        assert_eq!(body["messages"][1]["content"][0]["type"], "text");
        assert_eq!(
            body["messages"][1]["content"][0]["text"],
            "Check my working."
        );
    }

    #[test]
    fn build_body_carries_image_url_with_a_data_url() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let body = p.build_body(&req("sys", false));
        let expected_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &sample_shot().png,
        );
        let image = &body["messages"][1]["content"][1];
        assert_eq!(image["type"], "image_url");
        assert_eq!(
            image["image_url"]["url"],
            format!("data:image/png;base64,{expected_b64}")
        );
    }

    #[test]
    fn build_body_uses_default_max_tokens_when_request_has_none() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let body = p.build_body(&req("sys", false));
        assert_eq!(body["max_tokens"], DEFAULT_MAX_TOKENS);
    }

    #[test]
    fn build_body_respects_an_explicit_max_tokens() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let mut r = req("sys", false);
        r.max_tokens = 77;
        let body = p.build_body(&r);
        assert_eq!(body["max_tokens"], 77);
    }

    #[test]
    fn build_body_omits_response_format_when_request_has_no_schema() {
        for structured in [
            Structured::JsonSchema,
            Structured::JsonObject,
            Structured::Prompt,
        ] {
            let p = provider(CompatAuth::Bearer, structured);
            let mut r = req("sys", false);
            r.schema = None;
            let body = p.build_body(&r);
            assert!(
                body.get("response_format").is_none(),
                "structured {structured:?}"
            );
            // No schema instruction appended either, with nothing to describe.
            assert_eq!(body["messages"][0]["content"], "sys");
        }
    }

    // -- build_body: per structured mode (#16's per-mode goldens) -----------

    #[test]
    fn json_schema_mode_sends_response_format_and_no_prompt_instruction() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let body = p.build_body(&req("system prompt text", false));

        assert_eq!(body["response_format"]["type"], "json_schema");
        assert_eq!(body["response_format"]["json_schema"]["name"], "answer");
        assert_eq!(body["response_format"]["json_schema"]["strict"], true);
        let schema = &body["response_format"]["json_schema"]["schema"];
        assert_eq!(schema["required"], json!(["detail", "headline"]));
        // No textual schema instruction appended -- the server enforces it.
        assert_eq!(body["messages"][0]["content"], "system prompt text");
    }

    #[test]
    fn json_object_mode_sends_response_format_and_a_prompt_instruction() {
        let p = provider(CompatAuth::Bearer, Structured::JsonObject);
        let body = p.build_body(&req("system prompt text", false));

        assert_eq!(body["response_format"], json!({"type": "json_object"}));
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("system prompt text"));
        assert!(system.contains("JSON Schema"));
        assert!(system.contains("\"detail\""));
    }

    #[test]
    fn prompt_mode_sends_no_response_format_but_still_a_prompt_instruction() {
        let p = provider(CompatAuth::Bearer, Structured::Prompt);
        let body = p.build_body(&req("system prompt text", false));

        assert!(body.get("response_format").is_none());
        let system = body["messages"][0]["content"].as_str().unwrap();
        assert!(system.starts_with("system prompt text"));
        assert!(system.contains("JSON Schema"));
    }

    /// CLAUDE.md rule 3: `detail` must stay before `headline` in the schema
    /// handed to `response_format.json_schema.schema`, unchanged from what
    /// `physics_request` built.
    #[test]
    fn json_schema_mode_preserves_the_schema_property_order() {
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let body = p.build_body(&req("sys", false));
        let schema_str =
            serde_json::to_string(&body["response_format"]["json_schema"]["schema"]).unwrap();
        let detail_pos = schema_str.find("\"detail\"").unwrap();
        let headline_pos = schema_str.find("\"headline\"").unwrap();
        assert!(
            detail_pos < headline_pos,
            "detail must precede headline: {schema_str}"
        );
    }

    // -- build_body: per auth mode (#16's per-auth goldens; headers, not
    // body -- exercised through `complete`'s header construction, but the
    // body itself must not vary by auth mode) ------------------------------

    #[test]
    fn build_body_does_not_vary_by_auth_mode() {
        let bearer =
            provider(CompatAuth::Bearer, Structured::JsonSchema).build_body(&req("sys", false));
        let header = provider(CompatAuth::ApiKeyHeader, Structured::JsonSchema)
            .build_body(&req("sys", false));
        let none =
            provider(CompatAuth::None, Structured::JsonSchema).build_body(&req("sys", false));
        assert_eq!(bearer, header);
        assert_eq!(header, none);
    }

    // -- ready --------------------------------------------------------------

    #[test]
    fn ready_requires_a_key_for_bearer_and_api_key_header_but_not_none() {
        assert!(provider(CompatAuth::Bearer, Structured::Prompt).ready());
        assert!(provider(CompatAuth::ApiKeyHeader, Structured::Prompt).ready());
        assert!(provider(CompatAuth::None, Structured::Prompt).ready());

        let no_key_bearer = OpenAiCompat::new(
            "https://example.invalid",
            "m",
            CompatAuth::Bearer,
            "",
            "",
            Structured::Prompt,
        );
        assert!(!no_key_bearer.ready());
        let no_key_header = OpenAiCompat::new(
            "https://example.invalid",
            "m",
            CompatAuth::ApiKeyHeader,
            "X-Api-Key",
            "",
            Structured::Prompt,
        );
        assert!(!no_key_header.ready());
        let no_key_none = OpenAiCompat::new(
            "https://example.invalid",
            "m",
            CompatAuth::None,
            "",
            "",
            Structured::Prompt,
        );
        assert!(no_key_none.ready());
    }

    #[test]
    fn ready_is_false_for_an_empty_base_url_regardless_of_auth() {
        assert!(!OpenAiCompat::new("", "m", CompatAuth::None, "", "", Structured::Prompt).ready());
        assert!(!OpenAiCompat::new(
            "   ",
            "m",
            CompatAuth::Bearer,
            "",
            "sk-x",
            Structured::Prompt
        )
        .ready());
    }

    // -- capabilities ---------------------------------------------------

    #[test]
    fn capabilities_reports_json_schema_only_for_json_schema_mode() {
        assert!(
            provider(CompatAuth::None, Structured::JsonSchema)
                .capabilities("m")
                .json_schema
        );
        assert!(
            !provider(CompatAuth::None, Structured::JsonObject)
                .capabilities("m")
                .json_schema
        );
        assert!(
            !provider(CompatAuth::None, Structured::Prompt)
                .capabilities("m")
                .json_schema
        );
    }

    // -- image_limits (issue #169) ---------------------------------------

    #[test]
    fn capabilities_reports_no_image_limits_unknown_backend() {
        // Deliberately `None`, not a conservative default: this provider
        // covers any OpenAI-compatible endpoint, each potentially proxying
        // a different model with a different real limit (see this file's
        // `capabilities()` doc comment).
        assert_eq!(
            provider(CompatAuth::None, Structured::Prompt)
                .capabilities("m")
                .image_limits,
            None
        );
    }

    #[test]
    fn own_caps_matches_capabilities_for_the_configured_model() {
        let p = provider(CompatAuth::None, Structured::Prompt);
        assert_eq!(p.own_caps(), p.capabilities("m"));
    }

    #[test]
    fn id_is_openai_compat() {
        assert_eq!(
            provider(CompatAuth::None, Structured::Prompt).id(),
            "openai-compat"
        );
    }

    // -- parse_completion (recorded fixture + inline bodies) ---------------

    #[test]
    fn parse_completion_round_trips_a_recorded_fixture() {
        let body = fs::read_to_string("tests/fixtures/openai_compat_response.json")
            .expect("fixture file should exist");
        let completion = OpenAiCompat::parse_completion(&body).expect("should parse");
        let answer = parse_answer(&completion.text).expect("should parse as an Answer");
        assert_eq!(answer.headline, "12 is correct");
        assert_eq!(completion.stop, StopReason::Complete);
        let usage = completion.usage.expect("fixture carries token counts");
        assert_eq!(usage.input_tokens, 120);
        assert_eq!(usage.output_tokens, 40);
    }

    #[test]
    fn parse_completion_rejects_invalid_json() {
        let err = OpenAiCompat::parse_completion("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn parse_completion_rejects_missing_choices() {
        let err = OpenAiCompat::parse_completion(r#"{"id": "x"}"#).unwrap_err();
        assert!(err.to_string().contains("no choices"));
    }

    #[test]
    fn parse_completion_reports_budget_exhaustion_for_length_finish_reason() {
        let body = r#"{"choices": [{"finish_reason": "length", "message": {"role": "assistant", "content": "{\n \"det"}}]}"#;
        let err = OpenAiCompat::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("ran out of room"));
    }

    #[test]
    fn parse_completion_rejects_a_refusal() {
        let body = r#"{"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": null, "refusal": "I can't help with that."}}]}"#;
        let err = OpenAiCompat::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("refused"));
    }

    #[test]
    fn parse_completion_empty_refusal_string_is_not_treated_as_a_refusal() {
        // Some servers always include a `refusal` key, empty when there is
        // none -- must not be misread as an actual refusal.
        let body = r#"{"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "{\"detail\":\"d\",\"headline\":\"h\"}", "refusal": ""}}]}"#;
        let completion = OpenAiCompat::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.headline, "h");
    }

    #[test]
    fn parse_completion_rejects_missing_message_content_when_not_length() {
        let body = r#"{"choices": [{"finish_reason": "stop", "message": {"role": "assistant"}}]}"#;
        let err = OpenAiCompat::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no message.content"));
    }

    #[test]
    fn parse_completion_reads_usage_when_present() {
        let body = r#"{"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "{\"detail\":\"d\",\"headline\":\"h\"}"}}], "usage": {"prompt_tokens": 10, "completion_tokens": 5}}"#;
        let completion = OpenAiCompat::parse_completion(body).expect("should parse");
        let usage = completion.usage.expect("usage should be present");
        assert_eq!(usage.input_tokens, 10);
        assert_eq!(usage.output_tokens, 5);
    }

    #[test]
    fn parse_completion_usage_is_none_when_absent() {
        let body = r#"{"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "{\"detail\":\"d\",\"headline\":\"h\"}"}}]}"#;
        let completion = OpenAiCompat::parse_completion(body).expect("should parse");
        assert!(completion.usage.is_none());
    }

    #[test]
    fn request_effort_is_accepted_but_ignored_no_panic() {
        // This provider has no standardized effort field across arbitrary
        // OpenAI-compatible backends -- a `Request::effort` override must
        // not panic or change the body shape.
        let p = provider(CompatAuth::Bearer, Structured::JsonSchema);
        let mut r = req("sys", false);
        r.effort = Effort::High;
        let body = p.build_body(&r);
        assert!(body.get("effort").is_none());
        assert!(body.get("reasoning").is_none());
    }

    // -- live check (#16's Done-when for a real endpoint) -------------------
    //
    // Not run by default (`cargo test` / `cargo test provider` never touch
    // the network). Run explicitly: `cargo test openai_compat_live -- --ignored`.
    // Requires Ollama running locally with `gemma3:4b` pulled, serving its
    // OpenAI-compatible surface at `http://127.0.0.1:11434/v1`.
    #[test]
    #[ignore]
    fn openai_compat_live_answers_a_synthetic_arithmetic_screenshot_against_local_ollama() {
        use image::{ImageBuffer, Rgb};

        let width = 400u32;
        let height = 200u32;
        let mut img = ImageBuffer::from_pixel(width, height, Rgb([255u8, 255, 255]));
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

        // json_schema, not json_object: MEASURED 2026-09-17 (manual
        // Invoke-RestMethod probe against this same server/model, outside
        // this test): with `response_format: {"type": "json_object"}`,
        // gemma3:4b echoed the JSON *Schema itself* back
        // (`{"type":"object","properties":{"detail":...}}`) instead of an
        // instance of it, failing `parse_answer` with "missing field
        // `detail`" every time. With `response_format: {"type":
        // "json_schema", "json_schema": {...}}`, the same model reliably
        // returned a real instance (`{"headline":...,"detail":...}`) --
        // Ollama's /v1 surface DOES enforce `json_schema`, unlike
        // `json_object`, at least for this model. `json_schema` is the mode
        // this live check exercises for that reason.
        let provider = OpenAiCompat::new(
            "http://127.0.0.1:11434/v1",
            "gemma3:4b",
            CompatAuth::None,
            "",
            "",
            Structured::JsonSchema,
        );

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
            .expect("live openai-compat request against local Ollama should succeed");
        let elapsed = started.elapsed();

        let answer = parse_answer(&completion.text).expect("response should parse as an Answer");
        assert!(!answer.headline.is_empty());
        eprintln!(
            "openai_compat_live: model=gemma3:4b elapsed={:?} headline={:?}",
            elapsed, answer.headline
        );

        // #16's live-check instructions: /v1 has no `keep_alive`
        // equivalent, so unload the model afterwards via the native
        // `/api/generate` endpoint instead, through the same guarded
        // transport every provider uses (`common::post_json`).
        let unload = common::post_json(
            "http://127.0.0.1:11434/api/generate",
            &[("Content-Type", "application/json")],
            &json!({"model": "gemma3:4b", "keep_alive": 0}),
            Duration::from_secs(10),
            "ollama-unload",
        );
        eprintln!("openai_compat_live: unload result={unload:?}");
    }
}
