//! Shared helpers between `anthropic.rs` and `openai.rs` (#156). Both wrap a
//! different vendor API, so their request/response *shapes* stay separate —
//! only the genuinely identical boilerplate lives here: the HTTP send, the
//! image encoding, the answer JSON Schema and the difficulty-rubric prompt
//! append.
//!
//! `answer_schema`/`augmented_system_prompt` are physics-answer-specific
//! today (the only action Wingman has), but neither provider calls them any
//! more: `provider::physics_request` does, and hands the result over as
//! opaque `Request` fields (see the 2026-09-16 expansion plan's "Provider
//! trait, extended"). They stay in this file rather than `mod.rs` simply
//! because that's where the JSON-building imports already are.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use base64::Engine;
use serde_json::{json, Value};

/// POSTs `body` as JSON to `url` with `headers`, waits up to `timeout`, and
/// returns the raw response body text for any 2xx status.
///
/// `http_status_as_error(false)` is load-bearing: without it a non-2xx comes
/// back as an `Err` that has already discarded the body, and the body is
/// exactly what the caller needs to report (the API's own error message).
/// Both providers used to do this identically except for the `{tag}:`
/// prefix on the error text; `tag` is that prefix (e.g. `"anthropic"`).
pub(crate) fn post_json(
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
    timeout: Duration,
    tag: &str,
) -> Result<String> {
    let mut builder = ureq::post(url);
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }

    let mut response = builder
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(timeout))
        .build()
        .send_json(body)
        .map_err(|e| anyhow!("{tag}: transport error: {e}"))?;

    let status = response.status();
    let body_text = response
        .body_mut()
        .read_to_string()
        .with_context(|| format!("{tag}: failed to read response body"))?;

    if !status.is_success() {
        let truncated: String = body_text.chars().take(300).collect();
        return Err(anyhow!("{tag}: HTTP {status}: {truncated}"));
    }

    Ok(body_text)
}

/// Base64-encodes each image, in order. Both providers embed the screenshot
/// as base64 PNG, just inside different envelope shapes.
pub(crate) fn encode_images_base64(images: &[Vec<u8>]) -> Vec<String> {
    images
        .iter()
        .map(|png| base64::engine::general_purpose::STANDARD.encode(png))
        .collect()
}

/// The JSON Schema for the physics-check `Answer`: `detail` is listed (and
/// required) before `headline` deliberately -- with `headline` first the
/// model committed to a verdict before doing the arithmetic and then
/// contradicted itself (rule 3). `difficulty` goes last, after `headline`,
/// so the model rates the problem only once it has actually worked through
/// it rather than up front.
pub(crate) fn answer_schema(want_difficulty: bool) -> Value {
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
    json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

/// Appends [`super::DIFFICULTY_RUBRIC`] to `prompt` when requested, never
/// merging it into `super::DEFAULT_PROMPT` itself, so the user's own edited
/// prompt text in Settings is untouched.
pub(crate) fn augmented_system_prompt(prompt: &str, want_difficulty: bool) -> String {
    if want_difficulty {
        format!("{prompt}{}", super::DIFFICULTY_RUBRIC)
    } else {
        prompt.to_string()
    }
}
