use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Answer, Provider, Shot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

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
    fn build_body(&self, shot: &Shot, prompt: &str) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);

        json!({
            "model": self.model,
            "max_tokens": 4000,
            "system": prompt,
            "output_config": {
                "effort": self.effort,
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object",
                        "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                        "required": ["detail", "headline"], "additionalProperties": false}
                }
            },
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

        serde_json::from_str::<Answer>(text).context("anthropic: text block is not a valid Answer")
    }
}

impl Provider for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn ready(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    fn ask(&self, shot: &Shot, prompt: &str) -> Result<Answer> {
        let body = self.build_body(shot, prompt);

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
        let body = provider.build_body(&sample_shot(), "system prompt text");

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
        let body = provider.build_body(&sample_shot(), "prompt");
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
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
}
