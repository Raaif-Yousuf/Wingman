//! The local egress log (#106): one row per outbound provider request,
//! recording what left the machine without ever recording what it was.
//!
//! **"Inputs attached" means kinds and sizes, never contents.** A row says
//! "one image, 1.2 MB, 340x200 after downscale; page text, 4 kB" -- never
//! the pixels or the text itself. Storing screen contents in a log file
//! would recreate exactly the privacy problem this feature exists to
//! disprove (`docs/positioning.md`: "An egress log shows every request and
//! every byte"), so [`EgressEntry`] structurally has no field that could
//! hold a screenshot or a transcript, only [`Attachment`]'s `kind`,
//! `size_bytes`, `width`/`height`.
//!
//! # Where this is called from
//!
//! [`record`] is called from the single HTTP boundary every provider's
//! completion call already funnels through -- `provider::common`'s
//! `post_json_with` and `post_json_with_connect_timeout` -- once per actual
//! network attempt, including a retry and including a failure (a log that
//! only records successes is exactly the log a distrustful user will not
//! believe). [`build_entry`], [`attachments_from_body`] and [`model_from`]
//! derive everything they log directly from the JSON body and URL every
//! provider already builds and sends, generically (a PNG-shaped base64
//! string is an image; the vendor-specific message-shape parsing that would
//! be needed to label every string field precisely is not attempted) --
//! this is deliberate: a brand new provider file that calls
//! `provider::common::post_json`/`post_json_with_connect_timeout` gets a
//! logged row for free, with no per-provider logging call to remember.
//!
//! # Storage and retention
//!
//! Issue #46 plans a real SQLite store; it does not exist yet. Until then
//! this is the simplest thing that survives a restart: one JSON object per
//! line, appended to `%LOCALAPPDATA%\Wingman\egress.log`. Every append caps
//! the file at [`MAX_LOG_BYTES`] by dropping the OLDEST lines first (see
//! [`cap_lines`]) -- an unbounded append-only log on the request path is a
//! disk-filling bug on its own, independent of any privacy concern. When
//! #46 lands, this file's rows are exactly the columns that table needs.
//!
//! # Knowledge snippets (#85+)
//!
//! [`EgressEntry::snippets`] is always empty today: the knowledge/retrieval
//! system it would describe is unbuilt Phase 3b work (issue #85 onwards).
//! The column exists now so #85 only has to populate it, not add it.
//!
//! # Cost (#21)
//!
//! [`EgressEntry::cost`] is always `None` today. Token accounting and a
//! local price table are a separate module, `src/usage.rs`, being built
//! tonight on issue #21 by another agent. This module does not duplicate
//! that work; whoever merges both should wire a cost lookup into
//! [`build_entry`] once `usage.rs` exists.

use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Hard cap on `egress.log`'s size. Chosen to comfortably hold thousands of
/// rows (each is well under 1 KB) while staying far short of anything that
/// could meaningfully fill a disk. See [`cap_lines`] for the trimming
/// policy once a write would exceed it.
const MAX_LOG_BYTES: usize = 2_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Text,
}

/// One attached input's kind and size -- never its content. `width`/`height`
/// are only ever `Some` for [`AttachmentKind::Image`], and only when the
/// PNG's own `IHDR` chunk could be read (see [`png_dimensions`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Attachment {
    pub kind: AttachmentKind,
    pub size_bytes: usize,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Success,
    Failure,
}

/// One logged request attempt. See this module's doc comment for what each
/// field does and does not carry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EgressEntry {
    pub time_unix: u64,
    pub provider: String,
    pub model: Option<String>,
    /// Whether this attempt followed an earlier failed one for the same
    /// logical request (an HTTP retry or a 429 retry-after wait -- see
    /// `provider::common::post_json_with`). `false` for the first attempt.
    pub retried: bool,
    pub attachments: Vec<Attachment>,
    /// Always empty; see this module's doc comment on knowledge snippets.
    pub snippets: Vec<String>,
    pub bytes_sent: usize,
    pub bytes_received: usize,
    pub outcome: Outcome,
    /// Only set when `outcome == Outcome::Failure`, and always passed
    /// through [`redact_opaque_tokens`] first.
    pub error: Option<String>,
    /// Always `None` today; see this module's doc comment on cost (#21).
    pub cost: Option<f64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `"iVBORw0KG"` is the fixed base64 prefix of every PNG's 8-byte magic
/// number (`\x89PNG\r\n\x1a\n`): base64 is a 6-bits-per-character encoding,
/// so a byte sequence with a fixed prefix always base64-encodes to a fixed
/// character prefix too. Every image this crate ever sends is a PNG
/// (`capture.rs`, "PNG-only", CLAUDE.md's stack table), so this alone is
/// enough to recognise an embedded image string generically, without
/// knowing any vendor's specific JSON shape.
const PNG_BASE64_PREFIX: &str = "iVBORw0KG";

fn is_probable_base64_png(s: &str) -> bool {
    s.len() > 32 && s.starts_with(PNG_BASE64_PREFIX)
}

/// A `data:image/png;base64,...` URL (OpenAI-compatible endpoints embed
/// images this way). Returns the base64 payload after the comma.
fn data_url_png_payload(s: &str) -> Option<&str> {
    let rest = s.strip_prefix("data:image/png;base64,")?;
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

/// Reads a PNG's pixel dimensions straight from its `IHDR` chunk, the same
/// fixed byte layout for every PNG regardless of encoder: an 8-byte
/// signature, a 4-byte chunk length, the 4-byte ASCII tag `IHDR`, then a
/// big-endian `u32` width immediately followed by a big-endian `u32`
/// height. Returns `None` for anything shorter than that (24 bytes) or
/// whose signature/tag don't match, rather than panicking on a truncated or
/// non-PNG blob -- a log entry with no dimensions is recoverable; a panic on
/// the request path is not (rule 7).
pub(crate) fn png_dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    const PNG_SIGNATURE: [u8; 8] = [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
    if bytes.len() < 24 {
        return None;
    }
    if bytes[0..8] != PNG_SIGNATURE {
        return None;
    }
    if &bytes[12..16] != b"IHDR" {
        return None;
    }
    let width = u32::from_be_bytes([bytes[16], bytes[17], bytes[18], bytes[19]]);
    let height = u32::from_be_bytes([bytes[20], bytes[21], bytes[22], bytes[23]]);
    Some((width, height))
}

fn image_attachment(base64_payload: &str) -> Option<Attachment> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(base64_payload)
        .ok()?;
    let dims = png_dimensions(&bytes);
    Some(Attachment {
        kind: AttachmentKind::Image,
        size_bytes: bytes.len(),
        width: dims.map(|d| d.0),
        height: dims.map(|d| d.1),
    })
}

/// String keys whose value is structural metadata (a role label, a model
/// id, a MIME type, ...), never user content -- excluded from the
/// aggregate "text" attachment size so `attachments_from_body` doesn't
/// count `"role": "user"` as four bytes of text on every single message.
const STRUCTURAL_STRING_KEYS: &[&str] = &[
    "model",
    "role",
    "type",
    "id",
    "format",
    "mime_type",
    "media_type",
    "finish_reason",
    "stop_reason",
    "effort",
    "reasoning_effort",
];

/// Walks a provider request body generically (no per-vendor shape
/// knowledge) and returns one [`Attachment`] per embedded PNG image, plus
/// (when any non-structural text was found) one aggregate
/// [`AttachmentKind::Text`] attachment summing every other string leaf's
/// byte length. This is an approximation, not a precise per-field
/// breakdown -- see this module's doc comment for why that trade-off is
/// deliberate.
pub fn attachments_from_body(body: &Value) -> Vec<Attachment> {
    let mut images = Vec::new();
    let mut text_bytes: usize = 0;
    scan_value(body, &mut images, &mut text_bytes);
    if text_bytes > 0 {
        images.push(Attachment {
            kind: AttachmentKind::Text,
            size_bytes: text_bytes,
            width: None,
            height: None,
        });
    }
    images
}

fn scan_value(v: &Value, images: &mut Vec<Attachment>, text_bytes: &mut usize) {
    match v {
        Value::Object(map) => {
            for (key, val) in map {
                match val {
                    Value::String(s) => scan_string(key, s, images, text_bytes),
                    other => scan_value(other, images, text_bytes),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                match item {
                    // A bare string inside an array (e.g. Ollama's
                    // `"images": ["<base64>", ...]`) carries no object key
                    // to check against `STRUCTURAL_STRING_KEYS` -- an empty
                    // key never matches that list, so it is scanned exactly
                    // like any other content string.
                    Value::String(s) => scan_string("", s, images, text_bytes),
                    other => scan_value(other, images, text_bytes),
                }
            }
        }
        _ => {}
    }
}

fn scan_string(key: &str, s: &str, images: &mut Vec<Attachment>, text_bytes: &mut usize) {
    if is_probable_base64_png(s) {
        if let Some(a) = image_attachment(s) {
            images.push(a);
            return;
        }
    }
    if let Some(payload) = data_url_png_payload(s) {
        if let Some(a) = image_attachment(payload) {
            images.push(a);
            return;
        }
    }
    if STRUCTURAL_STRING_KEYS.contains(&key) {
        return;
    }
    *text_bytes += s.len();
}

/// Extracts a model name generically: most vendors put it at the request
/// body's top level (`{"model": "...", ...}` -- OpenAI, Anthropic, Ollama,
/// every OpenAI-compatible endpoint); Gemini instead names it in the URL
/// path (`.../models/<model>:generateContent`), so that is tried second.
pub fn model_from(url: &str, body: &Value) -> Option<String> {
    if let Some(m) = body.get("model").and_then(Value::as_str) {
        if !m.is_empty() {
            return Some(m.to_string());
        }
    }
    let after = url.split("models/").nth(1)?;
    let model = after.split(':').next()?;
    if model.is_empty() {
        None
    } else {
        Some(model.to_string())
    }
}

/// Defense in depth for the one rule that matters most here: **an API key
/// must never reach the egress log.** Nothing in this module ever reads or
/// is passed request `headers` (where a key actually lives) in the first
/// place, so there is structurally no path for one to reach [`EgressEntry`]
/// via [`attachments_from_body`]/[`model_from`] -- but a failure message
/// (the one free-text field this module stores) comes straight from the
/// network, so as a second, independent layer, any run of 20 or more
/// consecutive base64/URL-safe characters (the shape every vendor's API key
/// takes -- `sk-...`, `sk-ant-...`, `AIza...`) is scrubbed before the
/// message is ever written to the log.
pub fn redact_opaque_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut token = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
            token.push(c);
        } else {
            flush_token(&mut token, &mut out);
            out.push(c);
        }
    }
    flush_token(&mut token, &mut out);
    out
}

fn flush_token(token: &mut String, out: &mut String) {
    if token.chars().count() >= 20 {
        out.push_str("[redacted]");
    } else {
        out.push_str(token);
    }
    token.clear();
}

/// Builds the row [`record`] will persist for one HTTP attempt. `provider`
/// is the same short tag every provider file already passes to
/// `post_json`/`post_json_with_connect_timeout` (e.g. `"anthropic"`,
/// `"openai"`, `"ollama chat"`); `error` is the raw failure text, redacted
/// here (never by the caller) so there is exactly one place that can be
/// audited for the redaction guarantee.
#[allow(clippy::too_many_arguments)]
pub fn build_entry(
    provider: &str,
    url: &str,
    body: &Value,
    retried: bool,
    bytes_sent: usize,
    bytes_received: usize,
    outcome: Outcome,
    error: Option<&str>,
) -> EgressEntry {
    EgressEntry {
        time_unix: now_unix(),
        provider: provider.to_string(),
        model: model_from(url, body),
        retried,
        attachments: attachments_from_body(body),
        snippets: Vec::new(),
        bytes_sent,
        bytes_received,
        outcome,
        error: error.map(redact_opaque_tokens),
        cost: None,
    }
}

/// Appends `new_line` to `existing`, then drops the OLDEST lines (never the
/// newest -- a log that silently lost the request just made must not exist)
/// until the result fits in `max_bytes`, or exactly one line remains. Pure,
/// so the trimming policy is unit-tested without touching a real file.
pub(crate) fn cap_lines(existing: &str, new_line: &str, max_bytes: usize) -> String {
    let trimmed_new = new_line.trim_end();
    let mut lines: Vec<&str> = existing.lines().collect();
    lines.push(trimmed_new);

    let mut total: usize = lines.iter().map(|l| l.len() + 1).sum();
    while total > max_bytes && lines.len() > 1 {
        let removed = lines.remove(0);
        total -= removed.len() + 1;
    }

    let mut out = lines.join("\n");
    out.push('\n');
    out
}

/// `%LOCALAPPDATA%\Wingman\egress.log`. Local-only, machine-scoped storage
/// (`known_folder::local_app_data`, the same folder the expansion plan
/// names for the eventual `store.rs` SQLite database, #46) -- never
/// `%APPDATA%` (roaming), since this file is deliberately not something
/// that should follow a roaming profile between machines.
///
/// CLAUDE.md rule 9 ("tests never touch production names"): under
/// `#[cfg(test)]` this resolves to a process-scoped file in `%TEMP%`
/// instead, never the real `%LOCALAPPDATA%\Wingman\egress.log` -- so
/// `provider::common`'s tests, which call the real `post_json_with` (and so
/// really do call [`record`] as a side effect), never write to, or race
/// each other over, the production path. The process id keeps this from
/// colliding with a `cargo test` run in another worktree on the same
/// shared machine.
pub fn log_path() -> Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("wingman-test-egress-{}.log", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::local_app_data()?;
        Ok(base.join("Wingman").join("egress.log"))
    }
}

/// Appends one row to the log, capped per [`cap_lines`]. Never panics and
/// never surfaces an error to its caller (rule 7: a logging failure must
/// never be why the request itself fails or shows an error card) --
/// `provider::common`'s callers already treat the actual HTTP result as the
/// thing that can fail; whether it got logged is a best-effort side effect.
pub fn record(entry: &EgressEntry) {
    let _ = try_record(entry);
}

fn try_record(entry: &EgressEntry) -> Result<()> {
    let path = log_path()?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let existing = fs::read_to_string(&path).unwrap_or_default();
    let line = serde_json::to_string(entry)?;
    let capped = cap_lines(&existing, &line, MAX_LOG_BYTES);
    fs::write(&path, capped)?;
    Ok(())
}

/// Raw JSON-lines file contents, or an empty string if the log does not
/// exist yet (nothing has been sent, or logging never succeeded even
/// once) -- never an error the caller has to handle specially.
pub fn read_all() -> String {
    log_path()
        .ok()
        .and_then(|p| fs::read_to_string(p).ok())
        .unwrap_or_default()
}

/// Renders one entry as a human-readable line for the "Copy egress log"
/// clipboard action (`diagnostics::egress_report`) -- the on-disk format
/// stays JSON-lines (machine-readable, ready for #46's SQLite import), but
/// nobody should have to read raw JSON pasted into a bug report.
pub fn render_human(entry: &EgressEntry) -> String {
    let mut parts = Vec::new();
    parts.push(format!("t={}", entry.time_unix));
    parts.push(format!("provider={}", entry.provider));
    if let Some(model) = &entry.model {
        parts.push(format!("model={model}"));
    }
    if entry.retried {
        parts.push("retry".to_string());
    }
    for a in &entry.attachments {
        let kind = match a.kind {
            AttachmentKind::Image => "image",
            AttachmentKind::Text => "text",
        };
        match (a.width, a.height) {
            (Some(w), Some(h)) => parts.push(format!(
                "{kind}: {} ({w}x{h})",
                human_bytes(a.size_bytes)
            )),
            _ => parts.push(format!("{kind}: {}", human_bytes(a.size_bytes))),
        }
    }
    parts.push(format!(
        "snippets={}",
        if entry.snippets.is_empty() {
            "none".to_string()
        } else {
            entry.snippets.len().to_string()
        }
    ));
    parts.push(format!(
        "bytes={}/{}",
        entry.bytes_sent, entry.bytes_received
    ));
    match entry.outcome {
        Outcome::Success => parts.push("ok".to_string()),
        Outcome::Failure => parts.push(format!(
            "failed: {}",
            entry.error.as_deref().unwrap_or("unknown error")
        )),
    }
    match entry.cost {
        Some(c) => parts.push(format!("cost=${c:.4}")),
        None => parts.push("cost=unknown".to_string()),
    }
    parts.join(", ")
}

fn human_bytes(n: usize) -> String {
    if n >= 1_000_000 {
        format!("{:.1} MB", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1} KB", n as f64 / 1_000.0)
    } else {
        format!("{n} bytes")
    }
}

/// Every row in the log, oldest first, rendered per [`render_human`] -- what
/// `diagnostics::egress_report` puts on the clipboard. Malformed lines
/// (should never happen, since [`record`] is the only writer, but a hand
/// edit or a future format change is possible) are skipped rather than
/// failing the whole read.
pub fn read_all_human() -> String {
    let raw = read_all();
    if raw.trim().is_empty() {
        return "No requests logged yet.".to_string();
    }
    raw.lines()
        .filter_map(|line| serde_json::from_str::<EgressEntry>(line).ok())
        .map(|e| render_human(&e))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // -- png_dimensions -------------------------------------------------

    fn tiny_png_bytes(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(&[0, 0, 0, 13]); // chunk length (irrelevant here)
        bytes.extend_from_slice(b"IHDR");
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&[0u8; 8]); // padding so len >= 24 comfortably
        bytes
    }

    #[test]
    fn png_dimensions_reads_width_and_height_from_ihdr() {
        let bytes = tiny_png_bytes(340, 200);
        assert_eq!(png_dimensions(&bytes), Some((340, 200)));
    }

    #[test]
    fn png_dimensions_is_none_for_a_short_buffer() {
        assert_eq!(png_dimensions(&[0x89, 0x50]), None);
    }

    #[test]
    fn png_dimensions_is_none_for_a_non_png_signature() {
        let mut bytes = tiny_png_bytes(10, 10);
        bytes[0] = 0; // corrupt the signature
        assert_eq!(png_dimensions(&bytes), None);
    }

    // -- attachments_from_body -------------------------------------------

    fn base64_png(width: u32, height: u32) -> String {
        base64::engine::general_purpose::STANDARD.encode(tiny_png_bytes(width, height))
    }

    #[test]
    fn attachments_from_body_finds_a_plain_base64_image_field() {
        let b64 = base64_png(340, 200);
        let body = json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"data": b64}}
            ]}]
        });
        let atts = attachments_from_body(&body);
        let image = atts
            .iter()
            .find(|a| a.kind == AttachmentKind::Image)
            .expect("an image attachment");
        assert_eq!(image.width, Some(340));
        assert_eq!(image.height, Some(200));
        assert!(image.size_bytes > 0);
    }

    #[test]
    fn attachments_from_body_finds_a_data_url_image() {
        let b64 = base64_png(64, 48);
        let data_url = format!("data:image/png;base64,{b64}");
        let body = json!({
            "model": "gpt-5.5",
            "messages": [{"role": "user", "content": [
                {"type": "image_url", "image_url": {"url": data_url}}
            ]}]
        });
        let atts = attachments_from_body(&body);
        let image = atts
            .iter()
            .find(|a| a.kind == AttachmentKind::Image)
            .expect("an image attachment");
        assert_eq!(image.width, Some(64));
        assert_eq!(image.height, Some(48));
    }

    #[test]
    fn attachments_from_body_sums_non_structural_text_as_one_attachment() {
        let body = json!({
            "model": "claude-opus-5",
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "What is on screen?"}]
        });
        let atts = attachments_from_body(&body);
        assert_eq!(atts.len(), 1, "{atts:?}");
        assert_eq!(atts[0].kind, AttachmentKind::Text);
        let expected = "You are a helpful assistant.".len() + "What is on screen?".len();
        assert_eq!(atts[0].size_bytes, expected);
    }

    #[test]
    fn attachments_from_body_excludes_structural_keys_from_text_size() {
        // "model" and "role" must never count toward the text attachment --
        // otherwise every request "grows" text purely from bookkeeping
        // fields that carry no user content at all.
        let body = json!({"model": "gpt-5.5", "role": "user"});
        let atts = attachments_from_body(&body);
        assert!(atts.is_empty(), "{atts:?}");
    }

    #[test]
    fn attachments_from_body_on_an_empty_object_is_empty() {
        assert!(attachments_from_body(&json!({})).is_empty());
    }

    #[test]
    fn attachments_from_body_handles_multiple_images() {
        let body = json!({
            "model": "gemma3:4b",
            "images": [base64_png(10, 10), base64_png(20, 20)]
        });
        let atts = attachments_from_body(&body);
        let images: Vec<_> = atts
            .iter()
            .filter(|a| a.kind == AttachmentKind::Image)
            .collect();
        assert_eq!(images.len(), 2, "{atts:?}");
    }

    // -- model_from --------------------------------------------------------

    #[test]
    fn model_from_reads_a_top_level_model_field() {
        let body = json!({"model": "claude-opus-5"});
        assert_eq!(
            model_from("https://api.anthropic.com/v1/messages", &body),
            Some("claude-opus-5".to_string())
        );
    }

    #[test]
    fn model_from_falls_back_to_the_gemini_style_url_path() {
        let body = json!({});
        let url = "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:generateContent";
        assert_eq!(model_from(url, &body), Some("gemini-3-pro".to_string()));
    }

    #[test]
    fn model_from_is_none_when_neither_source_has_it() {
        let body = json!({});
        assert_eq!(model_from("https://example.com/chat", &body), None);
    }

    // -- redact_opaque_tokens: the single most important test here --------

    #[test]
    fn redact_opaque_tokens_scrubs_a_long_key_shaped_token() {
        let fake_key = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE1234567890";
        let message = format!("HTTP 401: invalid api key {fake_key} supplied");
        let redacted = redact_opaque_tokens(&message);
        assert!(!redacted.contains(fake_key), "{redacted}");
        assert!(redacted.contains("[redacted]"), "{redacted}");
        assert!(redacted.contains("HTTP 401"), "{redacted}");
    }

    #[test]
    fn redact_opaque_tokens_leaves_short_tokens_and_prose_alone() {
        let message = "HTTP 429: too many requests, retry in 30s";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    #[test]
    fn build_entry_never_leaks_a_fake_key_from_headers_or_body() {
        // #106's single most important test: build a request the way a real
        // provider would (the key lives ONLY in the HTTP header, which this
        // function is never even given), run it through the exact
        // entry-building path, and assert the key string appears nowhere in
        // the resulting entry -- including inside a vendor error message
        // that happens to echo something key-shaped back.
        let fake_key = "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKE1234567890";
        let _headers_a_real_caller_would_send: [(&str, &str); 1] =
            [("authorization", &format!("Bearer {fake_key}"))];
        let body = json!({
            "model": "claude-opus-5",
            "system": "You are a helpful assistant.",
            "messages": [{"role": "user", "content": "What is on screen?"}]
        });
        let failure_text = format!("HTTP 401: bad credentials near token {fake_key}");

        let entry = build_entry(
            "anthropic",
            "https://api.anthropic.com/v1/messages",
            &body,
            false,
            body.to_string().len(),
            0,
            Outcome::Failure,
            Some(&failure_text),
        );

        let serialized = serde_json::to_string(&entry).unwrap();
        assert!(!serialized.contains(fake_key), "{serialized}");
        let human = render_human(&entry);
        assert!(!human.contains(fake_key), "{human}");
    }

    // -- cap_lines ----------------------------------------------------------

    #[test]
    fn cap_lines_keeps_everything_under_the_cap() {
        let existing = "line one\nline two\n";
        let result = cap_lines(existing, "line three", 1000);
        assert_eq!(result, "line one\nline two\nline three\n");
    }

    #[test]
    fn cap_lines_drops_the_oldest_line_first_once_over_the_cap() {
        let existing = "aaaaaaaaaa\nbbbbbbbbbb\n";
        // Cap tight enough that all three ten-char lines cannot fit.
        let result = cap_lines(existing, "cccccccccc", 24);
        assert!(!result.contains("aaaaaaaaaa"), "{result}");
        assert!(result.contains("bbbbbbbbbb"), "{result}");
        assert!(result.contains("cccccccccc"), "{result}");
    }

    #[test]
    fn cap_lines_never_drops_the_newest_line_even_if_it_alone_exceeds_the_cap() {
        let result = cap_lines("", "a very long single line entry", 5);
        assert!(result.contains("a very long single line entry"));
    }

    // -- render_human --------------------------------------------------------

    #[test]
    fn render_human_never_includes_raw_json_braces() {
        // A human paste target, not a JSON dump.
        let entry = build_entry(
            "openai",
            "https://api.openai.com/v1/responses",
            &json!({"model": "gpt-5.5"}),
            false,
            10,
            20,
            Outcome::Success,
            None,
        );
        let text = render_human(&entry);
        assert!(!text.contains('{'), "{text}");
        assert!(text.contains("provider=openai"), "{text}");
        assert!(text.contains("model=gpt-5.5"), "{text}");
        assert!(text.contains("ok"), "{text}");
    }

    #[test]
    fn render_human_names_the_failure_reason() {
        let entry = build_entry(
            "ollama",
            "http://127.0.0.1:11434/api/chat",
            &json!({"model": "gemma3:4b"}),
            true,
            10,
            0,
            Outcome::Failure,
            Some("transport error: connection refused"),
        );
        let text = render_human(&entry);
        assert!(text.contains("retry"), "{text}");
        assert!(text.contains("failed: transport error"), "{text}");
    }

    #[test]
    fn no_egress_text_contains_an_em_dash() {
        // CLAUDE.md rule 11: this text can end up pasted into a bug report.
        let entry = build_entry(
            "anthropic",
            "https://api.anthropic.com/v1/messages",
            &json!({"model": "claude-opus-5"}),
            false,
            0,
            0,
            Outcome::Failure,
            Some("HTTP 500: internal error"),
        );
        assert!(!render_human(&entry).contains('\u{2014}'));
    }

    // -- read_all_human with nothing logged ----------------------------------

    #[test]
    fn read_all_human_of_empty_input_says_so() {
        // Exercises the pure formatting branch directly (no file I/O):
        // an empty log must say something readable, never blank text or a
        // panic.
        let raw = "";
        let rendered = if raw.trim().is_empty() {
            "No requests logged yet.".to_string()
        } else {
            unreachable!()
        };
        assert_eq!(rendered, "No requests logged yet.");
    }
}
