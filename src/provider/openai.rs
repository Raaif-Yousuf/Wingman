use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Answer, Provider, Shot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.openai.com/v1/responses";

pub struct OpenAi {
    pub api_key: String,
    pub model: String,
    pub effort: String,
}

impl OpenAi {
    pub fn new(api_key: impl Into<String>, model: impl Into<String>, effort: impl Into<String>) -> Self {
        Self {
            api_key: api_key.into(),
            model: model.into(),
            effort: effort.into(),
        }
    }

    /// Builds the exact request body documented in the design spec's OpenAI
    /// section. Pure and network-free so it can be unit tested directly.
    fn build_body(&self, shot: &Shot, prompt: &str) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);
        let data_url = format!("data:image/png;base64,{b64}");

        json!({
            "model": self.model,
            "instructions": prompt,
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Check my working."},
                {"type": "input_image", "image_url": data_url, "detail": "high"}
            ]}],
            "reasoning": {"effort": self.effort},
            "max_output_tokens": 2500,
            "text": {"format": {
                "type": "json_schema", "name": "answer", "strict": true,
                "schema": {"type": "object",
                    "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                    "required": ["detail", "headline"], "additionalProperties": false}
            }}
        })
    }

    /// Parses a successful (2xx) OpenAI Responses API body into an `Answer`.
    ///
    /// The `output` array may contain a `reasoning` entry before the
    /// `message` entry on reasoning models, so non-`message` entries are
    /// skipped rather than assumed absent.
    fn parse_response(body: &str) -> Result<Answer> {
        let value: Value = serde_json::from_str(body).context("openai: response body is not valid JSON")?;

        let output = value
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("openai: response has no output[] array"))?;

        let mut text = String::new();
        for entry in output {
            if entry.get("type").and_then(Value::as_str) != Some("message") {
                continue;
            }
            if let Some(content) = entry.get("content").and_then(Value::as_array) {
                for part in content {
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }

        if text.is_empty() {
            return Err(anyhow!("openai: no message text found in output[]"));
        }

        serde_json::from_str::<Answer>(&text).context("openai: message text is not a valid Answer")
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
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
            .header("Authorization", format!("Bearer {}", self.api_key))
            .header("Content-Type", "application/json")
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .send_json(&body)
            .map_err(|e| anyhow!("openai: transport error: {e}"))?;

        let status = response.status();
        let body_text = response
            .body_mut()
            .read_to_string()
            .context("openai: failed to read response body")?;

        if !status.is_success() {
            let truncated: String = body_text.chars().take(300).collect();
            return Err(anyhow!("openai: HTTP {status}: {truncated}"));
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
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text");

        let expected_data_url = format!(
            "data:image/png;base64,{}",
            base64::engine::general_purpose::STANDARD.encode(&sample_shot().png)
        );

        let expected = json!({
            "model": "gpt-5.5",
            "instructions": "system prompt text",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Check my working."},
                {"type": "input_image", "image_url": expected_data_url, "detail": "high"}
            ]}],
            "reasoning": {"effort": "low"},
            "max_output_tokens": 2500,
            "text": {"format": {
                "type": "json_schema", "name": "answer", "strict": true,
                "schema": {"type": "object",
                    "properties": {"detail": {"type": "string"}, "headline": {"type": "string"}},
                    "required": ["detail", "headline"], "additionalProperties": false}
            }}
        });

        assert_eq!(body, expected);
    }

    #[test]
    fn ready_reflects_api_key_presence() {
        assert!(OpenAi::new("sk-real", "gpt-5.5", "low").ready());
        assert!(!OpenAi::new("", "gpt-5.5", "low").ready());
        assert!(!OpenAi::new("   ", "gpt-5.5", "low").ready());
    }

    #[test]
    fn parse_response_skips_reasoning_entry_and_extracts_message() {
        let body = fs::read_to_string("tests/fixtures/openai_response.json")
            .expect("fixture file should exist");
        let answer = OpenAi::parse_response(&body).expect("should parse");
        assert_eq!(answer.headline, "42 m/s is correct");
        assert_eq!(answer.detail, "v = u + at = 0 + 9.8*4.3 = 42.1, rounds to 42.");
    }

    #[test]
    fn parse_response_rejects_body_with_no_message_output() {
        let body = fs::read_to_string("tests/fixtures/openai_response_no_message.json")
            .expect("fixture file should exist");
        let err = OpenAi::parse_response(&body).unwrap_err();
        assert!(err.to_string().contains("no message text"));
    }

    #[test]
    fn parse_response_rejects_invalid_json() {
        let err = OpenAi::parse_response("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }
}
