use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use super::common;
use super::{Caps, Completion, Effort, ImageLimits, Provider, Request, StopReason};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(90);
const ENDPOINT: &str = "https://api.openai.com/v1/responses";
/// Used when `Request::max_tokens` is `0` (the caller has no opinion).
const DEFAULT_MAX_TOKENS: u32 = 2500;

pub struct OpenAi {
    pub api_key: String,
    pub model: String,
    /// The configured default, used when `Request::effort` is `Effort::Unset`.
    pub effort: Effort,
}

impl OpenAi {
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

    /// Builds the exact request body documented in the design spec's OpenAI
    /// section. Pure and network-free so it can be unit tested directly.
    ///
    /// The schema, instructions text and image bytes all come from `req`:
    /// this provider never builds the physics-check schema itself (#12; see
    /// the 2026-09-16 expansion plan's "Provider trait, extended").
    ///
    /// When `req` has no difficulty rubric folded into `system` this must
    /// stay byte-identical to the pre-#12 shape (see
    /// `build_body_is_unchanged_when_difficulty_off`).
    fn build_body(&self, req: &Request) -> Value {
        let mut content = vec![json!({"type": "input_text", "text": req.user})];
        for b64 in common::encode_images_base64(&req.images) {
            content.push(json!({
                "type": "input_image",
                "image_url": format!("data:image/png;base64,{b64}"),
                "detail": "high"
            }));
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
            "instructions": req.system,
            "input": [{"role": "user", "content": content}],
            "reasoning": {"effort": effort.as_str().unwrap_or("")},
            "max_output_tokens": max_tokens,
        });

        if let Some(schema) = &req.schema {
            body["text"] = json!({"format": {
                "type": "json_schema", "name": "answer", "strict": true,
                "schema": schema
            }});
        }

        // Two independent reasons to drop `reasoning` entirely, mirrored
        // from Anthropic's `supports_effort` gate:
        // - `effort.as_str().is_none()`: `Unset` is a plausible hand-edit
        //   of config.toml (`effort = ""` meaning "use the default"), and
        //   the Responses API validates `reasoning.effort` against a fixed
        //   enum -- sending an empty string is a hard 400 on every request,
        //   not a no-op (#154).
        // - `!supports_reasoning(&self.model)`: `gpt-4.1` (shipped in
        //   `Providers::default`'s OpenAI model list, config.rs) predates
        //   `reasoning.effort` on the Responses API (#167). By analogy with
        //   Anthropic's 4.5-generation 400 (MEASURED 2026-09-15, rule 10),
        //   sending it here is assumed unsupported -- THEORY (unverified):
        //   no live OpenAI call has confirmed whether this 400s or is
        //   silently ignored; replace this comment with MEASURED when it
        //   is checked.
        if effort.as_str().is_none() || !supports_reasoning(&self.model) {
            body.as_object_mut()
                .expect("body is always an object")
                .remove("reasoning");
        }

        body
    }

    /// Parses a successful (2xx) OpenAI Responses API body into a
    /// `Completion`. The text is returned as-is -- this provider never
    /// interprets it against a schema (#12).
    ///
    /// The `output` array may contain a `reasoning` entry before the
    /// `message` entry on reasoning models, so non-`message` entries are
    /// skipped rather than assumed absent.
    fn parse_completion(body: &str) -> Result<Completion> {
        let value: Value =
            serde_json::from_str(body).context("openai: response body is not valid JSON")?;

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
                    // Mirrors `anthropic.rs`'s `stop_reason: "refusal"` and
                    // `openai_compat.rs`'s `message.refusal` handling: the
                    // Responses API can carry a `type: "refusal"` content
                    // part in place of a `text` part (THEORY (unverified
                    // against a live API): no live OpenAI Responses call
                    // has confirmed this shape; see #254). A refusal must
                    // surface as itself, not as "no message text found",
                    // and must win even if an earlier part in the same
                    // message carried partial text.
                    if let Some(refusal) = part.get("refusal").and_then(Value::as_str) {
                        if !refusal.is_empty() {
                            return Err(anyhow!("openai: model refused to answer"));
                        }
                    }
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        text.push_str(t);
                    }
                }
            }
        }

        let status = value.get("status").and_then(Value::as_str);

        if text.is_empty() {
            // `status: "incomplete"` with no `message` entry means the
            // model spent its whole output-token budget on reasoning and
            // never got to emit the answer -- a specific, reachable shape
            // (see `tests/fixtures/openai_response_no_message.json`), not
            // a generic malformed response (#155).
            if status == Some("incomplete") {
                return Err(anyhow!(
                    "openai: The model ran out of room before answering. Lower the effort setting or raise the token limit."
                ));
            }
            return Err(anyhow!("openai: no message text found in output[]"));
        }

        let stop = match status {
            Some("incomplete") => StopReason::MaxTokens,
            Some("completed") => StopReason::Complete,
            _ => StopReason::Other,
        };

        Ok(Completion {
            text,
            usage: None,
            stop,
        })
    }
}

/// Whether a model accepts `reasoning.effort` on the Responses API.
///
/// #167: `gpt-4.1` (shipped in `Providers::default`'s OpenAI model list,
/// config.rs) predates the reasoning-effort parameter -- it is not a
/// reasoning model, unlike the gpt-5.x/o* lines. By analogy with
/// Anthropic's `supports_effort` gate (a hard 400 on unsupported models,
/// MEASURED 2026-09-15, AGENTS.md rule 10), `gpt-4.1` is carved out here
/// too.
///
/// THEORY (unverified): no live OpenAI call has confirmed whether sending
/// `reasoning.effort` to `gpt-4.1` actually 400s, is silently ignored, or
/// errors some other way -- this task explicitly does not make live OpenAI
/// calls. Replace this comment with `MEASURED <date>:` once checked, per
/// #167's "Done when".
///
/// Also used as this provider's `thinking` capability, mirroring
/// Anthropic's `supports_effort` doing double duty the same way.
fn supports_reasoning(model: &str) -> bool {
    const NO_REASONING: [&str; 1] = ["gpt-4.1"];
    !NO_REASONING.iter().any(|m| model.starts_with(m))
}

impl Provider for OpenAi {
    fn id(&self) -> &'static str {
        "openai"
    }

    fn ready(&self) -> bool {
        !self.api_key.trim().is_empty()
    }

    /// Issue #169: `image_limits` is `capture.rs`'s `OPENAI_TILE_*` budget
    /// for every model -- `build_body` always sends `detail: "high"` (see
    /// above), and every model currently offered is a Responses-API model
    /// on that same tile pipeline, so this is not model-dependent the way
    /// Anthropic's tiers are.
    fn capabilities(&self, model: &str) -> Caps {
        // Every model currently offered in Settings (see `Providers::default`
        // in config.rs) is a Responses-API model with vision and strict
        // json_schema support. `reasoning.effort` support varies by model --
        // see `supports_reasoning` (#167).
        Caps {
            vision: true,
            json_schema: true,
            thinking: supports_reasoning(model),
            image_limits: Some(ImageLimits {
                max_long_edge: crate::capture::OPENAI_TILE_MAX_LONG_EDGE,
                max_pixels: crate::capture::OPENAI_TILE_MAX_PIXELS,
            }),
        }
    }

    fn own_caps(&self) -> Caps {
        self.capabilities(&self.model)
    }

    fn complete(&self, req: &Request) -> Result<Completion> {
        let body = self.build_body(req);

        let auth = format!("Bearer {}", self.api_key);
        let body_text = common::post_json(
            ENDPOINT,
            &[
                ("Authorization", auth.as_str()),
                ("Content-Type", "application/json"),
            ],
            &body,
            REQUEST_TIMEOUT,
            "openai",
        )?;

        Self::parse_completion(&body_text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{parse_answer, physics_request, Shot};
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
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&req("system prompt text", false));

        let expected_data_url = format!(
            "data:image/png;base64,{}",
            base64::Engine::encode(
                &base64::engine::general_purpose::STANDARD,
                &sample_shot().png
            )
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
        let with_flag = provider.build_body(&req("system prompt text", false));
        let today = json!({
            "model": "gpt-5.5",
            "instructions": "system prompt text",
            "input": [{"role": "user", "content": [
                {"type": "input_text", "text": "Check my working."},
                {"type": "input_image", "image_url": format!(
                    "data:image/png;base64,{}",
                    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &sample_shot().png)
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
        let body = provider.build_body(&req("system prompt text", true));

        let schema = &body["text"]["format"]["schema"];
        assert_eq!(
            schema["properties"]["difficulty"],
            json!({"type": "string", "enum": ["1","2","3","4","5","6","7","8","9","10","U","N"]})
        );
        assert_eq!(
            schema["required"],
            json!(["detail", "headline", "difficulty"])
        );

        // The rubric is appended, not merged into the caller's prompt text.
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("system prompt text"));
        assert!(instructions.contains("difficulty"));
        assert!(instructions.len() > "system prompt text".len());
    }

    // #156: see the identical comment in `anthropic.rs` -- `assert_eq!` on
    // two `Value`s ignores key order under `preserve_order`, so only a
    // serialized-string comparison proves the refactor is byte-identical.
    #[test]
    fn golden_request_body_no_difficulty_is_byte_identical() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&req("system prompt text", false));
        let golden = r#"{"model":"gpt-5.5","instructions":"system prompt text","input":[{"role":"user","content":[{"type":"input_text","text":"Check my working."},{"type":"input_image","image_url":"data:image/png;base64,iVBORw==","detail":"high"}]}],"reasoning":{"effort":"low"},"max_output_tokens":2500,"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"}},"required":["detail","headline"],"additionalProperties":false}}}}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
    }

    #[test]
    fn golden_request_body_with_difficulty_is_byte_identical() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&req("system prompt text", true));
        let golden = r#"{"model":"gpt-5.5","instructions":"system prompt text\n\nAlso rate how difficult the PROBLEM ON SCREEN is for a HUMAN STUDENT. Add a third field:\n- difficulty: THIRD, after headline, once you have actually worked the problem through. Exactly one of \"1\" through \"10\", or \"U\".\n\nCalibration is the hard part, so read this carefully. You solve nearly all of these easily; that is NOT the scale. Do not rate your own confidence, your own effort, or how quickly you found the answer. Rate how hard the problem would be for a student at the level it is aimed at. Rating by your own effort compresses everything into 1-5 and makes the whole scale useless.\n\nAnchors:\n1 = an easy high-school question. One step, one formula. (speed = distance / time)\n2 = high-school, a couple of steps.\n3 = easy university intro-course level. (a block on an incline; moment of inertia of a disk)\n4 = intro university, several steps or a small subtlety.\n5 = medium university level. Mid-degree material: multi-step, and you must choose the method rather than being told it.\n6 = upper-undergraduate, harder than routine homework.\n7 = hard university level. Typically GRADUATE coursework: quantum perturbation theory, Lagrangian mechanics with constraints, a non-obvious statistical derivation.\n8 = graduate coursework that most of the class would get wrong.\n9 = very hard for an undergraduate. Qualifying-exam standard.\n10 = a PhD student in the field would struggle. Open-ended derivations and proofs requiring a specialist technique, not just more algebra.\nU = Ultra: a professor would struggle. Research-level, or a known-hard proof.\n\nUse the WHOLE range. Most routine homework is 2-5. If the problem is recognisably graduate-level, it starts at 7, not 5. If it asks you to PROVE a general theorem rather than compute a value, it is almost never below 8.\n\nIf there is no problem to rate at all -- the screen shows no question, or you are asking for something to be made visible -- answer \"N\". Do NOT reach for \"U\" in that case: \"U\" means the problem is extraordinarily hard, not that you could not find one.","input":[{"role":"user","content":[{"type":"input_text","text":"Check my working."},{"type":"input_image","image_url":"data:image/png;base64,iVBORw==","detail":"high"}]}],"reasoning":{"effort":"low"},"max_output_tokens":2500,"text":{"format":{"type":"json_schema","name":"answer","strict":true,"schema":{"type":"object","properties":{"detail":{"type":"string"},"headline":{"type":"string"},"difficulty":{"type":"string","enum":["1","2","3","4","5","6","7","8","9","10","U","N"]}},"required":["detail","headline","difficulty"],"additionalProperties":false}}}}"#;
        assert_eq!(serde_json::to_string(&body).unwrap(), golden);
    }

    #[test]
    fn ready_reflects_api_key_presence() {
        assert!(OpenAi::new("sk-real", "gpt-5.5", "low").ready());
        assert!(!OpenAi::new("", "gpt-5.5", "low").ready());
        assert!(!OpenAi::new("   ", "gpt-5.5", "low").ready());
    }

    #[test]
    fn capabilities_report_vision_json_schema_and_thinking() {
        let provider = OpenAi::new("k", "gpt-5.5", "low");
        let caps = provider.capabilities("gpt-5.5");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(caps.thinking);
    }

    /// #167: `gpt-4.1` is not a reasoning model. `capabilities().thinking`
    /// must say so, mirroring Anthropic's per-model
    /// `capabilities_report_vision_and_json_schema_always_and_thinking_per_model`.
    #[test]
    fn capabilities_reports_no_thinking_for_gpt_4_1() {
        let provider = OpenAi::new("k", "gpt-4.1", "low");
        let caps = provider.capabilities("gpt-4.1");
        assert!(caps.vision);
        assert!(caps.json_schema);
        assert!(!caps.thinking);
    }

    // -- image_limits (issue #169) ---------------------------------------

    #[test]
    fn capabilities_reports_the_tile_image_budget_regardless_of_model() {
        let provider = OpenAi::new("k", "gpt-5.5", "low");
        for model in ["gpt-5.5", "gpt-4.1"] {
            let limits = provider
                .capabilities(model)
                .image_limits
                .unwrap_or_else(|| panic!("expected image_limits for {model}"));
            assert_eq!(
                limits.max_long_edge,
                crate::capture::OPENAI_TILE_MAX_LONG_EDGE
            );
            assert_eq!(limits.max_pixels, crate::capture::OPENAI_TILE_MAX_PIXELS);
        }
    }

    #[test]
    fn own_caps_matches_capabilities_for_the_configured_model() {
        let provider = OpenAi::new("k", "gpt-4.1", "low");
        assert_eq!(provider.own_caps(), provider.capabilities("gpt-4.1"));
    }

    #[test]
    fn parse_completion_skips_reasoning_entry_and_extracts_message() {
        let body = fs::read_to_string("tests/fixtures/openai_response.json")
            .expect("fixture file should exist");
        let completion = OpenAi::parse_completion(&body).expect("should parse");
        let answer = parse_answer(&completion.text).expect("should parse as an Answer");
        assert_eq!(answer.headline, "42 m/s is correct");
        assert_eq!(
            answer.detail,
            "v = u + at = 0 + 9.8*4.3 = 42.1, rounds to 42."
        );
        // The fixture predates the difficulty field entirely.
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_parses_a_valid_difficulty() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"7\"}"}
        ]}]}"#;
        let completion = OpenAi::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(
            answer.difficulty,
            Some(crate::provider::Difficulty::Level(7))
        );
    }

    #[test]
    fn parse_completion_degrades_unparseable_difficulty_to_none_without_erroring() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\", \"difficulty\": \"not-a-level\"}"}
        ]}]}"#;
        let completion = OpenAi::parse_completion(body).expect("should still parse");
        let answer = parse_answer(&completion.text).expect("should still parse the answer");
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_missing_difficulty_key_is_none() {
        let body = r#"{"output": [{"type": "message", "content": [
            {"text": "{\"detail\": \"d\", \"headline\": \"h\"}"}
        ]}]}"#;
        let completion = OpenAi::parse_completion(body).expect("should parse");
        let answer = parse_answer(&completion.text).unwrap();
        assert_eq!(answer.difficulty, None);
    }

    #[test]
    fn parse_completion_rejects_invalid_json() {
        let err = OpenAi::parse_completion("not json").unwrap_err();
        assert!(err.to_string().contains("not valid JSON"));
    }

    /// #155: `openai_response_no_message.json` is `status: "incomplete"`
    /// with only a `reasoning` entry (empty summary) and no `message` at
    /// all -- the model spent its whole output-token budget on reasoning.
    /// That must surface as an actionable, specific message, not the
    /// generic "no message text found" a genuinely malformed body gets.
    #[test]
    fn parse_completion_reports_budget_exhaustion_for_incomplete_status_with_no_message() {
        let body = fs::read_to_string("tests/fixtures/openai_response_no_message.json")
            .expect("fixture file should exist");
        let err = OpenAi::parse_completion(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("ran out of room"), "{msg}");
        assert!(!msg.contains("no message text"), "{msg}");
    }

    /// Neighbour of the above: a body with no message and no `status:
    /// "incomplete"` is a genuinely malformed response, not budget
    /// exhaustion, and must keep the generic message.
    #[test]
    fn parse_completion_rejects_body_with_no_message_and_no_incomplete_status() {
        let body = r#"{"output": []}"#;
        let err = OpenAi::parse_completion(body).unwrap_err();
        assert!(err.to_string().contains("no message text"));
    }

    /// #254: the Responses API's `output[].content[]` can carry a
    /// `type: "refusal"` part with a `refusal` string instead of a `text`
    /// part. Mirrors `anthropic.rs`'s `parse_completion_rejects_refusal`
    /// and `openai_compat.rs`'s `parse_completion_rejects_a_refusal`: a
    /// refusal must surface as "model refused to answer", not the generic
    /// "no message text found in output[]" a genuinely malformed body
    /// gets.
    #[test]
    fn parse_completion_rejects_a_refusal() {
        let body = fs::read_to_string("tests/fixtures/openai_response_refusal.json")
            .expect("fixture file should exist");
        let err = OpenAi::parse_completion(&body).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("refused"), "{msg}");
        assert!(!msg.contains("no message text"), "{msg}");
    }

    /// Neighbour: a `content[]` mixing a `text` part with a `refusal` part
    /// in the same message. The refusal must still be reported rather than
    /// silently dropped in favour of the partial text.
    #[test]
    fn parse_completion_rejects_a_refusal_mixed_with_text() {
        let body = fs::read_to_string("tests/fixtures/openai_response_refusal_mixed.json")
            .expect("fixture file should exist");
        let err = OpenAi::parse_completion(&body).unwrap_err();
        assert!(err.to_string().contains("refused"));
    }

    /// #154: mirrors Anthropic's `empty_effort_is_never_sent` -- an empty
    /// `effort` string is a plausible hand-edit of config.toml and must
    /// never be sent verbatim (OpenAI's Responses API 400s on it).
    #[test]
    fn empty_effort_is_never_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "");
        let body = provider.build_body(&req("sys", false));
        assert!(body.get("reasoning").is_none());
    }

    /// Neighbour: whitespace-only is the same footgun as empty.
    #[test]
    fn whitespace_only_effort_is_never_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "   ");
        let body = provider.build_body(&req("sys", false));
        assert!(body.get("reasoning").is_none());
    }

    /// Neighbour: a real effort value must still be sent (not swallowed by
    /// the guard).
    #[test]
    fn non_empty_effort_is_still_sent() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let body = provider.build_body(&req("sys", false));
        assert_eq!(body["reasoning"]["effort"], "low");
    }

    /// #167: `gpt-4.1` predates `reasoning.effort` on the Responses API
    /// (it is not a reasoning model, unlike the gpt-5.x/o* lines). Sending
    /// it a non-empty configured effort must not add the `reasoning` key --
    /// mirrors Anthropic's `effort_is_omitted_for_models_that_reject_it`.
    #[test]
    fn reasoning_is_omitted_for_models_that_do_not_support_it() {
        let provider = OpenAi::new("sk-test", "gpt-4.1", "low");
        let body = provider.build_body(&req("sys", false));
        assert!(
            body.get("reasoning").is_none(),
            "gpt-4.1 must not carry reasoning.effort"
        );
        // The schema must still be there -- only reasoning is dropped.
        assert!(body.get("text").is_some());
    }

    /// Neighbour: a model that does support it must still get it, i.e. the
    /// new gate doesn't accidentally swallow the gpt-5.x/o* lines too.
    #[test]
    fn reasoning_is_still_sent_for_models_that_support_it() {
        for model in ["gpt-5.5", "gpt-5", "gpt-5-mini", "o3"] {
            let provider = OpenAi::new("sk-test", model, "low");
            let body = provider.build_body(&req("sys", false));
            assert_eq!(
                body["reasoning"]["effort"], "low",
                "{model} should still send reasoning.effort"
            );
        }
    }

    /// #12 neighbour: a `Request::effort` override takes precedence over
    /// the provider's own configured default.
    #[test]
    fn request_effort_override_takes_precedence_over_configured_default() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let mut r = req("sys", false);
        r.effort = Effort::High;
        let body = provider.build_body(&r);
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    /// #12 neighbour: a request with no schema (a future plain-text action)
    /// must not fabricate a `text` key.
    #[test]
    fn no_schema_request_omits_text_format() {
        let provider = OpenAi::new("sk-test", "gpt-5.5", "low");
        let mut r = req("sys", false);
        r.schema = None;
        let body = provider.build_body(&r);
        assert!(body.get("text").is_none());
    }
}
