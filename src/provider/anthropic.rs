use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Answer, Difficulty, Provider, Shot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

/// The wire shape of the model's JSON payload. Kept separate from the public
/// `Answer` because `difficulty` arrives as a bare string ("7", "U", ...)
/// that is not a `Difficulty`'s natural `Deserialize` form — it is parsed
/// explicitly in `parse_response`, and a bad/missing value must degrade to
/// `None` rather than fail the whole parse.
#[derive(serde::Deserialize)]
struct RawAnswer {
    detail: String,
    headline: String,
    #[serde(default)]
    difficulty: Option<String>,
}

pub struct Anthropic {
    pub api_key: String,
    pub model: String,
    pub effort: String,
}

impl Anthropic {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, effort: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            effort: effort.into(),
        }
    }

    /// Builds the exact request body documented in the design spec's
    /// Anthropic section. Pure and network-free so it can be unit tested
    /// directly.
    ///
    /// Critical details, do not change without re-reading the spec:
    /// - `output_config.format`, never the deprecated `output_format`.
    /// - Anthropic's `format` object takes `schema` directly: no `name`, no
    ///   `strict` (those are the OpenAI spelling).
    /// - Never send `budget_tokens`.
    /// - Never prefill an assistant turn — the only message is the user turn.
    /// - `effort` is omitted for models that reject it (see
    ///   [`supports_effort`]), because sending it there is a hard 400.
    ///
    /// When `want_difficulty` is false this must stay byte-identical to the
    /// pre-difficulty shape: no `difficulty` property, no rubric text in
    /// `system` (see `build_body_is_unchanged_when_difficulty_off`).
    fn build_body(&self, shot: &Shot, prompt: &str, want_difficulty: bool) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);

        // The rubric is appended here, never merged into DEFAULT_PROMPT, so
        // the user's own edited prompt text in Settings is untouched.
        let system_prompt = if want_difficulty {
            format!("{prompt}{}", super::DIFFICULTY_RUBRIC)
        } else {
            prompt.to_string()
        };

        // `detail` is listed (and required) before `headline` deliberately:
        // with `headline` first the model committed to a verdict before
        // doing the arithmetic and then contradicted itself. `difficulty`
        // goes last, after `headline`, so the model rates the problem only
        // once it has actually worked through it rather than up front.
        let mut properties = json!({
            "detail": {"type": "string"},
            "headline": {"type": "string"}
        });
        let mut required = vec!["detail", "headline"];
        if want_difficulty {
            properties["difficulty"] = json!({
                "type": "string",
                "enum": ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "U"]
            });
            required.push("difficulty");
        }

        let mut output_config = json!({
            "format": {
                    "type": "json_schema",
                "schema": {"type": "object",
                    "properties": properties,
                    "required": required, "additionalProperties": false}
            }
        });
        if supports_effort(&self.model) && !self.effort.is_empty() {
            output_config["effort"] = Value::String(self.effort.clone());
        }

        json!({
            "model": self.model,
            "max_tokens": 4000,
            "system": system_prompt,
            "output_config": output_config,
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64}},
                {"type": "text", "text": "Check my working."}
            ]}]
        })
    }

    /// Parses a successful (2xx) Anthropic Messages API body into an
    /// `Answer`. Returns a clear error if the model refused rather than
    /// answering.
    fn parse_response(body: &str) -> Result<Answer> {
        let value: Value = serde_json::from_str(body).context("anthropic: response body is not valid JSON")?;

        if value.get("stop_reason").and_then(Value::as_str) == Some("refusal") {
            return Err(anyhow!("anthropic: model refused to answer"));
        }

        let content = value
            .get("content")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("anthropic: response has no content[] array"))?;

        let text = content
            .iter()
            .find(|block| block.get("type").and_then(Value::as_str) == Some("text"))
            .and_then(|block| block.get("text"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("anthropic: no text block found in content[]"))?;

        let raw: RawAnswer =
            serde_json::from_str(text).context("anthropic: text block is not a valid Answer")?;
        Ok(Answer {
            detail: raw.detail,
            headline: raw.headline,
            // A missing or unparseable difficulty must yield `None`, never
            // an error — the answer itself is what matters.
            difficulty: raw.difficulty.as_deref().and_then(Difficulty::parse),
        })
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn ready(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn ask(&self, shot: &Shot, prompt: &str, want_difficulty: bool) -> Result<Answer> {
        let body = self.build_body(shot, prompt, want_difficulty);

        // `http_status_as_error(false)` so a non-2xx comes back as `Ok` with
        // the real response (and its body) instead of an `Err` that has
        // discarded the body we need to report.
        let mut response = ureq::post(ENDPOINT)
            .header("x-api-key", self.api_key.clone())
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .send_json(&body)
            .map_err(|e| anyhow!("anthropic: transport error: {e}"))?;

        let status = response.status();
        let body_text = response
            .body_mut()
            .read_to_string()
            .context("anthropic: failed to read response body")?;

        if !status.is_success() {
            let truncated: String = body_text.chars().take(300).collect();
            return Err(anyhow!("anthropic: HTTP {status}: {truncated}"));
        }

        Self::parse_response(&body_text)
    }
}


/// Whether a model accepts `output_config.effort`.
///
/// Effort is rejected outright on the 4.5-generation Sonnet and Haiku models —
/// sending it returns a 400 rather than being ignored — so those must be
/// filtered out rather than passed through hopefully. Everything current
/// (Opus 5 / 4.8 / 4.7 / 4.6, Sonnet 5, the Fable line) accepts it, so the
/// default is to send it and carve out the known exceptions.
fn supports_effort(model: &str) -> bool {
    const NO_EFFORT: [&str; 2] = ["claude-haiku-4-5", "claude-sonnet-4-5"];
    !NO_EFFORT.iter().any(|m| model.starts_with(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn sample_shot() -> Shot {
        Shot {
            png: vec![0x89, 0x50, 0x4E, 0x47],
            width: 100,
            height: 200,
        }
    }

    #[test]
    fn build_body_matches_spec_shape() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text", false);

        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&sample_shot().png);

        let expected = json!({
            "model": "claude-opus-5",
            "max_tokens": 4000,
            "system": "system prompt text",
            "output_config": {
                "effort": "low",
                "format": {"type": "json_schema",
                    "schema": {"type": "object",
                        "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                        "required": ["detail", "headline"], "additionalProperties": false}}
            },
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": expected_b64}},
                {"type": "text", "text": "Check my working."}
            ]}]
        });

        assert_eq!(body, expected);

        // Guard against regressions to the deprecated/incorrect spellings.
        assert!(body.get("output_format").is_none());
        assert!(body["output_config"].get("budget_tokens").is_none());
        assert!(body["output_config"]["format"].get("name").is_none());
        assert!(body["output_config"]["format"].get("strict").is_none());
    }

    #[test]
    fn build_body_never_prefills_assistant_turn() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&sample_shot(), "prompt", false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    /// When the toggle is off, the schema and prompt must be byte-identical
    /// to today's — no empty `difficulty` property, no stray rubric text.
    #[test]
    fn build_body_is_unchanged_when_difficulty_off() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let with_flag = provider.build_body(&sample_shot(), "system prompt text", false);
        let expected_b64 = base64::engine::general_purpose::STANDARD.encode(&sample_shot().png);
        let today = json!({
            "model": "claude-opus-5",
            "max_tokens": 4000,
            "system": "system prompt text",
            "output_config": {
                "effort": "low",
                "format": {"type": "json_schema",
                    "schema": {"type": "object",
                        "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                        "required": ["detail", "headline"], "additionalProperties": false}}
            },
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": expected_b64}},
                {"type": "text", "text": "Check my working."}
            ]}]
        });
        assert_eq!(with_flag, today);
        assert_eq!(with_flag["system"], "system prompt text");
    }

    #[test]
    fn build_body_adds_difficulty_property_when_requested() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text", true);

        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(
            schema["properties"]["difficulty"],
            json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U"]})
        );
        assert_eq!(schema["required"], json!(["detail", "headline", "difficulty"]));
        // Anthropic's format object still takes no `name`/`strict`.
        assert!(body["output_config"]["format"].get("name").is_none());
        assert!(body["output_config"]["format"].get("strict").is_none());

        // The rubric is appended, not merged into the caller's prompt text.
        let system = body["system"].as_str().unwrap();
        assert!(system.starts_with("system prompt text"));
        assert!(system.contains("difficulty"));
        assert!(system.len() > "system prompt text".len());
    }

    #[test]
    fn ready_reflects_api_key_presence() {
        assert!(Anthropic::new("sk-ant-real", "claude-opus-5", "low").ready());
        assert!(!Anthropic::new("", "claude-opus-5", "low").ready());
        assert!(!Anthropic::new("   ", "claude-opus-5", "low").ready());
    }

    #[test]
    fn parse_response_extracts_first_text_block() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response.json")
            .expect("fixture file should exist");
        let answer = Anthropic::parse_response(&body).expect("should parse");
        assert_eq!(answer.headline, "sample variance is 6.5, not 5.2");
        assert_eq!(
            answer.detail,
            "s^2 = sum((x-mean)^2)/(n-1) = 26/4 = 6.5. You divided by n instead of n-1."
        );
        // The fixture predates the difficulty field entirely.
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_parses_a_valid_difficulty() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"U\"}"}
        ]}"#;
        let answer = Anthropic::parse_response(body).expect("should parse");
        assert_eq!(answer.difficulty, Some(Difficulty::Ultra));
    }

    #[test]
    fn parse_response_degrades_unparseable_difficulty_to_none_without_erroring() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"way too hard\"}"}
        ]}"#;
        let answer = Anthropic::parse_response(body).expect("should still parse the answer");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_missing_difficulty_key_is_none() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\"}"}
        ]}"#;
        let answer = Anthropic::parse_response(body).expect("should parse");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_rejects_refusal() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response_refusal.json")
            .expect("fixture file should exist");
        let err = Anthropic::parse_response(&body).unwrap_err();
        assert!(err.to_string().contains("refused"));
    }

    #[test]
    fn parse_response_rejects_invalid_json() {
        let err = Anthropic::parse_response("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn effort_is_sent_for_models_that_accept_it() {
        let p = Anthropic::new("k", "claude-opus-5", "low");
        let body = p.build_body(&sample_shot(), "sys", false);
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn effort_is_omitted_for_models_that_reject_it() {
        // Sending effort to these is a 400, not a no-op.
        for model in ["claude-haiku-4-5", "claude-sonnet-4-5"] {
            let p = Anthropic::new("k", model, "low");
            let body = p.build_body(&sample_shot(), "sys", false);
            assert!(
                body["output_config"].get("effort").is_none(),
                "{model} must not carry effort"
            );
            // The schema must still be there — only effort is dropped.
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        }
    }

    #[test]
    fn empty_effort_is_never_sent() {
        let p = Anthropic::new("k", "claude-opus-5", "");
        let body = p.build_body(&sample_shot(), "sys", false);
        assert!(body["output_config"].get("effort").is_none());
    }
}
