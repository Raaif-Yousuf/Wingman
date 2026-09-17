use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::common;
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
    /// - `effort` is omitted for models that reject it (see
    ///   [`supports_effort`]), because sending it there is a hard 400.
    ///
    /// When `want_difficulty` is false this must stay byte-identical to the
    /// pre-difficulty shape: no `difficulty` property, no rubric text in
    /// `system` (see `build_body_is_unchanged_when_difficulty_off`).
    fn build_body(&self, shot: &Shot, prompt: &str, want_difficulty: bool) -> Value {
        let b64 = common::encode_images_base64(std::slice::from_ref(&shot.png))
            .pop()
            .expect("exactly one image was passed in");

        let system_prompt = common::augmented_system_prompt(prompt, want_difficulty);
        let schema = common::answer_schema(want_difficulty);

        let mut output_config = json!({
            "format": {
                    "type": "json_schema",
                "schema": schema
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

        let stop_reason = value.get("stop_reason").and_then(Value::as_str);

        if stop_reason == Some("refusal") {
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
            .and_then(Value::as_str);

        let text = match text {
            Some(t) => t,
            None => {
                // `stop_reason: "max_tokens"` with no `text` block means the
                // model spent its whole output-token budget on thinking and
                // never got to emit the answer -- a specific, reachable
                // shape, not a generic malformed response (#155).
                if stop_reason == Some("max_tokens") {
                    return Err(anyhow!(
                        "anthropic: The model ran out of room before answering. Lower the effort setting or raise the token limit."
                    ));
                }
                return Err(anyhow!("anthropic: no text block found in content[]"));
            }
        };

        common::parse_answer_text("anthropic", text)
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

        let body_text = common::post_json(
            ENDPOINT,
            &[
                ("x-api-key", self.api_key.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
                ("content-type", "application/json"),
            ],
            &body,
            REQUEST_TIMEOUT,
            "anthropic",
        )?;

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
    use base64::Engine;
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
            json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U","N"]})
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

    // #156: `assert_eq!(a_value, b_value)` on two `serde_json::Value`s does
    // NOT check key order -- with `preserve_order`, `Value::Object` is
    // backed by an `IndexMap`, and its `PartialEq` compares as a set. Only a
    // *serialized string* comparison catches a property-order regression,
    // and rule 3 says order is load-bearing. These two golden strings were
    // captured from this exact function before the #156 dedup refactor
    // (`cargo test zzz_print_golden_strings -- --nocapture`, then deleted)
    // and must still match byte-for-byte after it.
    #[test]
    fn golden_request_body_no_difficulty_is_byte_identical() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text", false);
        let golden = r#"{"model":"claude-opus-5","max_tokens":4000,"system":"system prompt text","output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false}},"effort":"low"},"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw=="}},{"type":"text","text":"Check my working."}]}]}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
    }

    #[test]
    fn golden_request_body_with_difficulty_is_byte_identical() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&sample_shot(), "system prompt text", true);
        let golden = r#"{"model":"claude-opus-5","max_tokens":4000,"system":"system prompt text\n\nAlso rate how difficult the PROBLEM ON SCREEN is for a HUMAN STUDENT. Add a third field:\n- difficulty: THIRD, after headline, once you have actually worked the problem through. Exactly one of \"1\" through \"10\", or \"U\".\n\nCalibration is the hard part, so read this carefully. You solve nearly all of these easily; that is NOT the scale. Do not rate your own confidence, your own effort, or how quickly you found the answer. Rate how hard the problem would be for a student at the level it is aimed at. Rating by your own effort compresses everything into 1-5 and makes the whole scale useless.\n\nAnchors:\n1 = an easy high-school question. One step, one formula. (speed = distance / time)\n2 = high-school, a couple of steps.\n3 = easy university intro-course level. (a block on an incline; moment of inertia of a disk)\n4 = intro university, several steps or a small subtlety.\n5 = medium university level. Mid-degree material: multi-step, and you must choose the method rather than being told it.\n6 = upper-undergraduate, harder than routine homework.\n7 = hard university level. Typically GRADUATE coursework: quantum perturbation theory, Lagrangian mechanics with constraints, a non-obvious statistical derivation.\n8 = graduate coursework that most of the class would get wrong.\n9 = very hard for an undergraduate. Qualifying-exam standard.\n10 = a PhD student in the field would struggle. Open-ended derivations and proofs requiring a specialist technique, not just more algebra.\nU = Ultra: a professor would struggle. Research-level, or a known-hard proof.\n\nUse the WHOLE range. Most routine homework is 2-5. If the problem is recognisably graduate-level, it starts at 7, not 5. If it asks you to PROVE a general theorem rather than compute a value, it is almost never below 8.\n\nIf there is no problem to rate at all -- the screen shows no question, or you are asking for something to be made visible -- answer \"N\". Do NOT reach for \"U\" in that case: \"U\" means the problem is extraordinarily hard, not that you could not find one.","output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"},"difficulty":{"type":"string","enum":["1","2","3","4","5","6","7","8","9","10","U","N"]}},"required":["detail","headline","difficulty"],"additionalProperties":false}},"effort":"low"},"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw=="}},{"type":"text","text":"Check my working."}]}]}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
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
        assert_eq!(answer.difficulty, Some(crate::provider::Difficulty::Ultra));
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

    /// #155: `stop_reason: "max_tokens"` with only a `thinking` block and
    /// no `text` block -- the model spent its whole output-token budget on
    /// thinking. Must be a specific, actionable message, not the generic
    /// "no text block found" a genuinely malformed body gets.
    #[test]
    fn parse_response_reports_budget_exhaustion_for_max_tokens_with_no_text() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response_max_tokens.json")
            .expect("fixture file should exist");
        let err = Anthropic::parse_response(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
        assert!(!msg.contains("no text block found"), "{msg}");
    }

    /// Neighbour of the above: a missing text block with a stop_reason
    /// other than `max_tokens` is genuinely malformed, not budget
    /// exhaustion, and must keep the generic message.
    #[test]
    fn parse_response_rejects_missing_text_block_when_not_max_tokens() {
        let body = r#"{"stop_reason": "end_turn", "content": []}"#;
        let err = Anthropic::parse_response(body).unwrap_err();
        assert!(err.to_string().contains("no text block found"));
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
