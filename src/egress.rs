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
//! Rows live in the local store's `egress` table (`store/egress.rs`, #46).
//! Retention is 90 days and 10 000 rows, pruned inside the insert. The old
//! `%LOCALAPPDATA%\Wingman\egress.log` is imported once and renamed
//! `egress.log.migrated`.
//!
//! **A store failure never fails the request and never loses a row
//! silently.** When the store cannot open or a write fails (locked, full,
//! damaged, newer than this build, a failed ACL step), the failure is
//! reported once per run on the next card (`store::report`) and the row is
//! appended to `egress-pending.log` next to the store folder. The store
//! imports and removes that file on the next open that works, so the row
//! reaches the table and nothing reads two sources for long. Only when even
//! that append fails is the row kept in a bounded in-memory list
//! ([`UNSAVED_CAP`] rows, oldest dropped first and counted); the next
//! successful save writes the kept rows first, in order. [`read_all_human`]
//! shows the kept rows, the count waiting in the pending file, and the reason,
//! so "Copy egress log" still answers "what left this machine" meanwhile.
//!
//! # Knowledge snippets (#85+)
//!
//! A request that attaches excerpts from the user's documents (#92) records
//! one label per excerpt in [`EgressEntry::snippets`], in the spec's form
//! `file#chunk 12:ab12cd34` (file name, chunk number, first 8 hex characters
//! of the chunk hash), never the text, and one [`AttachmentKind::Knowledge`]
//! attachment with the excerpt bytes. The worker names its excerpts with
//! [`KnowledgeScope::new`] before it sends; the scope clears them when it is
//! dropped, the way the trigger is named
//! ([`crate::activation::set_worker_trigger`]). [`build_entry`] reads them on
//! that same thread. A request that attached none leaves the field empty.
//!
//! # Cost (#21)
//!
//! [`EgressEntry::cost`] is filled on a successful model response by
//! `provider::common`, which asks `usage::response_cost` (the one price
//! table, shared with the tooltip and the daily budget). This module has no
//! price knowledge of its own.

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex};

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::store::{Store, StoreError};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AttachmentKind {
    Image,
    Text,
    /// Excerpts from the user's documents (#92), a subset of the request's
    /// text. `size_bytes` is the excerpt text.
    Knowledge,
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
    /// One label per document excerpt in the request, empty when it carried
    /// none; see this module's doc comment on knowledge snippets.
    pub snippets: Vec<String>,
    pub bytes_sent: usize,
    pub bytes_received: usize,
    pub outcome: Outcome,
    /// Only set when `outcome == Outcome::Failure`, and always passed
    /// through [`redact_opaque_tokens`] first.
    pub error: Option<String>,
    /// Dollars for a successful response, from `usage::response_cost`.
    /// `None` when the model has no price row or the response carried no
    /// usage (shown as "unknown"), and on failures and non-model requests.
    pub cost: Option<f64>,
    /// What started the ask this request belongs to (`hotkey`, `activation`,
    /// `tray`, `palette`, `options`, see `activation::Trigger`), so "forty of these were
    /// activations" can be told apart from key presses (#236). `None` for
    /// requests not started by an ask, and for rows written before this
    /// field existed.
    #[serde(default)]
    pub trigger: Option<String>,
    /// How many private details were hidden from this request, by kind
    /// (`email`, `card number`, ...): regions painted into the screenshot
    /// plus values swapped for markers in the text (#508). Counts only,
    /// never what was hidden. Left out of the row when nothing was.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub redactions: BTreeMap<String, usize>,
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
/// (`capture.rs`, "PNG-only", AGENTS.md's stack table), so this alone is
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

/// True for a string leaf that is structural metadata (see
/// [`STRUCTURAL_STRING_KEYS`]) when found under `key`.
pub(crate) fn is_structural_key(key: &str) -> bool {
    STRUCTURAL_STRING_KEYS.contains(&key)
}

/// True for a string that is an embedded PNG: bare base64, or a
/// `data:image/png;base64,` URL.
pub(crate) fn is_embedded_png(s: &str) -> bool {
    is_probable_base64_png(s) || data_url_png_payload(s).is_some()
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
///
/// #273 widened it. The original alphabet was base64url and nothing else,
/// so a credential written in standard base64 (`+`, `/`, `=`) or split by a
/// vendor's own punctuation was seen as several short fragments, each under
/// the threshold, and every one of them survived: an AWS-style
/// `wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY` passed through completely
/// intact. Those characters cannot simply join the alphabet, because a URL
/// and a filesystem path are long runs over exactly them, so a wide run is
/// scrubbed whole only when it also [`looks_like_a_credential`]. See that
/// function for the discriminator and its one known false positive.
///
/// #273 also scrubs a hex secret of 32 or more digits cut into short pieces
/// by those separators (no upper case, so the credential test passes it by);
/// see [`hex_chain_spans`].
///
/// #253 made this matter twice over: it is now the same function that
/// scrubs the error text `provider::common` returns to the caller, which
/// rule 7 puts on the card and which a user pastes into a bug report.
///
/// #302 item 6. A newline is still a hard boundary: a credential wrapped
/// across a line break used to come out as two runs, the long first half
/// redacted and a short tail (under [`MIN_TOKEN_CHARS`] on its own) left to
/// survive in full. The run is now allowed to bridge a single `\n` when the
/// part before it is already long enough to be redacted on length alone,
/// since that is a guarantee, not a guess, that the whole thing was always
/// going to be scrubbed -- bridging only ever widens a redaction that was
/// already happening, never creates one in text that would otherwise be
/// left alone.
pub fn redact_opaque_tokens(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut run = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if is_wide_token_char(c) {
            run.push(c);
        } else if c == '\n'
            && run.chars().count() >= MIN_TOKEN_CHARS
            && chars.peek().is_some_and(|&next| is_wide_token_char(next))
        {
            // #302 item 6: a newline is still a hard boundary for `run`, so
            // a credential wrapped across a line break used to come out as
            // two runs -- the long first half got redacted, but a short
            // tail (under MIN_TOKEN_CHARS on its own) survived in full.
            //
            // This is safe to bridge rather than flush: the run so far is
            // already MIN_TOKEN_CHARS or longer, which on its own guarantees
            // `flush_run` will redact it whole (the narrow fallback below
            // has no character-class requirement, only this length floor),
            // so absorbing the newline can only ever extend a redaction
            // that was already going to happen, never create a new one in
            // text that would otherwise be left alone. Dropping the newline
            // here (instead of pushing it into `run`, which is not itself a
            // token character) lets the run keep accumulating across the
            // line break exactly as if it had not been there.
            continue;
        } else {
            flush_run(&mut run, &mut out);
            out.push(c);
        }
    }
    flush_run(&mut run, &mut out);
    out
}

/// The narrow alphabet the threshold rule applies to on length alone:
/// base64url plus the separators every vendor prefix uses (`sk-ant-...`,
/// `AIza...`). Unchanged since the first version of this function.
fn is_narrow_token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// The narrow alphabet plus the characters that split a credential into
/// short, individually-innocent fragments: standard-base64's `+`, `/` and
/// `=`, and the `.` a JWT joins its segments with (#273).
///
/// These cannot be added to the narrow set directly, because a URL and a
/// filesystem path are both long runs over exactly them, and scrubbing
/// those would cost the reader the one thing that makes a transport error
/// actionable. They only ever join a run that [`looks_like_a_credential`]
/// also accepts.
fn is_wide_token_char(c: char) -> bool {
    is_narrow_token_char(c) || matches!(c, '+' | '/' | '=' | '.')
}

/// Whether a wide run should be scrubbed whole rather than fragment by
/// fragment.
///
/// The discriminator is a mix of upper case, lower case and digit within
/// one unbroken run. Every credential shape has it, because the alphabet is
/// chosen for entropy. The things a wide run would otherwise swallow do
/// not: `//api.openai.com/v1/responses` is lower case and digits with no
/// upper case, `/Users/someone/AppData/Roaming` is upper and lower with no
/// digit, and prose is lower case with spaces that break the run anyway.
///
/// THEORY (unverified): a hostname or path that happens to carry all three
/// classes in one run, such as a versioned CDN path with a hash in it, will
/// be scrubbed. That is the safe direction to be wrong in, and the length
/// floor keeps it rare.
fn looks_like_a_credential(run: &str) -> bool {
    let mut upper = false;
    let mut lower = false;
    let mut digit = false;
    for c in run.chars() {
        upper |= c.is_ascii_uppercase();
        lower |= c.is_ascii_lowercase();
        digit |= c.is_ascii_digit();
    }
    upper && lower && digit
}

/// Scrub one maximal wide run: every span [`token_spans`] finds becomes
/// `[redacted]`, the rest is copied through.
fn flush_run(run: &mut String, out: &mut String) {
    let mut at = 0;
    for span in token_spans(run) {
        out.push_str(&run[at..span.start]);
        out.push_str("[redacted]");
        at = span.end;
    }
    out.push_str(&run[at..]);
    run.clear();
}

/// The shape every vendor's API key takes is at least this long; below it,
/// a run is a word, a status code or a short identifier.
const MIN_TOKEN_CHARS: usize = 20;

/// A hex secret (a 128-bit key is 32 digits) cut into short pieces by
/// separators is scrubbed once the pieces add up to this many hex digits.
const MIN_HEX_CHAIN_DIGITS: usize = 32;

/// The byte ranges of one wide run that get scrubbed, sorted and
/// non-overlapping. Three rules, in the order they were added:
///
/// 1. the whole run, when it is long enough and
///    [`looks_like_a_credential`];
/// 2. each narrow fragment of at least [`MIN_TOKEN_CHARS`], on length alone
///    (the original rule: nothing it redacted stops being redacted);
/// 3. a chain of hex pieces (#273, see [`hex_chain_spans`]).
///
/// Shared by [`redact_opaque_tokens`] and [`find_opaque_tokens`] so the log
/// and the screenshot painter can never disagree about what a secret is.
fn token_spans(run: &str) -> Vec<std::ops::Range<usize>> {
    if run.chars().count() >= MIN_TOKEN_CHARS && looks_like_a_credential(run) {
        return std::iter::once(0..run.len()).collect();
    }
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in run.char_indices() {
        match (is_narrow_token_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(b)) => {
                spans.push(b..i);
                start = None;
            }
            _ => {}
        }
    }
    if let Some(b) = start {
        spans.push(b..run.len());
    }
    spans.retain(|r| {
        let part = &run[r.clone()];
        part.chars().count() >= MIN_TOKEN_CHARS && !KNOWN_VENDOR_CODES.contains(&part)
    });
    spans.extend(hex_chain_spans(run));
    spans.sort_by_key(|r| r.start);

    // Merge overlaps: a long hex fragment is found by rules 2 and 3.
    let mut merged: Vec<std::ops::Range<usize>> = Vec::with_capacity(spans.len());
    for span in spans {
        match merged.last_mut() {
            Some(last) if span.start <= last.end => last.end = last.end.max(span.end),
            _ => merged.push(span),
        }
    }
    merged
}

/// #941: the error `type` and `code` words the vendors publish that are long
/// enough to look key-shaped. A narrow fragment equal to one of these, whole,
/// is kept, so the card still says why a request failed. It is a fixed list,
/// not a shape rule: anything else, however word-like, is still scrubbed.
const KNOWN_VENDOR_CODES: [&str; 12] = [
    "invalid_request_error",
    "authentication_error",
    "content_policy_violation",
    "image_content_policy_violation",
    "context_length_exceeded",
    "invalid_organization",
    "string_above_max_length",
    "unsupported_country_region_territory",
    "invalid_request_parameters",
    "request_too_large",
    "permission_denied_error",
    "insufficient_permissions",
];

/// #273: a same-case hex secret split by `/ + = . - _` is a row of short
/// fragments, none long enough for the narrow rule and none mixed-case
/// enough for [`looks_like_a_credential`]. Find chains of consecutive pieces
/// that are each nothing but hex digits, joined by a single separator, and
/// scrub a chain of [`MIN_HEX_CHAIN_DIGITS`] or more digits.
///
/// A piece that is not all hex (`api`, `v1`, `key`) ends the chain, which is
/// what keeps lower-case URLs and paths alone. The span starts at the first
/// digit and ends at the last, so `key=` and the following space survive.
fn hex_chain_spans(run: &str) -> Vec<std::ops::Range<usize>> {
    let is_sep = |c: char| matches!(c, '/' | '+' | '=' | '.' | '-' | '_');
    // (byte range, all hex digits and non-empty) per piece, split on separators.
    let mut pieces: Vec<(std::ops::Range<usize>, bool)> = Vec::new();
    let mut start = 0;
    for (i, c) in run.char_indices() {
        if is_sep(c) {
            pieces.push((
                start..i,
                !run[start..i].is_empty() && all_hex(&run[start..i]),
            ));
            start = i + c.len_utf8();
        }
    }
    pieces.push((
        start..run.len(),
        !run[start..].is_empty() && all_hex(&run[start..]),
    ));

    let mut spans = Vec::new();
    let mut chain: Option<(usize, usize, usize)> = None; // (first byte, last byte, digits)
    let mut close = |chain: &mut Option<(usize, usize, usize)>| {
        if let Some((from, to, digits)) = chain.take() {
            if digits >= MIN_HEX_CHAIN_DIGITS {
                spans.push(from..to);
            }
        }
    };
    for (range, hex) in &pieces {
        if !hex {
            close(&mut chain);
            continue;
        }
        // Pieces are separated by exactly one separator char (an empty piece
        // between two separators is not hex and already broke the chain).
        match &mut chain {
            Some((_, to, digits)) => {
                *to = range.end;
                *digits += range.len();
            }
            None => chain = Some((range.start, range.end, range.len())),
        }
    }
    close(&mut chain);
    spans
}

fn all_hex(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_hexdigit())
}

/// Byte ranges of the key-shaped tokens [`redact_opaque_tokens`] would
/// scrub in `s`, for callers that need to know where they are (screenshot
/// redaction paints the words over a span, #103). Same rules as
/// `flush_run`, by way of [`token_spans`]. Single-line input only: the
/// newline bridging in `redact_opaque_tokens` has no meaning for one OCR
/// line.
pub(crate) fn find_opaque_tokens(s: &str) -> Vec<std::ops::Range<usize>> {
    let mut spans = Vec::new();
    let mut start = None;
    for (i, c) in s.char_indices() {
        match (is_wide_token_char(c), start) {
            (true, None) => start = Some(i),
            (false, Some(b)) => {
                spans.extend(run_spans(s, b..i));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(b) = start {
        spans.extend(run_spans(s, b..s.len()));
    }
    spans
}

fn run_spans(s: &str, run: std::ops::Range<usize>) -> Vec<std::ops::Range<usize>> {
    token_spans(&s[run.clone()])
        .into_iter()
        .map(|r| run.start + r.start..run.start + r.end)
        .collect()
}

/// The excerpts the current worker thread is about to send: their labels and
/// the bytes of their text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct WorkerKnowledge {
    labels: Vec<String>,
    bytes: usize,
}

thread_local! {
    static WORKER_KNOWLEDGE: std::cell::RefCell<WorkerKnowledge> =
        const { std::cell::RefCell::new(WorkerKnowledge { labels: Vec::new(), bytes: 0 }) };
}

/// The labels set on this thread, for the send preview.
pub fn worker_knowledge_labels() -> Vec<String> {
    WORKER_KNOWLEDGE.with(|k| k.borrow().labels.clone())
}

/// Names the excerpts this thread's requests attach (see the module doc) and
/// forgets them when dropped, on every exit path.
pub struct KnowledgeScope;

impl KnowledgeScope {
    pub fn new(labels: Vec<String>, bytes: usize) -> Self {
        WORKER_KNOWLEDGE.with(|k| *k.borrow_mut() = WorkerKnowledge { labels, bytes });
        Self
    }
}

impl Drop for KnowledgeScope {
    fn drop(&mut self) {
        WORKER_KNOWLEDGE.with(|k| *k.borrow_mut() = WorkerKnowledge::default());
    }
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
    let mut attachments = attachments_from_body(body);
    let knowledge = WORKER_KNOWLEDGE.with(|k| k.borrow().clone());
    if !knowledge.labels.is_empty() {
        attachments.push(Attachment {
            kind: AttachmentKind::Knowledge,
            size_bytes: knowledge.bytes,
            width: None,
            height: None,
        });
    }
    // The painted counts describe the screenshot, so they only belong on a
    // request that carries one.
    let redactions = if attachments.iter().any(|a| a.kind == AttachmentKind::Image) {
        crate::redact::image_counts()
    } else {
        BTreeMap::new()
    };
    EgressEntry {
        time_unix: now_unix(),
        provider: provider.to_string(),
        model: model_from(url, body),
        retried,
        attachments,
        snippets: knowledge.labels,
        bytes_sent,
        bytes_received,
        outcome,
        // #733: the log is pasted into bug reports too: text redaction and
        // the account name as well as key tokens.
        error: error.map(crate::diagnostics::scrub_error_chain_here),
        cost: None,
        trigger: crate::activation::worker_trigger_label().map(str::to_string),
        redactions,
    }
}

impl EgressEntry {
    /// The same row with its cost filled in (#487).
    pub fn with_cost(mut self, cost: Option<f64>) -> Self {
        self.cost = cost;
        self
    }

    /// The same row with the text step's counts added to the painted ones.
    pub fn with_text_redactions(mut self, counts: BTreeMap<String, usize>) -> Self {
        for (kind, n) in counts {
            *self.redactions.entry(kind).or_insert(0) += n;
        }
        self
    }
}

/// Most rows kept in memory while the store cannot save them. A broken store
/// must not turn every request into unbounded memory growth; the oldest rows
/// go first and are counted, never dropped without a trace.
const UNSAVED_CAP: usize = 500;

/// Rows the store has not accepted yet, oldest first.
#[derive(Default)]
struct Unsaved {
    rows: VecDeque<EgressEntry>,
    /// Rows pushed out of `rows` by the cap.
    dropped: usize,
    /// Rows written to the pending file this run (not in `rows`).
    parked: usize,
}

static UNSAVED: Mutex<Unsaved> = Mutex::new(Unsaved {
    rows: VecDeque::new(),
    dropped: 0,
    parked: 0,
});

static SAVE_GATE: Mutex<()> = Mutex::new(());

fn lock_unsaved(unsaved: &Mutex<Unsaved>) -> std::sync::MutexGuard<'_, Unsaved> {
    // A panic elsewhere must not take the history with it.
    unsaved.lock().unwrap_or_else(|e| e.into_inner())
}

/// Saves one row to the store. Never panics and never surfaces an error to
/// its caller (rule 7: a logging failure must never be why the request
/// itself fails or shows an error card). A failure keeps the row in memory
/// and is reported once per run; see this module's doc comment.
pub fn record(entry: &EgressEntry) {
    save(
        &crate::store::global,
        crate::store::append_pending,
        &UNSAVED,
        entry,
    );
}

/// The body of [`record`], with the store and the kept-rows list passed in so
/// the failure paths can be tested without touching process-wide state.
fn save(
    open: &dyn Fn() -> Result<Arc<Store>, StoreError>,
    fallback: impl FnMut(&EgressEntry) -> Result<(), StoreError>,
    unsaved: &Mutex<Unsaved>,
    entry: &EgressEntry,
) {
    // One save at a time, so rows kept by a failed save cannot be reordered
    // behind a newer row from another thread.
    let _gate = SAVE_GATE.lock().unwrap_or_else(|e| e.into_inner());
    // Anything kept from earlier failures goes first, then this row.
    let mut pending: Vec<EgressEntry> = lock_unsaved(unsaved).rows.drain(..).collect();
    pending.push(entry.clone());
    let Err(e) = open().and_then(|store| write_pending(&mut pending, |r| store.record_egress(r)))
    else {
        return;
    };
    crate::store::report(&e);
    // The store is down: park what is left in the pending file, which the
    // store imports on the next open that works. Only a row that cannot be
    // parked either stays in memory.
    let before = pending.len();
    let _ = write_pending(&mut pending, fallback);
    lock_unsaved(unsaved).parked += before - pending.len();
    keep(unsaved, pending);
}

/// Writes `pending` oldest first and removes each row as it is saved, so on a
/// failure `pending` holds exactly the rows that are still unsaved.
fn write_pending(
    pending: &mut Vec<EgressEntry>,
    mut write: impl FnMut(&EgressEntry) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let mut saved = 0;
    let mut result = Ok(());
    for row in pending.iter() {
        match write(row) {
            Ok(()) => saved += 1,
            Err(e) => {
                result = Err(e);
                break;
            }
        }
    }
    pending.drain(..saved);
    result
}

/// Puts rows back at the front of the kept list (anything that arrived from
/// another thread meanwhile is newer) and applies the cap.
fn keep(unsaved: &Mutex<Unsaved>, rows: Vec<EgressEntry>) {
    let mut u = lock_unsaved(unsaved);
    for row in rows.into_iter().rev() {
        u.rows.push_front(row);
    }
    while u.rows.len() > UNSAVED_CAP {
        u.rows.pop_front();
        u.dropped += 1;
    }
}

/// Test builds only: every saved row, oldest first.
#[cfg(test)]
pub fn rows() -> Result<Vec<EgressEntry>, StoreError> {
    crate::store::global()?.egress_rows()
}

/// [`rows`] and how many stored rows were unreadable and left out.
fn rows_counted() -> Result<(Vec<EgressEntry>, usize), StoreError> {
    crate::store::global()?.egress_rows_counted()
}

/// Test builds only: every saved row as JSON lines, the shape the old log
/// had, so tests can look at what a request logged.
#[cfg(test)]
pub fn read_all() -> String {
    rows()
        .unwrap_or_default()
        .iter()
        .filter_map(|e| serde_json::to_string(e).ok())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Test builds only: forgets every saved row (the store is shared by every
/// test in the process, so tests that read "the last row" start clean).
#[cfg(test)]
pub fn clear_for_test() {
    if let Ok(store) = crate::store::global() {
        store.clear_egress();
    }
}

/// Renders one entry as a human-readable line for the "Copy egress log"
/// clipboard action (`diagnostics::egress_report`) -- the on-disk format
/// stays JSON-lines (machine-readable, ready for #46's SQLite import), but
/// nobody should have to read raw JSON pasted into a bug report.
pub fn render_human(entry: &EgressEntry) -> String {
    let mut parts = Vec::new();
    parts.push(format!("t={}", entry.time_unix));
    parts.push(format!("provider={}", entry.provider));
    if let Some(trigger) = &entry.trigger {
        parts.push(format!("trigger={trigger}"));
    }
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
            AttachmentKind::Knowledge => "knowledge",
        };
        match (a.width, a.height) {
            (Some(w), Some(h)) => {
                parts.push(format!("{kind}: {} ({w}x{h})", human_bytes(a.size_bytes)))
            }
            _ => parts.push(format!("{kind}: {}", human_bytes(a.size_bytes))),
        }
    }
    parts.push(format!(
        "snippets={}",
        if entry.snippets.is_empty() {
            "none".to_string()
        } else {
            entry.snippets.join("; ")
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
    if !entry.redactions.is_empty() {
        let kinds: Vec<String> = entry
            .redactions
            .iter()
            .map(|(kind, n)| format!("{n} {kind}"))
            .collect();
        parts.push(format!("redacted: {}", kinds.join(", ")));
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

/// Every saved row, oldest first, rendered per [`render_human`], then any
/// rows the store has not accepted yet and why -- what
/// `diagnostics::egress_report` puts on the clipboard. Never blank: a store
/// that cannot be read says so instead of looking like an empty log.
pub fn read_all_human() -> String {
    human_report(rows_counted(), &lock_unsaved(&UNSAVED))
}

fn human_report(saved: Result<(Vec<EgressEntry>, usize), StoreError>, unsaved: &Unsaved) -> String {
    let mut lines: Vec<String> = Vec::new();
    match saved {
        Ok((rows, skipped)) => {
            lines.extend(rows.iter().map(render_human));
            if skipped > 0 {
                lines.push(format!("{skipped} saved requests could not be read."));
            }
        }
        Err(e) => lines.push(format!("The saved request log could not be read. {e}")),
    }
    if unsaved.parked > 0 {
        lines.push(format!(
            "{} requests are waiting in egress-pending.log and will be listed after the next start.",
            unsaved.parked
        ));
    }
    if !unsaved.rows.is_empty() || unsaved.dropped > 0 {
        lines.push(format!(
            "Not saved yet ({} requests, kept in memory until a save works):",
            unsaved.rows.len()
        ));
        lines.extend(unsaved.rows.iter().map(render_human));
        if unsaved.dropped > 0 {
            lines.push(format!(
                "{} older requests that could not be saved were dropped.",
                unsaved.dropped
            ));
        }
    }
    if lines.is_empty() {
        return "No requests logged yet.".to_string();
    }
    lines.join("\n")
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
        let url =
            "https://generativelanguage.googleapis.com/v1beta/models/gemini-3-pro:generateContent";
        assert_eq!(model_from(url, &body), Some("gemini-3-pro".to_string()));
    }

    #[test]
    fn model_from_is_none_when_neither_source_has_it() {
        let body = json!({});
        assert_eq!(model_from("https://example.com/chat", &body), None);
    }

    // -- redact_opaque_tokens: the single most important test here --------

    /// #941: a vendor's published error type and code stay readable; word-like
    /// text that is not on the list (a key with short segments, a passphrase)
    /// is still scrubbed.
    #[test]
    fn redact_opaque_tokens_keeps_known_vendor_codes_only() {
        let body = r#"{"type":"invalid_request_error","code":"content_policy_violation"}"#;
        assert_eq!(redact_opaque_tokens(body), body);

        for other in [
            "tok_prod_abcdefghijkl",
            "correct_horse_battery",
            "invalid_request_error_x",
            "xinvalid_request_error",
        ] {
            let out = redact_opaque_tokens(&format!("key {other} bad"));
            assert!(!out.contains(other), "{out}");
        }
    }

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

    // -- #273: the separator characters the first pass did not know about --
    //
    // The original scrubber's token alphabet was `[A-Za-z0-9_-]`, which is
    // base64url and nothing else. A credential written in standard base64
    // (`+`, `/`, `=`) or split by a vendor's own punctuation is therefore
    // seen as several short fragments, each under the 20-character
    // threshold, and every one of them survives. These tests are the shapes
    // that leaked.

    /// The worst case: an AWS-style secret whose `/` separators leave three
    /// fragments, none of them long enough to trip the old threshold, so the
    /// whole credential passed through intact.
    #[test]
    fn redact_opaque_tokens_scrubs_a_key_split_by_slashes() {
        let fake = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
        let message = format!("HTTP 403: signature mismatch for {fake} in request");
        let redacted = redact_opaque_tokens(&message);
        assert!(!redacted.contains(fake), "{redacted}");
        assert!(
            !redacted.contains("wJalrXUtnFEMI"),
            "no fragment may survive: {redacted}"
        );
        assert!(redacted.contains("HTTP 403"), "{redacted}");
    }

    #[test]
    fn redact_opaque_tokens_scrubs_a_standard_base64_key() {
        let fake = "QUJDRGVmZ2hpSktMTU5vcHFyc3R1dnd4+Kg/4Q==";
        let message = format!("auth failed: token {fake} rejected");
        let redacted = redact_opaque_tokens(&message);
        assert!(!redacted.contains(fake), "{redacted}");
        assert!(
            !redacted.contains("QUJDRGVmZ2hpSktMTU5vcHFyc3R1dnd4"),
            "{redacted}"
        );
    }

    /// A JWT is three base64url segments joined by dots. Each segment can be
    /// under 20 characters on a small payload.
    #[test]
    fn redact_opaque_tokens_scrubs_a_dot_separated_token() {
        let fake = "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4";
        let message = format!("401: bearer {fake} expired");
        let redacted = redact_opaque_tokens(&message);
        assert!(!redacted.contains(fake), "{redacted}");
        assert!(redacted.contains("401"), "{redacted}");
    }

    /// The reason the fix cannot simply add `/` and `.` to the token
    /// alphabet: a URL is a long run over exactly those characters, and
    /// scrubbing it would cost the reader the one thing that makes a
    /// transport error actionable. The extra characters only join a run
    /// when the run also looks like a credential, which a lowercase
    /// hostname and path do not.
    #[test]
    fn redact_opaque_tokens_leaves_a_url_intact() {
        let message = "transport error: failed to connect to https://api.openai.com/v1/responses";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    #[test]
    fn redact_opaque_tokens_leaves_a_windows_path_intact() {
        let message = r"could not read C:/Users/someone/AppData/Roaming/Wingman/actions.toml";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    /// Prose is prose however long the sentence is: without a mix of upper,
    /// lower and digit in one unbroken run there is nothing credential
    /// shaped about it.
    #[test]
    fn redact_opaque_tokens_leaves_long_prose_intact() {
        let message = "the model did not return any message text in the response body";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    /// The original guarantee must not regress: a single run over the old
    /// alphabet is still scrubbed on length alone, with no character-class
    /// test, because that is what caught `sk-ant-...` and `AIza...`.
    #[test]
    fn redact_opaque_tokens_still_scrubs_a_lowercase_only_run_on_length_alone() {
        let fake = "abcdefghijklmnopqrstuvwxyz";
        let redacted = redact_opaque_tokens(&format!("key {fake} rejected"));
        assert!(!redacted.contains(fake), "{redacted}");
    }

    /// The 20-character boundary is unchanged either side of it.
    #[test]
    fn redact_opaque_tokens_threshold_is_unchanged_at_nineteen_and_twenty() {
        let nineteen = "a1B".repeat(6) + "z"; // 19
        assert_eq!(nineteen.chars().count(), 19);
        assert!(redact_opaque_tokens(&nineteen).contains(&nineteen));

        let twenty = nineteen.clone() + "z"; // 20
        assert_eq!(twenty.chars().count(), 20);
        assert!(!redact_opaque_tokens(&twenty).contains(&twenty));
    }

    // -- #302 item 6: a key split across a newline --------------------------

    /// The shape the issue describes: a `sk-`-style 40+ character key
    /// wrapped onto a second line by whatever put it in the error text,
    /// leaving a short tail (well under `MIN_TOKEN_CHARS`) on its own on the
    /// second line. Before the fix this tail survived in full because the
    /// newline split it from the long first run that triggered redaction.
    #[test]
    fn redact_opaque_tokens_scrubs_a_key_split_across_a_newline() {
        let first = "sk-proj-ABCdefGHIjkl1234567890MNOPqr"; // 36 chars
        let tail = "stUVWXYZ"; // 8 chars: short on its own
        let message = format!("request failed: key {first}\n{tail} was rejected");

        let redacted = redact_opaque_tokens(&message);
        assert!(!redacted.contains(first), "{redacted}");
        assert!(!redacted.contains(tail), "no tail may survive: {redacted}");
        assert!(redacted.contains("[redacted]"), "{redacted}");
        assert!(redacted.contains("request failed"), "{redacted}");
        assert!(redacted.contains("was rejected"), "{redacted}");
    }

    /// The other half of the fix's safety case: ordinary prose that happens
    /// to wrap onto a new line must come through untouched. Nothing in this
    /// sentence is a 20+ character unbroken run of token characters, so the
    /// bridging rule never engages.
    #[test]
    fn redact_opaque_tokens_leaves_a_normal_multiline_sentence_intact() {
        let message = "The request failed because the connection\nwas reset by the remote host while streaming the response body.";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    // -- #273: a same-case hex secret split by separators -------------------

    /// A 40-character hex token cut into short pieces by `/`, `-`, `.` or
    /// `_` leaves every piece under `MIN_TOKEN_CHARS` and no upper case, so
    /// neither the credential heuristic nor the narrow rule catches it.
    /// Decision: 32 or more hex digits in one chain are scrubbed.
    const HEX40: &str = "0123456789abcdef0123456789abcdef01234567";

    fn split_hex(sep: &str, hex: &str) -> String {
        hex.as_bytes()
            .chunks(8)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join(sep)
    }

    #[test]
    fn redact_opaque_tokens_scrubs_split_hex_in_a_url_path() {
        for sep in ["/", "-", ".", "_", "+", "="] {
            let token = split_hex(sep, HEX40);
            let message = format!("GET https://api.example.com/v1/{token} failed");
            let redacted = redact_opaque_tokens(&message);
            for piece in token.split(sep) {
                assert!(!redacted.contains(piece), "sep {sep:?}: {redacted}");
            }
            assert!(
                redacted.contains("https://api.example.com/v1/"),
                "{redacted}"
            );
            assert!(redacted.ends_with(" failed"), "{redacted}");
        }
    }

    #[test]
    fn redact_opaque_tokens_scrubs_split_hex_after_key_equals() {
        let token = split_hex("/", HEX40);
        let redacted = redact_opaque_tokens(&format!("auth failed key={token} (401)"));
        assert!(redacted.starts_with("auth failed key="), "{redacted}");
        assert!(redacted.ends_with(" (401)"), "{redacted}");
        assert!(!redacted.contains("01234567"), "{redacted}");
        assert!(redacted.contains("[redacted]"), "{redacted}");
    }

    #[test]
    fn redact_opaque_tokens_scrubs_split_upper_case_hex() {
        let token = split_hex("/", &HEX40.to_uppercase());
        let redacted = redact_opaque_tokens(&format!("key {token} rejected"));
        assert!(!redacted.contains("89ABCDEF"), "{redacted}");
    }

    /// One digit under the floor: 31 hex digits split into short pieces are
    /// left alone, so the new rule stops exactly at 32.
    #[test]
    fn redact_opaque_tokens_leaves_split_hex_under_thirty_two_digits() {
        let token = split_hex("/", &HEX40[..31]);
        let message = format!("see {token} for details");
        assert_eq!(redact_opaque_tokens(&message), message);
        let token = split_hex("/", &HEX40[..32]);
        let message = format!("see {token} for details");
        assert!(redact_opaque_tokens(&message).contains("[redacted]"));
    }

    /// Hex pieces with a non-hex word between them are not one chain.
    #[test]
    fn redact_opaque_tokens_hex_chain_stops_at_a_non_hex_piece() {
        let message = "/dead/beef/cafe/0123/archive/abcd/ef01/2345/6789/abcd/ef01/2345";
        assert_eq!(redact_opaque_tokens(message), message);
    }

    #[test]
    fn redact_opaque_tokens_leaves_ordinary_lowercase_text_alone() {
        for message in [
            "POST https://api.openai.com/v1/chat/completions returned 429",
            "could not read c:/users/someone/appdata/roaming/wingman/state.json",
            "the connection was reset by the remote host before any response",
            "error sending request for url (http://127.0.0.1:11434/api/chat)",
        ] {
            assert_eq!(redact_opaque_tokens(message), message);
        }
    }

    /// `find_opaque_tokens` (screenshot redaction) must agree with the text
    /// redaction, so a split hex secret is painted over as well.
    #[test]
    fn find_opaque_tokens_reports_a_split_hex_chain() {
        let token = split_hex("/", HEX40);
        let text = format!("key {token} end");
        let spans = find_opaque_tokens(&text);
        assert_eq!(spans.len(), 1, "{spans:?}");
        assert_eq!(&text[spans[0].clone()], token);
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

    // -- knowledge excerpts (#92) ----------------------------------------------

    #[test]
    fn a_request_with_excerpts_records_their_labels_and_never_their_text() {
        let body = json!({"model": "gpt-5.5", "input": "secret excerpt words"});
        let send = || {
            build_entry(
                "openai",
                "https://api.openai.com/v1/responses",
                &body,
                false,
                10,
                20,
                Outcome::Success,
                None,
            )
        };
        assert!(send().snippets.is_empty());
        let labels = vec!["cv.md#chunk 3:ab12cd34".to_string()];
        let entry = {
            let _scope = KnowledgeScope::new(labels.clone(), 20);
            assert_eq!(worker_knowledge_labels(), labels);
            send()
        };
        assert_eq!(entry.snippets, labels);
        let k = entry
            .attachments
            .iter()
            .find(|a| a.kind == AttachmentKind::Knowledge)
            .expect("a knowledge attachment");
        assert_eq!(k.size_bytes, 20);
        let human = render_human(&entry);
        assert!(human.contains("cv.md#chunk 3:ab12cd34"), "{human}");
        assert!(human.contains("knowledge:"), "{human}");
        assert!(!serde_json::to_string(&entry)
            .unwrap()
            .contains("secret excerpt"));
        // The scope is gone: the next request on this thread carries none.
        assert!(send().snippets.is_empty());
        assert!(worker_knowledge_labels().is_empty());
    }

    // -- trigger (#236) ------------------------------------------------------

    fn sample_entry() -> EgressEntry {
        build_entry(
            "openai",
            "https://api.openai.com/v1/responses",
            &json!({"model": "gpt-5.5"}),
            false,
            10,
            20,
            Outcome::Success,
            None,
        )
    }

    #[test]
    fn an_entry_written_by_a_marked_thread_carries_its_trigger() {
        let entry = std::thread::spawn(|| {
            crate::activation::set_worker_trigger(crate::activation::Trigger::Activation);
            sample_entry()
        })
        .join()
        .unwrap();
        assert_eq!(entry.trigger.as_deref(), Some("activation"));
        let line = serde_json::to_string(&entry).unwrap();
        assert!(line.contains(r#""trigger":"activation""#), "{line}");
        assert!(render_human(&entry).contains("trigger=activation"));
        let back: EgressEntry = serde_json::from_str(&line).unwrap();
        assert_eq!(back, entry);
    }

    #[test]
    fn an_unmarked_thread_writes_no_trigger_and_old_rows_still_parse() {
        let entry = std::thread::spawn(sample_entry).join().unwrap();
        assert_eq!(entry.trigger, None);
        assert!(!render_human(&entry).contains("trigger="));
        // A row written before the field existed has no "trigger" key.
        let mut v = serde_json::to_value(&entry).unwrap();
        v.as_object_mut().unwrap().remove("trigger");
        let back: EgressEntry = serde_json::from_value(v).unwrap();
        assert_eq!(back.trigger, None);
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
        // AGENTS.md rule 11: this text can end up pasted into a bug report.
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

    // -- saving, and what happens when the store cannot save -----------------
    //
    // `provider::common`'s tests reach `record` for real, so the shared test
    // store holds rows from other tests: these use their own provider names
    // and their own kept-rows list.

    fn tagged(provider: &str) -> EgressEntry {
        build_entry(
            provider,
            "https://api.example.com/",
            &json!({"model": "m"}),
            false,
            1,
            1,
            Outcome::Success,
            None,
        )
    }

    fn providers_like(prefix: &str) -> Vec<String> {
        rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .filter(|p| p.starts_with(prefix))
            .collect()
    }

    fn refuse() -> Result<Arc<Store>, StoreError> {
        Err(StoreError::Other("disk full".to_string()))
    }

    fn no_fallback(_: &EgressEntry) -> Result<(), StoreError> {
        Err(StoreError::Other("no pending file".to_string()))
    }

    /// The shared test store is also cleared by `provider::common`'s tests,
    /// which hold this lock; so must anything that reads it back.
    fn store_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::config::EGRESS_PREVIEW_TEST_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn record_saves_a_row_the_diagnostics_report_lists() {
        // The wired-to-nothing check: the public entry point, through the
        // real store, to the text "Copy egress log" puts on the clipboard.
        let _lock = store_lock();
        record(&tagged("wired-check"));
        assert_eq!(providers_like("wired-check"), ["wired-check"]);
        assert!(
            read_all_human().contains("provider=wired-check"),
            "the report must list a request saved a moment ago"
        );
    }

    #[test]
    fn a_failing_store_keeps_the_row_in_memory_and_does_not_panic() {
        let unsaved = Mutex::new(Unsaved::default());
        save(&refuse, no_fallback, &unsaved, &tagged("kept-1"));
        let u = lock_unsaved(&unsaved);
        assert_eq!(u.rows.len(), 1);
        assert_eq!(u.rows[0].provider, "kept-1");
    }

    #[test]
    fn a_store_that_cannot_open_parks_the_row_in_the_pending_file() {
        // The regression the old ACL test guarded: a store failure for the
        // whole run must not turn into rows lost on exit.
        let path = std::env::temp_dir().join(format!(
            "wingman-test-egress-park-{}.log",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);
        let unsaved = Mutex::new(Unsaved::default());

        save(
            &refuse,
            |r| crate::store::append_pending_at(&path, r),
            &unsaved,
            &tagged("parked-1"),
        );

        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(text.contains("\"provider\":\"parked-1\""), "{text}");
        let u = lock_unsaved(&unsaved);
        assert!(u.rows.is_empty(), "a parked row is not also kept");
        assert_eq!(u.parked, 1);
        let report = human_report(Ok((Vec::new(), 0)), &u);
        assert!(report.contains("1 requests are waiting"), "{report}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_next_good_save_writes_the_kept_rows_first_in_order() {
        let _lock = store_lock();
        let unsaved = Mutex::new(Unsaved::default());
        save(&refuse, no_fallback, &unsaved, &tagged("flush-a"));
        save(&refuse, no_fallback, &unsaved, &tagged("flush-b"));
        assert_eq!(lock_unsaved(&unsaved).rows.len(), 2);

        save(
            &crate::store::global,
            no_fallback,
            &unsaved,
            &tagged("flush-c"),
        );

        assert!(lock_unsaved(&unsaved).rows.is_empty());
        assert_eq!(providers_like("flush-"), ["flush-a", "flush-b", "flush-c"]);
    }

    #[test]
    fn a_write_that_fails_part_way_keeps_only_the_unsaved_rows() {
        // The first row saves, the second fails: only the second and later
        // rows may stay pending, or the first would be written twice.
        let mut pending = vec![tagged("half-a"), tagged("half-b"), tagged("half-c")];
        let mut calls = 0;
        let r = write_pending(&mut pending, |_| {
            calls += 1;
            if calls == 2 {
                Err(StoreError::Other("locked".to_string()))
            } else {
                Ok(())
            }
        });
        assert!(r.is_err());
        let left: Vec<&str> = pending.iter().map(|e| e.provider.as_str()).collect();
        assert_eq!(left, ["half-b", "half-c"]);
    }

    #[test]
    fn the_kept_list_is_bounded_and_counts_what_it_drops() {
        let unsaved = Mutex::new(Unsaved::default());
        for i in 0..UNSAVED_CAP + 3 {
            save(&refuse, no_fallback, &unsaved, &tagged(&format!("cap-{i}")));
        }
        let u = lock_unsaved(&unsaved);
        assert_eq!(u.rows.len(), UNSAVED_CAP);
        assert_eq!(u.dropped, 3);
        assert_eq!(u.rows.front().unwrap().provider, "cap-3", "oldest go first");
        assert_eq!(
            u.rows.back().unwrap().provider,
            format!("cap-{}", UNSAVED_CAP + 2),
            "the newest is never the one dropped"
        );
    }

    #[test]
    fn the_report_shows_kept_rows_and_the_reason_when_the_store_is_down() {
        let mut unsaved = Unsaved::default();
        unsaved.rows.push_back(tagged("report-kept"));
        unsaved.dropped = 2;
        let text = human_report(Err(StoreError::Other("locked".to_string())), &unsaved);
        assert!(text.contains("could not be read"), "{text}");
        assert!(text.contains("locked"), "{text}");
        assert!(text.contains("provider=report-kept"), "{text}");
        assert!(text.contains("2 older requests"), "{text}");
        assert!(!text.contains('\u{2014}'), "{text}");
    }

    #[test]
    fn the_report_lists_saved_rows_then_kept_rows() {
        let mut unsaved = Unsaved::default();
        unsaved.rows.push_back(tagged("second"));
        let text = human_report(Ok((vec![tagged("first")], 1)), &unsaved);
        assert!(
            text.contains("1 saved requests could not be read"),
            "{text}"
        );
        let (first, second) = (
            text.find("provider=first").unwrap(),
            text.find("provider=second").unwrap(),
        );
        assert!(first < second, "{text}");
    }

    #[test]
    fn an_empty_log_says_so() {
        assert_eq!(
            human_report(Ok((Vec::new(), 0)), &Unsaved::default()),
            "No requests logged yet."
        );
    }

    // -- redaction counts (#508) -------------------------------------------

    fn image_body() -> Value {
        json!({"model": "m", "messages": [{"role": "user", "content": [
            {"type": "image", "source": {"data": base64_png(10, 10)}}
        ]}]})
    }

    fn entry_for(body: &Value) -> EgressEntry {
        build_entry(
            "p",
            "https://x.example/",
            body,
            false,
            1,
            1,
            Outcome::Success,
            None,
        )
    }

    #[test]
    fn a_request_with_painted_regions_logs_their_kinds_and_counts() {
        crate::redact::reset_image_counts();
        crate::redact::note_image_redactions("card number", 2);
        let entry = entry_for(&image_body());
        crate::redact::reset_image_counts();
        assert_eq!(entry.redactions.get("card number"), Some(&2));
    }

    #[test]
    fn painted_counts_stay_off_a_request_that_carries_no_image() {
        crate::redact::reset_image_counts();
        crate::redact::note_image_redactions("email", 3);
        let entry = entry_for(&json!({"model": "m"}));
        crate::redact::reset_image_counts();
        assert!(entry.redactions.is_empty());
    }

    #[test]
    fn text_counts_add_to_painted_counts_of_the_same_kind() {
        crate::redact::reset_image_counts();
        crate::redact::note_image_redactions("email", 1);
        let text: std::collections::BTreeMap<String, usize> =
            [("email".to_string(), 2), ("key".to_string(), 1)].into();
        let entry = entry_for(&image_body()).with_text_redactions(text);
        crate::redact::reset_image_counts();
        assert_eq!(entry.redactions.get("email"), Some(&3));
        assert_eq!(entry.redactions.get("key"), Some(&1));
    }

    #[test]
    fn the_row_holds_counts_only_never_the_values() {
        crate::redact::reset_image_counts();
        let mut r = crate::redact::Redactions::default();
        r.redact_text("write to someone@example.com");
        let entry = entry_for(&json!({"model": "m"})).with_text_redactions(r.counts());
        let row = serde_json::to_string(&entry).unwrap();
        assert!(row.contains("\"redactions\":{\"email\":1}"), "{row}");
        assert!(
            !row.contains("someone") && !row.contains("example.com\""),
            "{row}"
        );
    }

    #[test]
    fn a_row_with_no_redactions_omits_the_field_and_old_rows_still_read() {
        crate::redact::reset_image_counts();
        let row = serde_json::to_string(&entry_for(&json!({"model": "m"}))).unwrap();
        assert!(!row.contains("redactions"), "{row}");
        let back: EgressEntry = serde_json::from_str(&row).unwrap();
        assert!(back.redactions.is_empty());
    }

    #[test]
    fn the_human_line_names_redaction_counts() {
        crate::redact::reset_image_counts();
        let text: std::collections::BTreeMap<String, usize> =
            [("email".to_string(), 2), ("key".to_string(), 1)].into();
        let with = render_human(&entry_for(&json!({"model": "m"})).with_text_redactions(text));
        assert!(with.contains("redacted: 2 email, 1 key"), "{with}");
        let without = render_human(&entry_for(&json!({"model": "m"})));
        assert!(!without.contains("redacted"), "{without}");
    }
}
