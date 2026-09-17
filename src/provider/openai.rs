use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

use super::{Answer, Difficulty, Provider, Shot};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.openai.com/v1/responses";

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
    ///
    /// When `want_difficulty` is false this must stay byte-identical to the
    /// pre-difficulty shape: no `difficulty` property, no rubric text in
    /// `instructions` (see `build_body_is_unchanged_when_difficulty_off`).
    fn build_body(&self, shot: &Shot, prompt: &str, want_difficulty: bool) -> Value {
        let b64 = base64::engine::general_purpose::STANDARD.encode(&shot.png);
        let data_url = format!("data:image/png;base64,{b64}");

        // The rubric is appended here, never merged into DEFAULT_PROMPT,
        // so the user's own edited prompt text in Settings is untouched.
        let instructions = if want_difficulty {
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
                "enum": ["1", "2", "3", "4", "5", "6", "7", "8", "9", "10", "U", "N"]
            });
            required.push("difficulty");
        }

        let mut body = json!({
            "model": self.model,
            "instructions": instructions,
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Check my working."},
                {"type": "input_image", "image_url": data_url, "detail": "high"}
            ]}],
            "reasoning": {"effort": self.effort},
            "max_output_tokens": 2500,
            "text": {"format": {
                "type": "json_schema", "name": "answer", "strict": true,
                "schema": {"type": "object",
                    "properties": properties,
                    "required": required, "additionalProperties": false}
            }}
        });

        // Mirrors Anthropic's `!self.effort.is_empty()` guard: an empty
        // `effort` is a plausible hand-edit of config.toml (`effort = ""`
        // meaning "use the default"), and the Responses API validates
        // `reasoning.effort` against a fixed enum -- sending an empty
        // string is a hard 400 on every request, not a no-op (#154).
        if self.effort.trim().is_empty() {
            body.as_object_mut()
                .expect("body is always an object")
                .remove("reasoning");
        }

        body
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
            // `status: "incomplete"` with no `message` entry means the
            // model spent its whole output-token budget on reasoning and
            // never got to emit the answer -- a specific, reachable shape
            // (see `tests/fixtures/openai_response_no_message.json`), not
            // a generic malformed response (#155).
            if value.get("status").and_then(Value::as_str) == Some("incomplete") {
                return Err(anyhow!(
                    "openai: The model ran out of room before answering. Lower the effort setting or raise the token limit."
                ));
            }
            return Err(anyhow!("openai: no message text found in output[]"));
        }

        let raw: RawAnswer = serde_json::from_str(&text).context("openai: message text is not a valid Answer")?;
        Ok(Answer {
            detail: raw.detail,
            headline: raw.headline,
            // A missing or unparseable difficulty must yield `None`, never
            // an error — the answer itself is what matters.
            difficulty: raw.difficulty.as_deref().and_then(Difficulty::parse),
        })
    }
}

impl Provider for OpenAi {
    fn name(&self) -> &'static str {
        "openai"
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
        let body = provider.build_body(&sample_shot(), "system prompt text", false);

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

    /// When the toggle is off, the schema and prompt must be byte-identical
    /// to today's — no empty `difficulty` property, no stray rubric text.
    #[test]
    fn build_body_is_unchanged_when_difficulty_off() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let with_flag = provider.build_body(&sample_shot(), "system prompt text", false);
        let today = json!({
            "model": "gpt-5.5",
            "instructions": "system prompt text",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Check my working."},
                {"type": "input_image", "image_url": format!(
                    "data:image/png;base64,{}",
                    base64::engine::general_purpose::STANDARD.encode(&sample_shot().png)
                ), "detail": "high"}
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
        assert_eq!(with_flag, today);
        assert_eq!(with_flag["instructions"], "system prompt text");
    }

    #[test]
    fn build_body_adds_difficulty_property_when_requested() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text", true);

        let schema = &body["text"]["format"]["schema"];
        assert_eq!(
            schema["properties"]["difficulty"],
            json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U","N"]})
        );
        assert_eq!(schema["required"], json!(["detail", "headline", "difficulty"]));

        // The rubric is appended, not merged into the caller's prompt text.
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("system prompt text"));
        assert!(instructions.contains("difficulty"));
        assert!(instructions.len() > "system prompt text".len());
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
        // The fixture predates the difficulty field entirely.
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_parses_a_valid_difficulty() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"7\"}"}
        ]}]}"#;
        let answer = OpenAi::parse_response(body).expect("should parse");
        assert_eq!(answer.difficulty, Some(Difficulty::Level(7)));
    }

    #[test]
    fn parse_response_degrades_unparseable_difficulty_to_none_without_erroring() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"not-a-level\"}"}
        ]}]}"#;
        let answer = OpenAi::parse_response(body).expect("should still parse the answer");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_missing_difficulty_key_is_none() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\"}"}
        ]}]}"#;
        let answer = OpenAi::parse_response(body).expect("should parse");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_response_rejects_invalid_json() {
        let err = OpenAi::parse_response("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    /// #155: `openai_response_no_message.json` is `status: "incomplete"`
    /// with only a `reasoning` entry (empty summary) and no `message` at
    /// all -- the model spent its whole output-token budget on reasoning.
    /// That must surface as an actionable, specific message, not the
    /// generic "no message text found" a genuinely malformed body gets.
    #[test]
    fn parse_response_reports_budget_exhaustion_for_incomplete_status_with_no_message() {
        let body = fs::read_to_string("tests/fixtures/openai_response_no_message.json")
            .expect("fixture file should exist");
        let err = OpenAi::parse_response(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
        assert!(!msg.contains("no message text"), "{msg}");
    }

    /// Neighbour of the above: a body with no message and no `status:
    /// "incomplete"` is a genuinely malformed response, not budget
    /// exhaustion, and must keep the generic message.
    #[test]
    fn parse_response_rejects_body_with_no_message_and_no_incomplete_status() {
        let body = r#"{"output": []}"#;
        let err = OpenAi::parse_response(body).unwrap_err();
        assert!(err.to_string().contains("no message text"));
    }

    /// #154: mirrors Anthropic's `empty_effort_is_never_sent` -- an empty
    /// `effort` string is a plausible hand-edit of config.toml and must
    /// never be sent verbatim (OpenAI's Responses API 400s on it).
    #[test]
    fn empty_effort_is_never_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "");
        let body = provider.build_body(&sample_shot(), "sys", false);
        assert!(body.get("reasoning").is_none());
    }

    /// Neighbour: whitespace-only is the same footgun as empty.
    #[test]
    fn whitespace_only_effort_is_never_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "   ");
        let body = provider.build_body(&sample_shot(), "sys", false);
        assert!(body.get("reasoning").is_none());
    }

    /// Neighbour: a real effort value must still be sent (not swallowed by
    /// the guard).
    #[test]
    fn non_empty_effort_is_still_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&sample_shot(), "sys", false);
        assert_eq!(body["reasoning"]["effort"], "low");
    }
}
