use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use super::common;
use super::{Caps, Completion, Effort, ImageLimits, Provider, Request, StopReason, Usage};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// Used when `Request::max_tokens` is `0` (the caller has no opinion).
const DEFAULT_MAX_TOKENS: u32 = 4000;

pub struct Anthropic {
    pub api_key: String,
    pub model: String,
    /// The configured default, used when `Request::effort` is `Effort::Unset`.
    pub effort: Effort,
}

impl Anthropic {
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
    /// - The schema, system text and image bytes all come from `req`: this
    ///   provider never builds the physics-check schema itself (#12; see the
    ///   2026-09-16 expansion plan's "Provider trait, extended").
    fn build_body(&self, req: &Request) -> Value {
        let mut content: Vec<Value> = common::encode_images_base64(&req.images)
            .into_iter()
            .map(|b64| {
                serde_json::json!({"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": b64}})
            })
            .collect();
        content.push(serde_json::json!({"type": "text", "text": req.user}));

        let mut output_config = serde_json::json!({});
        if let Some(schema) = &req.schema {
            output_config["format"] =
                serde_json::json!({"type": "json_schema", "schema": common::wire_schema(schema)});
        }
        let effort = if req.effort != Effort::Unset {
            req.effort
        } else {
            self.effort
        };
        if supports_effort(&self.model) {
            if let Some(e) = effort.as_str() {
                output_config["effort"] = Value::String(e.to_string());
            }
        }

        let max_tokens = if req.max_tokens > 0 {
            req.max_tokens
        } else {
            DEFAULT_MAX_TOKENS
        };

        serde_json::json!({
            "model": self.model,
            "max_tokens": max_tokens,
            "system": req.system,
            "output_config": output_config,
            "messages": [{"role": "user", "content": content}]
        })
    }

    /// Parses a successful (2xx) Anthropic Messages API body into a
    /// `Completion`. Returns a clear error if the model refused rather than
    /// answering. The text is returned as-is -- this provider never
    /// interprets it against a schema (#12).
    pub(crate) fn parse_completion(body: &str) -> Result<Completion> {
        let value: Value =
            serde_json::from_str(body).context("anthropic: response body is not valid JSON")?;

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
                return Err(match stop_reason {
                    Some("pause_turn") => anyhow!(
                        "anthropic: the reply was paused before it finished (pause_turn)"
                    ),
                    Some("model_context_window_exceeded") => anyhow!(
                        "anthropic: the request was too long for the model's context window (model_context_window_exceeded)"
                    ),
                    Some(other) => anyhow!(
                        "anthropic: no text block found in content[] (stop_reason: {})",
                        super::safe_reason(other)
                    ),
                    None => anyhow!("anthropic: no text block found in content[]"),
                });
            }
        };

        let stop = match stop_reason {
            Some("max_tokens") => StopReason::MaxTokens,
            Some("end_turn") | Some("stop_sequence") => StopReason::Complete,
            _ => StopReason::Other,
        };

        // `input_tokens` excludes the two cache fields; each is priced at its
        // own rate (see `usage.rs`). Absent today (no `cache_control`, #180).
        let usage = Self::usage_of(&value);

        Ok(Completion {
            text: text.to_string(),
            usage,
            stop,
        })
    }

    /// The usage block of a body, whether or not it holds an answer: a
    /// refusal or a truncated reply was billed too (#730).
    pub(crate) fn parse_usage(body: &str) -> Option<Usage> {
        Self::usage_of(&serde_json::from_str::<Value>(body).ok()?)
    }

    fn usage_of(value: &Value) -> Option<Usage> {
        value.get("usage").map(|u| {
            let count = |key: &str| u.get(key).and_then(Value::as_u64).unwrap_or(0) as u32;
            Usage {
                input_tokens: count("input_tokens"),
                output_tokens: count("output_tokens"),
                cache_write_tokens: count("cache_creation_input_tokens"),
                cache_read_tokens: count("cache_read_input_tokens"),
            }
        })
    }
}

impl Provider for Anthropic {
    fn id(&self) -> &'static str {
        "anthropic"
    }

    fn ready(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// Issue #169: `image_limits` is Anthropic's high-resolution tier for
    /// Claude 4.7+ ([`supports_high_res_tier`]), else the standard tier --
    /// see `capture.rs`'s `ANTHROPIC_*` constants for the MEASURED source.
    fn capabilities(&self, model: &str) -> Caps {
        let (max_long_edge, max_pixels) = if supports_high_res_tier(model) {
            (
                crate::capture::ANTHROPIC_HIGH_RES_MAX_LONG_EDGE,
                crate::capture::ANTHROPIC_HIGH_RES_MAX_PIXELS,
            )
        } else {
            (
                crate::capture::ANTHROPIC_STANDARD_MAX_LONG_EDGE,
                crate::capture::ANTHROPIC_STANDARD_MAX_PIXELS,
            )
        };
        Caps {
            vision: true,
            json_schema: true,
            thinking: supports_effort(model),
            image_limits: Some(ImageLimits {
                max_long_edge,
                max_pixels,
            }),
        }
    }

    fn own_caps(&self) -> Caps {
        self.capabilities(&self.model)
    }

    fn model_label(&self) -> String {
        format!("{}:{}", self.id(), self.model)
    }

    fn complete(&self, req: &Request) -> Result<Completion> {
        let body = self.build_body(req);

        let body_text = common::post_json(
            ENDPOINT,
            &[
                ("x-api-key", self.api_key.as_str()),
                ("anthropic-version", ANTHROPIC_VERSION),
                ("content-type", "application/json"),
            ],
            body,
            REQUEST_TIMEOUT,
            "anthropic",
        )?;

        crate::usage::tally(
            self.id(),
            &self.model,
            false,
            Self::parse_completion(&body_text),
            || Self::parse_usage(&body_text),
        )
    }
}

/// Whether a model accepts `output_config.effort`.
///
/// An allowlist by family and version, because sending effort to a model that
/// lacks it is a hard 400, not a no-op. MEASURED 2026-10-16 (one tiny call
/// each, test key): Haiku 4.5 answers 400 "This model does not support the
/// effort parameter"; these accepted it: Opus 4.5, 4.6, 4.7, 4.8, 5, 5.5,
/// Sonnet 4.6, 5, 5.5, Haiku 5.5, Fable 5 and 5.1. Sonnet 4.5 was measured
/// on 2026-09-15 to reject it too. Older ids (Sonnet 4, Opus 4 and 4.1, the
/// 3.x line) are retired for this key (404), so they could not be measured;
/// THEORY (unverified): they reject effort like Sonnet and Haiku 4.5, so they
/// are left out. An id this cannot place also gets no effort: the cost of a
/// wrong "no" is the model's default effort, the cost of a wrong "yes" is a
/// 400 card on every press.
///
/// Minimum versions: Opus 4.5, Sonnet 4.6, Haiku 5, any Fable. A `-latest`
/// alias carries no version, so it gets no effort (the model's own default).
///
/// Also used as this provider's `thinking` capability: on this API, effort
/// support and extended-thinking support are the same models.
fn supports_effort(model: &str) -> bool {
    match parse_claude_id(model) {
        Some((Family::Opus, v)) => v >= (4, 5),
        Some((Family::Sonnet, v)) => v >= (4, 6),
        Some((Family::Haiku, v)) => v >= (5, 0),
        Some((Family::Fable, _)) => true,
        None => false,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Family {
    Opus,
    Sonnet,
    Haiku,
    Fable,
}

/// Family and (major, minor) version of a Claude model id, in either naming
/// order (`claude-opus-4-5-20251101`, `claude-3-7-sonnet-20250219`). Date
/// stamps (more than two digits) and other words are ignored; a missing minor
/// is 0 (`claude-opus-5` is 5.0). `None` when no known family appears.
fn parse_claude_id(model: &str) -> Option<(Family, (u32, u32))> {
    let lower = model.to_ascii_lowercase();
    if !lower.starts_with("claude-") {
        return None;
    }
    let mut family = None;
    let mut nums = Vec::new();
    for part in lower.split('-').skip(1) {
        match part {
            "opus" => family = family.or(Some(Family::Opus)),
            "sonnet" => family = family.or(Some(Family::Sonnet)),
            "haiku" => family = family.or(Some(Family::Haiku)),
            "fable" => family = family.or(Some(Family::Fable)),
            _ => {
                if part.len() <= 2 {
                    if let Ok(n) = part.parse::<u32>() {
                        nums.push(n);
                    }
                }
            }
        }
    }
    let major = *nums.first()?;
    let minor = nums.get(1).copied().unwrap_or(0);
    Some((family?, (major, minor)))
}

/// Whether `model` is on Anthropic's "high-resolution" image tier (Claude
/// 4.7 and later: long edge 2576, 4784 visual tokens) rather than the
/// "standard" tier (long edge 1568, 1568 visual tokens) -- see
/// `capture.rs`'s `ANTHROPIC_HIGH_RES_MAX_LONG_EDGE`/`ANTHROPIC_STANDARD_MAX_LONG_EDGE`
/// doc comments for the MEASURED 2026-09-17 source of the two tiers
/// themselves.
///
/// By version number (4.7 or later), not by effort support: Opus 4.5/4.6 and
/// Sonnet 4.6 accept effort but sit on the standard tier. An id that cannot be
/// placed gets the standard tier, the smaller images (the safe side).
///
/// THEORY (unverified, not probed): that the Haiku 5.x and Fable lines are on
/// the high-resolution tier is inferred from "4.7 and later" in the vision
/// docs, by version number alone. A `-latest` alias has no version, so it is
/// "cannot be placed": no effort and the standard tier.
fn supports_high_res_tier(model: &str) -> bool {
    parse_claude_id(model).is_some_and(|(_, v)| v >= (4, 7))
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

    #[test]
    fn build_body_matches_spec_shape() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&req("system prompt text", false));

        let expected_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &sample_shot().png,
        );

        let expected = serde_json::json!({
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
        let body = provider.build_body(&req("prompt", false));
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    /// When the toggle is off, the schema and prompt must be byte-identical
    /// to today's — no empty `difficulty` property, no stray rubric text.
    #[test]
    fn build_body_is_unchanged_when_difficulty_off() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let with_flag = provider.build_body(&req("system prompt text", false));
        let expected_b64 = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            &sample_shot().png,
        );
        let today = serde_json::json!({
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
        let body = provider.build_body(&req("system prompt text", true));

        let schema = &body["output_config"]["format"]["schema"];
        assert_eq!(
            schema["properties"]["difficulty"],
            serde_json::json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U","N"]})
        );
        assert_eq!(
            schema["required"],
            serde_json::json!(["detail", "headline", "difficulty"])
        );
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
    // captured before the #156/#12 refactors and must still match
    // byte-for-byte after both.
    #[test]
    fn golden_request_body_no_difficulty_is_byte_identical() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&req("system prompt text", false));
        let golden = r#"{"model":"claude-opus-5","max_tokens":4000,"system":"system prompt text","output_config":{"format":{"type":"json_schema","schema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false}},"effort":"low"},"messages":[{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"iVBORw=="}},{"type":"text","text":"Check my working."}]}]}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
    }

    #[test]
    fn golden_request_body_with_difficulty_is_byte_identical() {
        let provider = Anthropic::new("sk-ant-test", "claude-opus-5", "low");
        let body = provider.build_body(&req("system prompt text", true));
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
    fn capabilities_report_vision_and_json_schema_always_and_thinking_per_model() {
        let provider = Anthropic::new("k", "claude-opus-5", "low");
        let caps = provider.capabilities("claude-opus-5");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(caps.thinking);

        let caps = provider.capabilities("claude-haiku-4-5");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(!caps.thinking);
    }

    // -- image_limits (issue #169) ---------------------------------------

    #[test]
    fn capabilities_reports_high_res_image_limits_for_4_7_plus_models() {
        let provider = Anthropic::new("k", "claude-opus-5", "low");
        for model in [
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-opus-4-8",
            "claude-fable-5-1",
        ] {
            let limits = provider
                .capabilities(model)
                .image_limits
                .unwrap_or_else(|| panic!("expected image_limits for {model}"));
            assert_eq!(
                limits.max_long_edge,
                crate::capture::ANTHROPIC_HIGH_RES_MAX_LONG_EDGE,
                "failed for {model}"
            );
            assert_eq!(
                limits.max_pixels,
                crate::capture::ANTHROPIC_HIGH_RES_MAX_PIXELS,
                "failed for {model}"
            );
        }
    }

    #[test]
    fn capabilities_reports_standard_image_limits_for_4_5_generation_models() {
        let provider = Anthropic::new("k", "claude-opus-5", "low");
        for model in ["claude-haiku-4-5", "claude-sonnet-4-5"] {
            let limits = provider
                .capabilities(model)
                .image_limits
                .unwrap_or_else(|| panic!("expected image_limits for {model}"));
            assert_eq!(
                limits.max_long_edge,
                crate::capture::ANTHROPIC_STANDARD_MAX_LONG_EDGE,
                "failed for {model}"
            );
            assert_eq!(
                limits.max_pixels,
                crate::capture::ANTHROPIC_STANDARD_MAX_PIXELS,
                "failed for {model}"
            );
        }
    }

    #[test]
    fn own_caps_matches_capabilities_for_the_configured_model() {
        let provider = Anthropic::new("k", "claude-haiku-4-5", "low");
        assert_eq!(
            provider.own_caps(),
            provider.capabilities("claude-haiku-4-5")
        );
    }

    #[test]
    fn parse_completion_reads_usage_and_cache_tokens() {
        let body = r#"{"stop_reason":"end_turn","content":[{"type":"text","text":"{}"}],"usage":{"input_tokens":11,"output_tokens":22,"cache_creation_input_tokens":33,"cache_read_input_tokens":44}}"#;
        let u = Anthropic::parse_completion(body).unwrap().usage.unwrap();
        assert_eq!(
            (
                u.input_tokens,
                u.output_tokens,
                u.cache_write_tokens,
                u.cache_read_tokens
            ),
            (11, 22, 33, 44)
        );
        let plain = include_str!("../../tests/fixtures/anthropic_response.json");
        let u = Anthropic::parse_completion(plain).unwrap().usage.unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (900, 60));
        assert_eq!((u.cache_write_tokens, u.cache_read_tokens), (0, 0));
    }

    #[test]
    fn parse_completion_extracts_first_text_block() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response.json")
            .expect("fixture file should exist");
        let completion = Anthropic::parse_completion(&body).expect("should parse");
        let answer = parse_answer(&completion.text).expect("should parse as an Answer");
        assert_eq!(answer.headline, "sample variance is 6.5, not 5.2");
        assert_eq!(
            answer.detail,
            "s^2 = sum((x-mean)^2)/(n-1) = 26/4 = 6.5. You divided by n instead of n-1."
        );
        // The fixture predates the difficulty field entirely.
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_parses_a_valid_difficulty() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"U\"}"}
        ]}"#;
        let completion = Anthropic::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.difficulty, Some(Difficulty::Ultra));
        assert_eq!(completion.stop, StopReason::Complete);
    }

    #[test]
    fn parse_completion_degrades_unparseable_difficulty_to_none_without_erroring() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"way too hard\"}"}
        ]}"#;
        let completion = Anthropic::parse_completion(body).expect("should still parse");
        let answer = parse_answer(&completion.text).expect("should still parse the answer");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_missing_difficulty_key_is_none() {
        let body = r#"{"stop_reason": "end_turn", "content": [
            {"type": "text", "text": "{\"detail\": \"d\", \"headline\": \"h\"}"}
        ]}"#;
        let completion = Anthropic::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_rejects_refusal() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response_refusal.json")
            .expect("fixture file should exist");
        let err = Anthropic::parse_completion(&body).unwrap_err();
        assert!(err.to_string().contains("refused"));
    }

    /// #155: `stop_reason: "max_tokens"` with only a `thinking` block and
    /// no `text` block -- the model spent its whole output-token budget on
    /// thinking. Must be a specific, actionable message, not the generic
    /// "no text block found" a genuinely malformed body gets.
    #[test]
    fn parse_completion_reports_budget_exhaustion_for_max_tokens_with_no_text() {
        let body = fs::read_to_string("tests/fixtures/anthropic_response_max_tokens.json")
            .expect("fixture file should exist");
        let err = Anthropic::parse_completion(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
        assert!(!msg.contains("no text block found"), "{msg}");
    }

    /// Neighbour of the above: a missing text block with a stop_reason
    /// other than `max_tokens` is genuinely malformed, not budget
    /// exhaustion, and must keep the generic message.
    #[test]
    fn parse_completion_rejects_missing_text_block_when_not_max_tokens() {
        let body = r#"{"stop_reason": "end_turn", "content": []}"#;
        let err = Anthropic::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no text block found"));
    }

    /// #731: stop reasons with no text block name themselves instead of
    /// blaming a missing block or the effort setting.
    #[test]
    fn parse_completion_names_other_stop_reasons_with_no_text() {
        for (reason, words) in [
            ("pause_turn", "paused"),
            ("model_context_window_exceeded", "too long"),
            ("some_new_reason", "some_new_reason"),
        ] {
            let body = format!(r#"{{"stop_reason":"{reason}","content":[]}}"#);
            let msg = Anthropic::parse_completion(&body).unwrap_err().to_string();
            assert!(msg.contains(reason), "{msg}");
            assert!(msg.contains(words), "{msg}");
            assert!(!msg.contains("ran out of room"), "{msg}");
            assert!(!msg.contains('\u{2014}'), "{msg}");
        }
    }

    /// #730: the usage block is readable from a reply that is not an answer.
    #[test]
    fn parse_usage_reads_a_max_tokens_body_with_no_text_block() {
        let body = r#"{"stop_reason":"max_tokens","content":[{"type":"thinking","thinking":"..."}],"usage":{"input_tokens":120,"output_tokens":4000}}"#;
        assert!(Anthropic::parse_completion(body).is_err());
        let u = Anthropic::parse_usage(body).unwrap();
        assert_eq!((u.input_tokens, u.output_tokens), (120, 4000));
        assert!(Anthropic::parse_usage("not json").is_none());
        assert!(Anthropic::parse_usage(r#"{"content":[]}"#).is_none());
    }

    #[test]
    fn parse_completion_rejects_invalid_json() {
        let err = Anthropic::parse_completion("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    #[test]
    fn effort_is_sent_for_models_that_accept_it() {
        let p = Anthropic::new("k", "claude-opus-5", "low");
        let body = p.build_body(&req("sys", false));
        assert_eq!(body["output_config"]["effort"], "low");
    }

    #[test]
    fn effort_is_omitted_for_models_that_reject_it() {
        // Sending effort to these is a 400, not a no-op.
        for model in ["claude-haiku-4-5", "claude-sonnet-4-5"] {
            let p = Anthropic::new("k", model, "low");
            let body = p.build_body(&req("sys", false));
            assert!(
                body["output_config"].get("effort").is_none(),
                "{model} must not carry effort"
            );
            // The schema must still be there — only effort is dropped.
            assert_eq!(body["output_config"]["format"]["type"], "json_schema");
        }
    }

    /// #719: the UI-only `editable` keyword never goes to the vendor.
    #[test]
    fn no_registered_schema_sends_the_editable_keyword() {
        let p = Anthropic::new("k", "claude-opus-5", "low");
        for kind in crate::actions::schema::PROPOSAL_KINDS {
            let mut r = req("sys", false);
            r.schema = crate::actions::schema::schema_for(kind, false);
            let body = serde_json::to_string(&p.build_body(&r)).unwrap();
            assert!(!body.contains("editable"), "{kind}: {body}");
        }
    }

    #[test]
    fn effort_allowlist_follows_family_and_version() {
        // MEASURED 2026-10-16 (test key): these accepted output_config.effort.
        for ok in [
            "claude-opus-4-5-20251101",
            "claude-opus-4-6",
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-opus-5-5",
            "claude-sonnet-4-6",
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5",
            "claude-fable-5-1",
        ] {
            assert!(supports_effort(ok), "{ok} should get effort");
        }
        // MEASURED 2026-10-16: Haiku 4.5 returns 400 "does not support the
        // effort parameter". The rest are older generations or ids we cannot
        // place; they are not sent effort (the safe side).
        for no in [
            "claude-haiku-4-5",
            "claude-haiku-4-5-20251001",
            "claude-sonnet-4-5",
            "claude-sonnet-4-5-20250929",
            "claude-sonnet-4-20250514",
            "claude-opus-4-20250514",
            "claude-opus-4-1-20250805",
            "claude-3-7-sonnet-20250219",
            "claude-3-5-haiku-20241022",
            "claude-3-opus-20240229",
            "custom-claude-model",
            "",
        ] {
            assert!(!supports_effort(no), "{no} must not get effort");
        }
    }

    #[test]
    fn older_models_get_no_effort_in_the_body() {
        for model in ["claude-opus-4-1-20250805", "claude-3-7-sonnet-20250219"] {
            let p = Anthropic::new("k", model, "low");
            let body = p.build_body(&req("sys", false));
            assert!(body["output_config"].get("effort").is_none(), "{model}");
        }
    }

    #[test]
    fn high_res_tier_starts_at_4_7_not_at_effort_support() {
        // Opus 4.5/4.6 and Sonnet 4.6 take effort but are on the standard
        // image tier (docs: high resolution is Claude 4.7 and later).
        for hi in [
            "claude-opus-4-7",
            "claude-opus-4-8",
            "claude-opus-5",
            "claude-sonnet-5",
            "claude-sonnet-5-5",
            "claude-haiku-5-5",
            "claude-fable-5-1",
        ] {
            assert!(supports_high_res_tier(hi), "{hi}");
        }
        for std in [
            "claude-opus-4-5-20251101",
            "claude-opus-4-6",
            "claude-sonnet-4-6",
            "claude-haiku-4-5",
            "claude-opus-4-1-20250805",
            "custom-claude-model",
        ] {
            assert!(!supports_high_res_tier(std), "{std}");
        }
    }

    #[test]
    fn empty_effort_is_never_sent() {
        let p = Anthropic::new("k", "claude-opus-5", "");
        let body = p.build_body(&req("sys", false));
        assert!(body["output_config"].get("effort").is_none());
    }

    /// #12 neighbour: a `Request::effort` override takes precedence over
    /// the provider's own configured default.
    #[test]
    fn request_effort_override_takes_precedence_over_configured_default() {
        let p = Anthropic::new("k", "claude-opus-5", "low");
        let mut r = req("sys", false);
        r.effort = Effort::High;
        let body = p.build_body(&r);
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// #12 neighbour: a request with no schema (a future plain-text action)
    /// must not fabricate a `format` key.
    #[test]
    fn no_schema_request_omits_output_format() {
        let p = Anthropic::new("k", "claude-opus-5", "low");
        let mut r = req("sys", false);
        r.schema = None;
        let body = p.build_body(&r);
        assert!(body["output_config"].get("format").is_none());
    }
}
