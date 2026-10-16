//! Screenshot redaction before upload (issue #103; design in
//! `docs/superpowers/specs/2026-10-15-screenshot-redaction-design.md`).
//!
//! One OCR pass over the captured pixels finds card numbers, IBANs, key-shaped
//! tokens and email addresses, and each match is painted over with a solid box
//! BEFORE the pixels are encoded to PNG, so the bytes a cloud provider receives
//! never contain the text. A box, not a blur: small blurred text can often be
//! read back, a filled rectangle cannot.
//!
//! Everything except [`apply`] is pure over text and pixels and unit-tested
//! without Win32. [`apply`] is the one call that runs OCR; it blocks, so it
//! runs on the worker thread (rule 5/7), never the message loop.
//!
//! Failure policy: [`apply`] returns `Err` when OCR cannot run. The caller
//! turns that into a card and sends nothing (fail closed).

use crate::capture::{RawShot, ScreenMap, ScreenRect};
use crate::ocr::{OcrLine, OcrWord};
use anyhow::{Context, Result};
use std::ops::Range;
use std::time::Duration;

/// How long redaction waits for OCR before giving up (and so sending
/// nothing). Cold OCR of a full screen is a few hundred milliseconds (see the
/// spec's MEASURED cost section); this is a generous ceiling, not a target.
const OCR_TIMEOUT: Duration = Duration::from_secs(5);

/// Prefix of the error [`apply`] returns when the check cannot run.
pub const FAILURE_NEEDLE: &str = "Could not check the screenshot for private details";

/// What the card says when the foreground app runs as administrator and
/// Windows refuses to list its password fields (#506). Used as the error text
/// itself; `app::humanize_error_chain` maps it to this same sentence.
pub const ADMIN_APP_SENTENCE: &str =
    "Wingman can't check this app for password fields (it may be running as administrator), so nothing was sent.";

/// What the card says when [`apply`] fails, whatever the underlying cause.
/// `app::humanize_error_chain` maps any error chain containing
/// [`FAILURE_NEEDLE`] to this sentence, so a Win32-looking cause cannot turn
/// it into "Something went wrong."
pub const FAILURE_SENTENCE: &str =
    "Could not check the screenshot for private details, so nothing was sent.";

/// Pixels of padding added around a painted region, so anti-aliased glyph
/// edges at the rect boundary do not survive.
const PAD: f32 = 3.0;

/// The fill colour of a painted region: opaque black.
pub const FILL: [u8; 4] = [0, 0, 0, 255];

/// A region of image pixels to paint over. `x1`/`y1` are exclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PixelRect {
    pub x0: u32,
    pub y0: u32,
    pub x1: u32,
    pub y1: u32,
}

/// Byte ranges of email addresses in `text`: `local@domain.tld`, the local
/// part from `[A-Za-z0-9._%+-]`, the domain from `[A-Za-z0-9.-]` with a final
/// label of two or more letters. Hand-written to avoid a regex dependency.
pub fn email_spans(text: &str) -> Vec<Range<usize>> {
    let bytes = text.as_bytes();
    let local_ok =
        |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'%' | b'+' | b'-');
    let domain_ok = |b: u8| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-');
    let mut spans: Vec<Range<usize>> = Vec::new();
    for (at, _) in text.match_indices('@') {
        let mut start = at;
        while start > 0 && local_ok(bytes[start - 1]) {
            start -= 1;
        }
        let mut end = at + 1;
        while end < bytes.len() && domain_ok(bytes[end]) {
            end += 1;
        }
        // A trailing dot or hyphen is sentence punctuation, not the domain.
        while end > at + 1 && matches!(bytes[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let domain = &text[at + 1..end];
        let tld_ok = domain
            .rsplit('.')
            .next()
            .is_some_and(|t| t.len() >= 2 && t.bytes().all(|b| b.is_ascii_alphabetic()));
        let is_new = spans.last().map_or(true, |last| last.end <= start);
        if start < at && domain.contains('.') && !domain.starts_with('.') && tld_ok && is_new {
            spans.push(start..end);
        }
    }
    spans
}

/// Every sensitive byte range in one line of text, sorted by start: card
/// numbers, IBANs, key-shaped tokens, emails. Ranges may overlap (a key-shaped
/// run can contain digits that also pass Luhn); callers paint each one. This
/// is the screenshot rule set; text uses [`text_spans`], which is narrower.
#[cfg(test)]
pub fn sensitive_spans(text: &str) -> Vec<Range<usize>> {
    sensitive_spans_labeled(text)
        .into_iter()
        .map(|(r, _)| r)
        .collect()
}

/// [`sensitive_spans`] with the kind of each range, for the egress log count.
fn sensitive_spans_labeled(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut spans: Vec<(Range<usize>, &'static str)> = Vec::new();
    let mut add = |found: Vec<Range<usize>>, label| {
        spans.extend(found.into_iter().map(|r| (r, label)));
    };
    add(crate::payment_denylist::card_number_spans(text), CARD);
    add(crate::payment_denylist::iban_spans(text), IBAN);
    add(crate::egress::find_opaque_tokens(text), KEY);
    add(shared_key_spans(text), KEY);
    add(email_spans(text), EMAIL);
    spans.sort_by_key(|(r, _)| (r.start, r.end));
    spans
}

// -- text redaction (#505) -------------------------------------------------
//
// The same four kinds as the screenshot step, with rules tuned for running
// text instead of OCR lines: free text is long and mostly innocent, so a
// false positive costs the model real context. Differences from the pixel
// rules: card numbers need a known network prefix, and key-shaped tokens use
// a narrow rule (a known vendor prefix, or a long digit-bearing alphanumeric
// run) and are never looked for inside a URL.

/// Marker label for each kind. The marker in the request is `[label N]`.
const CARD: &str = "card number";
const IBAN: &str = "IBAN";
const KEY: &str = "key";
const EMAIL: &str = "email";

/// Vendor key prefixes. A run starting with one of these is a key from
/// [`MIN_PREFIXED_KEY_CHARS`] characters on. One list for both paths: the text
/// rule and the screenshot rule both call [`shared_key_spans`], so a prefix
/// added here is covered everywhere (#734).
const KEY_PREFIXES: &[&str] = &[
    "sk-",
    "sk_",
    "pk_",
    "rk_",
    "AIza",
    "ghp_",
    "gho_",
    "ghu_",
    "ghs_",
    "ghr_",
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "xoxs-",
    "xoxe-",
    "xoxr-",
    "xapp-",
    "glpat-",
    "hf_",
    "whsec_",
    "npm_",
    "pypi-",
    "rubygems_",
    "shpat_",
    "shpca_",
    "shppa_",
    "shpss_",
    "dop_v1_",
    "doo_v1_",
    "dor_v1_",
    "gsk_",
    "pplx-",
    "nvapi-",
    "sq0atp-",
    "sq0csp-",
    "figd_",
    "lin_api_",
    "sbp_",
    "tskey-",
    "xkeysib-",
    "NRAK-",
];
const MIN_PREFIXED_KEY_CHARS: usize = 16;
/// A run with no known prefix must be this long, purely alphanumeric, and
/// hold both a digit and a letter. A hyphenated or underscored run is a slug
/// or an identifier (`2024-10-15-weekly-sync-notes`), not a key.
const MIN_BARE_KEY_CHARS: usize = 24;

/// True when a whitespace-free chunk is a URL or a web address (`https://x`,
/// `www.x`, `docs.google.com/d/<id>`): its long ids are links, not secrets.
fn url_shaped(chunk: &str) -> bool {
    if chunk.contains("://") || chunk.starts_with("www.") {
        return true;
    }
    match chunk.find('/') {
        Some(slash) => {
            let host = &chunk[..slash];
            host.contains('.') && !host.contains('@') && !host.starts_with('.')
        }
        None => false,
    }
}

fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// An AWS access key id: `AKIA` or `ASIA` then exactly 16 upper-case letters
/// or digits, 20 in all. `run` is already bounded by non-key characters, so
/// the whole run must be that shape (`ASIA-PACIFIC-REGION-REPORT` is not).
fn is_aws_key_id(run: &str) -> bool {
    run.len() == 20
        && (run.starts_with("AKIA") || run.starts_with("ASIA"))
        && run[4..]
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
}

/// What follows a vendor prefix has to look like random key material, not a
/// name: `npm_config_cache_dir` and `npm_lifecycle_event` are environment
/// variable names, `hf_hub_offline` is a setting. Key material has digits: at
/// least three, or one beside an upper-case letter. (A key with fewer digits
/// than that exists, but is rare enough that missing it is the cheaper error.)
fn looks_like_key_material(rest: &str) -> bool {
    let digits = rest.bytes().filter(u8::is_ascii_digit).count();
    let upper = rest.bytes().any(|b| b.is_ascii_uppercase());
    digits >= 3 || (digits >= 1 && upper)
}

/// A key with a recognisable vendor shape: an AWS id, or a vendor prefix
/// followed by key material. Also applied inside a link: a vendor prefix plus
/// random-looking material is unlikely to be a document id.
fn is_prefixed_key_run(run: &str) -> bool {
    is_aws_key_id(run)
        || (run.len() >= MIN_PREFIXED_KEY_CHARS
            && KEY_PREFIXES
                .iter()
                .find(|p| run.starts_with(**p))
                .is_some_and(|p| looks_like_key_material(&run[p.len()..])))
}

/// A long letters-and-digits run with no vendor shape. Not applied inside a
/// link, where long mixed-case ids are ordinary.
fn is_bare_key_run(run: &str) -> bool {
    run.len() >= MIN_BARE_KEY_CHARS
        && run.bytes().all(|b| b.is_ascii_alphanumeric())
        && run.bytes().any(|b| b.is_ascii_digit())
        && run.bytes().any(|b| b.is_ascii_alphabetic())
}

/// A JWT: three dot-joined base64url segments, the first two non-empty and
/// the first starting `eyJ` (the base64 of `{"`). The signature may be empty
/// (`alg: none`), so it is not required to hold anything.
fn is_jwt(run: &str) -> bool {
    let mut parts = run.split('.');
    let (Some(head), Some(payload), Some(_sig), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    head.starts_with("eyJ") && payload.len() >= 2 && run.len() >= MIN_PREFIXED_KEY_CHARS
}

/// A SendGrid key is `SG.` then 22 characters, a dot and 43 characters; a
/// Google OAuth access token is `ya29.` then a long run of key characters
/// (40 or more, no more dots). Anything else with the prefix, such as
/// `SG.Reports.Quarterly.xlsx`, is not one.
fn is_dotted_vendor_key(run: &str) -> bool {
    let all_key = |s: &str| s.chars().all(is_key_char);
    if let Some(rest) = run.strip_prefix("SG.") {
        return rest
            .split_once('.')
            .is_some_and(|(a, b)| a.len() == 22 && b.len() == 43 && all_key(a) && all_key(b));
    }
    run.strip_prefix("ya29.")
        .is_some_and(|rest| rest.len() >= 40 && all_key(rest))
}

/// How far past a private key header the search for its end line looks, and
/// the most a block with no end line is redacted. Real keys are a few
/// kilobytes; the bound keeps the scan linear on hostile text.
const PEM_WINDOW: usize = 16 * 1024;

/// Private key blocks: from a `-----BEGIN <label>-----` header whose label
/// ends in `PRIVATE KEY` (or is `PGP PRIVATE KEY BLOCK`) through the matching
/// `-----END <label>-----` line. Certificates and public keys are not secrets
/// and are left alone.
///
/// When there is no end line in reach (a selection often cuts the block off)
/// the redaction runs to the next blank line, or [`PEM_WINDOW`] bytes after
/// the header, whichever comes first. The choice: never the rest of the text,
/// so a cut-off key does not swallow the question typed after it, and a
/// real key body has no blank line in it.
///
/// Every step moves past what it looked at, so the scan is linear.
fn pem_private_key_spans(text: &str) -> Vec<Range<usize>> {
    const BEGIN: &str = "-----BEGIN ";
    const DASHES: &str = "-----";
    let mut spans = Vec::new();
    let mut from = 0;
    while let Some(rel) = text[from..].find(BEGIN) {
        let start = from + rel;
        let label_start = start + BEGIN.len();
        // The label ends at the closing dashes, on the same line.
        let Some(label_len) = text[label_start..].find(DASHES) else {
            break; // no dashes anywhere later, so no header can follow
        };
        let label = &text[label_start..label_start + label_len];
        let header_end = label_start + label_len + DASHES.len();
        if label.contains('\n') {
            from = label_start;
            continue;
        }
        if !(label.ends_with("PRIVATE KEY") || label == "PGP PRIVATE KEY BLOCK") {
            from = header_end;
            continue;
        }
        let mut limit = (header_end + PEM_WINDOW).min(text.len());
        while !text.is_char_boundary(limit) {
            limit -= 1;
        }
        let window = &text[header_end..limit];
        let end_line = format!("-----END {label}-----");
        let end = match window.find(&end_line) {
            Some(e) => header_end + e + end_line.len(),
            None => {
                let blank = ["\n\n", "\r\n\r\n"]
                    .iter()
                    .filter_map(|b| window.find(b))
                    .min();
                header_end + blank.unwrap_or(window.len())
            }
        };
        spans.push(start..end);
        from = end;
    }
    spans
}

/// Key shapes both paths share (#734): a run with a vendor prefix from
/// [`KEY_PREFIXES`], a dotted vendor key (`ya29.`, `SG.`), a JWT, and a PEM
/// private key block. The text path adds its narrow bare-run rule, the
/// screenshot path adds the wide opaque-token rule, but these cannot drift
/// apart because there is one copy. Also applied inside a link: each shape is
/// specific enough (a vendor prefix plus key material, a JWT's `eyJ`, a PEM
/// header) that a document id is unlikely to match it.
pub(crate) fn shared_key_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = pem_private_key_spans(text);
    // Runs over key characters: vendor prefixes and AWS ids.
    // Runs over key characters and dots: dotted vendor keys and JWTs.
    let mut key_start = None;
    let mut dot_start = None;
    for (i, c) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        match (is_key_char(c), key_start) {
            (true, None) => key_start = Some(i),
            (false, Some(b)) => {
                key_start = None;
                if is_prefixed_key_run(&text[b..i]) {
                    spans.push(b..i);
                }
            }
            _ => {}
        }
        match (is_key_char(c) || c == '.', dot_start) {
            (true, None) => dot_start = Some(i),
            (false, Some(b)) => {
                dot_start = None;
                let run = text[b..i].trim_end_matches('.');
                if is_jwt(run) || is_dotted_vendor_key(run) {
                    spans.push(b..b + run.len());
                }
            }
            _ => {}
        }
    }
    spans
}

/// Key-shaped runs inside one whitespace-free chunk of `text`. A URL-shaped
/// chunk only gets the bare-run rule when it is not a link; the vendor shapes
/// are in [`shared_key_spans`].
fn chunk_key_spans(text: &str, range: Range<usize>, spans: &mut Vec<Range<usize>>) {
    let chunk = &text[range.clone()];
    let in_link = url_shaped(chunk);
    let mut run_start = None;
    // A trailing space sentinel closes a run that reaches the chunk's end.
    for (i, c) in chunk
        .char_indices()
        .chain(std::iter::once((chunk.len(), ' ')))
    {
        match (is_key_char(c), run_start) {
            (true, None) => run_start = Some(i),
            (false, Some(b)) => {
                run_start = None;
                let run = &chunk[b..i];
                if !in_link && is_bare_key_run(run) {
                    spans.push(range.start + b..range.start + i);
                }
            }
            _ => {}
        }
    }
}

/// Byte ranges of key-shaped tokens in running text (see the section note).
fn text_key_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = Vec::new();
    let mut chunk_start = None;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace(), chunk_start) {
            (false, None) => chunk_start = Some(i),
            (true, Some(b)) => {
                chunk_key_spans(text, b..i, &mut spans);
                chunk_start = None;
            }
            _ => {}
        }
    }
    if let Some(b) = chunk_start {
        chunk_key_spans(text, b..text.len(), &mut spans);
    }
    spans
}

/// Every sensitive byte range in `text` with its marker label, sorted by
/// start. Ranges may overlap (a key can contain digits that pass Luhn).
fn text_spans(text: &str) -> Vec<(Range<usize>, &'static str)> {
    let mut spans: Vec<(Range<usize>, &'static str)> = Vec::new();
    let mut add = |found: Vec<Range<usize>>, label| {
        spans.extend(found.into_iter().map(|r| (r, label)));
    };
    add(
        crate::payment_denylist::card_number_spans_with_known_prefix(text),
        CARD,
    );
    add(crate::payment_denylist::iban_spans(text), IBAN);
    add(text_key_spans(text), KEY);
    add(shared_key_spans(text), KEY);
    add(email_spans(text), EMAIL);
    spans.sort_by_key(|(r, _)| (r.start, r.end));
    spans
}

/// The private details swapped out of one outbound request, kept only in
/// memory for the life of that request so the answer can get its real values
/// back. Numbered markers (`[email 1]`); the same value always gets the same
/// marker. Never logged, never stored: `Debug` prints a count, and the type
/// has no `Clone` or `Serialize`.
#[derive(Default)]
pub struct Redactions {
    /// `(marker, original)` in first-seen order.
    pairs: Vec<(String, String)>,
    /// The highest number used so far per label, counting both the markers
    /// issued here and any `[label N]` already written in the input, so a new
    /// marker can never be confused with text that was there already.
    last: std::collections::HashMap<&'static str, u32>,
    /// How many different values were replaced, per kind (#508).
    counted: std::collections::BTreeMap<&'static str, usize>,
}

impl std::fmt::Debug for Redactions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Redactions({} item(s))", self.pairs.len())
    }
}

impl Redactions {
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }

    /// How many different values were replaced, by kind: what the egress log
    /// shows. Counts only, never the values.
    pub fn counts(&self) -> std::collections::BTreeMap<String, usize> {
        self.counted
            .iter()
            .map(|(k, n)| (log_kind(k), *n))
            .collect()
    }

    /// Notes every `[label N]` already written in `text`, so numbering
    /// starts above it. Call on all of a request's text before redacting any
    /// of it ([`Redactions::redact_value`] does).
    pub fn note_existing(&mut self, text: &str) {
        if !text.contains('[') {
            return;
        }
        for label in [CARD, IBAN, KEY, EMAIL] {
            let open = format!("[{label} ");
            for (at, _) in text.match_indices(&open) {
                let rest = &text[at + open.len()..];
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                if !digits.is_empty() && rest[digits.len()..].starts_with(']') {
                    if let Ok(n) = digits.parse::<u32>() {
                        let slot = self.last.entry(label).or_insert(0);
                        *slot = (*slot).max(n);
                    }
                }
            }
        }
    }

    fn marker_for(&mut self, label: &'static str, original: &str) -> String {
        if let Some((marker, _)) = self.pairs.iter().find(|(_, o)| o == original) {
            return marker.clone();
        }
        let n = self.last.get(label).copied().unwrap_or(0) + 1;
        self.last.insert(label, n);
        *self.counted.entry(label).or_insert(0) += 1;
        let marker = format!("[{label} {n}]");
        self.pairs.push((marker.clone(), original.to_string()));
        marker
    }

    /// `text` with every sensitive span replaced by its numbered marker.
    /// Overlapping spans become one marker, the first one's. Text with
    /// nothing sensitive comes back unchanged.
    pub fn redact_text(&mut self, text: &str) -> String {
        let spans = text_spans(text);
        if spans.is_empty() {
            return text.to_string();
        }
        self.note_existing(text);
        // Join overlapping spans into one range each, keeping the first
        // one's label, so the stored original is the whole union and the
        // restored answer loses nothing.
        let mut groups: Vec<(Range<usize>, &'static str)> = Vec::new();
        for (r, label) in spans {
            match groups.last_mut() {
                Some((last, _)) if r.start < last.end => last.end = last.end.max(r.end),
                _ => groups.push((r, label)),
            }
        }
        let mut out = String::with_capacity(text.len());
        let mut at = 0;
        for (r, label) in groups {
            out.push_str(&text[at..r.start]);
            out.push_str(&self.marker_for(label, &text[r.clone()]));
            at = r.end;
        }
        out.push_str(&text[at..]);
        out
    }

    /// Puts the originals back into a model response that is JSON text.
    /// Each original is JSON-escaped first, so a restored value can never
    /// break the document. A marker the model made up, one not in this
    /// request's list, is left as it is.
    pub fn restore_in_json(&self, response: String) -> String {
        let mut out = response;
        for (marker, original) in &self.pairs {
            if !out.contains(marker.as_str()) {
                continue;
            }
            let escaped = serde_json::to_string(original).unwrap_or_default();
            let escaped = escaped
                .strip_prefix('"')
                .and_then(|s| s.strip_suffix('"'))
                .unwrap_or("");
            out = out.replace(marker.as_str(), escaped);
        }
        out
    }

    /// Redacts every text string in a provider request body, in place, and
    /// says whether anything changed. Skips structural metadata (`model`,
    /// `role`, ...) and embedded PNGs, which the pixel step already handled
    /// and which a text rule would corrupt. In place on purpose: nothing is
    /// copied, images included.
    pub fn redact_value(&mut self, v: &mut serde_json::Value) -> bool {
        self.note_existing_in(v);
        self.redact_value_under(v, "")
    }

    fn note_existing_in(&mut self, v: &serde_json::Value) {
        use serde_json::Value;
        match v {
            Value::String(s) => self.note_existing(s),
            Value::Array(items) => items.iter().for_each(|i| self.note_existing_in(i)),
            Value::Object(map) => map.values().for_each(|i| self.note_existing_in(i)),
            _ => {}
        }
    }

    fn redact_value_under(&mut self, v: &mut serde_json::Value, key: &str) -> bool {
        use serde_json::Value;
        match v {
            Value::String(s) => {
                if crate::egress::is_structural_key(key) || crate::egress::is_embedded_png(s) {
                    return false;
                }
                let redacted = self.redact_text(s);
                if redacted == *s {
                    return false;
                }
                *s = redacted;
                true
            }
            Value::Array(items) => {
                let mut changed = false;
                for i in items {
                    changed |= self.redact_value_under(i, key);
                }
                changed
            }
            Value::Object(map) => {
                let mut changed = false;
                for (k, i) in map.iter_mut() {
                    changed |= self.redact_value_under(i, k);
                }
                changed
            }
            _ => false,
        }
    }
}

/// Where each word of `line_text` sits, as byte ranges, found by searching for
/// the words in order. `None` when a word cannot be found in order (the OCR
/// text and its words disagree), in which case the caller paints the whole
/// line: too much is the safe direction.
pub fn word_byte_ranges(line_text: &str, words: &[OcrWord]) -> Option<Vec<Range<usize>>> {
    let mut cursor = 0;
    let mut out = Vec::with_capacity(words.len());
    for w in words {
        if w.text.is_empty() {
            return None;
        }
        let at = line_text[cursor..].find(&w.text)? + cursor;
        out.push(at..at + w.text.len());
        cursor = at + w.text.len();
    }
    Some(out)
}

/// The pixel regions to paint for `lines`, clipped to a `width` x `height`
/// image. One region per sensitive span (the union of the words it touches),
/// or the whole line when its words cannot be matched to its text.
#[cfg(test)]
pub fn regions_for_lines(lines: &[OcrLine], width: u32, height: u32) -> Vec<PixelRect> {
    regions_with_kinds(lines, width, height)
        .into_iter()
        .map(|(r, _)| r)
        .collect()
}

/// [`regions_for_lines`] with the kind of each region.
fn regions_with_kinds(
    lines: &[OcrLine],
    width: u32,
    height: u32,
) -> Vec<(PixelRect, &'static str)> {
    let mut out = Vec::new();
    for line in lines {
        let spans = sensitive_spans_labeled(&line.text);
        if spans.is_empty() {
            continue;
        }
        let ranges = if line.words.is_empty() {
            None
        } else {
            word_byte_ranges(&line.text, &line.words)
        };
        for (span, label) in spans {
            let rect = match &ranges {
                Some(ranges) => union_of_overlapping(&line.words, ranges, &span),
                None => None,
            }
            .unwrap_or((line.x, line.y, line.x + line.width, line.y + line.height));
            if let Some(r) = clip(rect, width, height) {
                out.push((r, label));
            }
        }
    }
    out
}

/// The union `(x0, y0, x1, y1)` of every word whose byte range overlaps
/// `span`, or `None` if none does.
fn union_of_overlapping(
    words: &[OcrWord],
    ranges: &[Range<usize>],
    span: &Range<usize>,
) -> Option<(f32, f32, f32, f32)> {
    let mut acc: Option<(f32, f32, f32, f32)> = None;
    for (w, r) in words.iter().zip(ranges) {
        if r.start < span.end && span.start < r.end {
            let b = (w.x, w.y, w.x + w.width, w.y + w.height);
            acc = Some(match acc {
                None => b,
                Some(a) => (a.0.min(b.0), a.1.min(b.1), a.2.max(b.2), a.3.max(b.3)),
            });
        }
    }
    acc
}

/// Pads a float rect, rounds outward and clips it to the image. `None` when
/// nothing is left (a zero-size or off-image rect).
fn clip(rect: (f32, f32, f32, f32), width: u32, height: u32) -> Option<PixelRect> {
    let x0 = (rect.0 - PAD).floor().max(0.0) as u32;
    let y0 = (rect.1 - PAD).floor().max(0.0) as u32;
    let x1 = ((rect.2 + PAD).ceil().max(0.0) as u32).min(width);
    let y1 = ((rect.3 + PAD).ceil().max(0.0) as u32).min(height);
    (x0 < x1 && y0 < y1).then_some(PixelRect { x0, y0, x1, y1 })
}

/// Fills each rect with [`FILL`], in place.
pub fn paint(raw: &mut RawShot, rects: &[PixelRect]) {
    let stride = raw.width as usize * 4;
    for r in rects {
        let x1 = r.x1.min(raw.width) as usize;
        let y1 = r.y1.min(raw.height) as usize;
        for y in r.y0 as usize..y1 {
            for x in r.x0 as usize..x1 {
                let i = y * stride + x * 4;
                raw.rgba[i..i + 4].copy_from_slice(&FILL);
            }
        }
    }
}

/// The image regions of the password fields at `rects` (screen pixels), through
/// a screen map (#506). Pure: padded like OCR regions, clipped to the image,
/// and an off-image, empty or unmappable rect gives nothing. Never panics on
/// a rect from another process (negative, huge, reversed).
pub fn password_regions(
    rects: &[ScreenRect],
    map: &ScreenMap,
    width: u32,
    height: u32,
) -> Vec<PixelRect> {
    rects
        .iter()
        .filter_map(|r| map.to_image(*r))
        .filter_map(|r| clip(r, width, height))
        .collect()
}

/// Paints the password fields of the window `raw` was captured against.
/// Returns how many regions were painted. A shot with no screen map (not a
/// capture) has nothing to map and is left alone. The pure half is
/// [`password_regions`]; this collects the lookup that started at capture time
/// and paints. Blocks only for what is left of
/// [`crate::inputs::uia::PASSWORD_LOOKUP_BUDGET`]: worker thread only. A
/// timeout, a failed start or an unexpected error fails closed; a window that
/// has gone has no fields; an administrator app has its own sentence.
fn apply_password_fields(raw: &mut RawShot) -> Result<usize> {
    use crate::inputs::uia::LookupError;
    let Some(map) = raw.screen.clone() else {
        return Ok(0);
    };
    // `apply` runs only for a request that is being redacted, and every such
    // capture starts a lookup (`capture::needs_password_lookup`). A map with
    // none is a capture path that forgot: refuse rather than send unchecked.
    let Some(lookup) = &map.lookup else {
        return Err(anyhow::anyhow!("no password field lookup was started").context(FAILURE_NEEDLE));
    };
    // The lookup started when the capture was taken; only what is left of its
    // budget is waited for here.
    let rects = lookup
        .collect(crate::inputs::uia::PASSWORD_LOOKUP_BUDGET)
        .map_err(|e| match e {
            LookupError::AccessDenied => anyhow::anyhow!(ADMIN_APP_SENTENCE),
            other => anyhow::anyhow!("password field lookup: {other:?}").context(FAILURE_NEEDLE),
        })?;
    let regions = password_regions(&rects, &map, raw.width, raw.height);
    Ok(paint_password_fields(raw, &regions))
}

/// Paints the password-manager windows that were beside the front window
/// when `raw` was captured (#575). Returns how many regions were painted.
/// Needs no lookup and nothing to wait for: the rectangles were read at
/// capture time. A shot with no screen map has none.
///
/// Fails closed: a list that could not be built (the window enumeration
/// failed, ran out of budget, or found too many managers) is an `Err`, so
/// nothing unpainted is sent.
fn apply_manager_windows(raw: &mut RawShot) -> Result<usize> {
    let Some(map) = &raw.screen else {
        return Ok(0);
    };
    let rects = map.manager_windows.as_ref().map_err(|why| {
        anyhow::anyhow!("password manager windows: {why}").context(FAILURE_NEEDLE)
    })?;
    let regions = password_regions(rects, map, raw.width, raw.height);
    paint(raw, &regions);
    note_image_redactions("password manager", regions.len());
    Ok(regions.len())
}

/// Paints the password-field `regions` and counts them for the egress log.
fn paint_password_fields(raw: &mut RawShot, regions: &[PixelRect]) -> usize {
    paint(raw, regions);
    note_image_redactions("password field", regions.len());
    regions.len()
}

/// Paints `lines`' sensitive regions into `raw` and returns how many regions
/// were painted. The pure half of [`apply`].
pub fn apply_lines(raw: &mut RawShot, lines: &[OcrLine]) -> usize {
    let found = regions_with_kinds(lines, raw.width, raw.height);
    let rects: Vec<PixelRect> = found.iter().map(|(r, _)| *r).collect();
    paint(raw, &rects);
    for (_, label) in &found {
        note_image_redactions(&log_kind(label), 1);
    }
    rects.len()
}

/// The name of a kind in the egress log: `card number`, `iban`, `key`, `email`.
fn log_kind(label: &str) -> String {
    label.to_lowercase()
}

thread_local! {
    /// What was painted into the screenshot this worker thread is sending, by
    /// kind (#508). A thread-local like the egress trigger label: the encode
    /// and the HTTP call run on the same worker thread, and other threads
    /// start with nothing.
    static IMAGE_COUNTS: std::cell::RefCell<std::collections::BTreeMap<String, usize>> =
        const { std::cell::RefCell::new(std::collections::BTreeMap::new()) };
}

/// Forgets the painted counts, at the start of each screenshot encode.
pub fn reset_image_counts() {
    IMAGE_COUNTS.with(|c| c.borrow_mut().clear());
}

/// Adds `n` painted regions of `kind` to this thread's screenshot count.
pub fn note_image_redactions(kind: &str, n: usize) {
    if n > 0 {
        IMAGE_COUNTS.with(|c| *c.borrow_mut().entry(kind.to_string()).or_insert(0) += n);
    }
}

/// What was painted into this thread's screenshot, by kind.
pub fn image_counts() -> std::collections::BTreeMap<String, usize> {
    IMAGE_COUNTS.with(|c| c.borrow().clone())
}

/// Runs OCR over `raw` and paints over everything sensitive it reads. Returns
/// the number of regions painted. Blocks for the OCR call: worker thread only.
///
/// An OCR failure is an `Err`, never "nothing found": the caller must not
/// send a screenshot nobody checked.
pub fn apply(raw: &mut RawShot) -> Result<usize> {
    let out = crate::ocr::recognize(&raw.rgba, raw.width, raw.height, OCR_TIMEOUT)
        .context(FAILURE_NEEDLE)?;
    // A password manager beside the front window is a whole window, painted
    // before anything else so nothing of it is read or sent (#575).
    let managers = apply_manager_windows(raw)?;
    // Password fields: OCR cannot read a masked field, and a revealed one is
    // not key-shaped (#506). A lookup that fails fails closed, like OCR.
    let passwords = apply_password_fields(raw)?;
    Ok(apply_lines(raw, &out.lines) + passwords + managers)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CARD: &str = "4111 1111 1111 1111";

    fn text_of(text: &str, spans: Vec<Range<usize>>) -> Vec<&str> {
        spans.into_iter().map(|r| &text[r]).collect()
    }

    // -- span finders ---------------------------------------------------

    #[test]
    fn card_number_span_covers_just_the_number() {
        let t = format!("Card: {CARD} expires soon");
        let spans = crate::payment_denylist::card_number_spans(&t);
        assert_eq!(text_of(&t, spans), vec![CARD]);
    }

    #[test]
    fn card_number_after_other_digits_in_the_same_run_is_found() {
        let t = format!("Total 100 {CARD}");
        let spans = crate::payment_denylist::card_number_spans(&t);
        assert_eq!(text_of(&t, spans), vec![CARD]);
    }

    #[test]
    fn a_card_after_a_leading_digit_group_is_painted_to_its_last_digit() {
        let t = format!("123456 {CARD}");
        let spans = crate::payment_denylist::card_number_spans(&t);
        assert!(!spans.is_empty());
        let card_start = t.find(CARD).unwrap();
        assert!(
            spans
                .iter()
                .any(|s| s.start <= card_start && s.end == t.len()),
            "{spans:?}"
        );
    }

    #[test]
    fn hyphenated_card_number_is_found() {
        let t = "pay 4111-1111-1111-1111 now";
        let spans = crate::payment_denylist::card_number_spans(t);
        assert_eq!(text_of(t, spans), vec!["4111-1111-1111-1111"]);
    }

    #[test]
    fn non_luhn_digits_are_not_a_card_number() {
        assert!(crate::payment_denylist::card_number_spans("1234567890123456").is_empty());
        assert!(crate::payment_denylist::card_number_spans("phone 555 123 4567").is_empty());
    }

    #[test]
    fn card_number_spans_agree_with_the_boolean_check() {
        for t in [
            CARD,
            "no digits here",
            "1234567890123456",
            "5500 0000 0000 0004",
            "12",
        ] {
            assert_eq!(
                !crate::payment_denylist::card_number_spans(t).is_empty(),
                crate::payment_denylist::contains_card_number(t),
                "{t}"
            );
        }
    }

    #[test]
    fn iban_span_covers_the_iban_and_not_the_label() {
        let t = "IBAN: GB29 NWBK 6016 1331 9268 19 thanks";
        let spans = crate::payment_denylist::iban_spans(t);
        assert_eq!(text_of(t, spans), vec!["GB29 NWBK 6016 1331 9268 19"]);
    }

    #[test]
    fn iban_after_plain_words_in_the_same_run_is_found() {
        let t = "Pay to GB29 NWBK 6016 1331 9268 19";
        let spans = crate::payment_denylist::iban_spans(t);
        assert_eq!(text_of(t, spans), vec!["GB29 NWBK 6016 1331 9268 19"]);
    }

    #[test]
    fn an_ordinary_sentence_has_no_iban() {
        assert!(
            crate::payment_denylist::iban_spans("Please review the attached report today")
                .is_empty()
        );
    }

    #[test]
    fn opaque_token_spans_match_what_the_redactor_scrubs() {
        for t in [
            "key sk-ant-api03-FAKEFAKEFAKEFAKEFAKE1234567890 end",
            "aws wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY ok",
            "plain prose with no secrets at all",
            "https://api.openai.com/v1/responses",
            "C:/Users/someone/AppData/Roaming/thing",
            "abcdefghijklmnopqrstuvwxyz",
        ] {
            let mut rebuilt = String::new();
            let mut at = 0;
            for r in crate::egress::find_opaque_tokens(t) {
                rebuilt.push_str(&t[at..r.start]);
                rebuilt.push_str("[redacted]");
                at = r.end;
            }
            rebuilt.push_str(&t[at..]);
            assert_eq!(rebuilt, crate::egress::redact_opaque_tokens(t), "{t}");
        }
    }

    #[test]
    fn email_spans_find_addresses_and_trim_punctuation() {
        let t = "Mail jane.doe+tag@example.co.uk. Or bob@x.org, thanks";
        assert_eq!(
            text_of(t, email_spans(t)),
            vec!["jane.doe+tag@example.co.uk", "bob@x.org"]
        );
    }

    #[test]
    fn email_spans_reject_things_that_are_not_addresses() {
        for t in ["@handle", "a@b", "a@b.c", "user@", "price @ 5.00", "x@.com"] {
            assert!(email_spans(t).is_empty(), "{t}");
        }
    }

    #[test]
    fn sensitive_spans_collects_every_rule_in_order() {
        let t = format!("me@example.com {CARD}");
        let got = text_of(&t, sensitive_spans(&t));
        assert_eq!(got, vec!["me@example.com", CARD]);
    }

    // -- text redaction (#505) ---------------------------------------------

    /// A fake key, built at run time so no secret-looking literal sits in the
    /// source for the secret scan to find.
    fn fake_key() -> String {
        ["sk-ant-", "api03-", "FAKEFAKEFAKEFAKEFAKE", "1234567890"].concat()
    }

    fn red(text: &str) -> String {
        Redactions::default().redact_text(text)
    }

    #[test]
    fn redact_text_replaces_each_kind_with_a_numbered_marker() {
        let key = fake_key();
        let t = format!(
            "mail me@example.com card {CARD} iban GB29 NWBK 6016 1331 9268 19 key {key} end"
        );
        assert_eq!(
            red(&t),
            "mail [email 1] card [card number 1] iban [IBAN 1] key [key 1] end"
        );
    }

    #[test]
    fn the_same_value_gets_the_same_marker_and_a_new_value_the_next_number() {
        let mut r = Redactions::default();
        let out = r.redact_text("a@x.org then b@x.org then a@x.org");
        assert_eq!(out, "[email 1] then [email 2] then [email 1]");
        // The same instance keeps numbering across calls (one request).
        assert_eq!(r.redact_text("c@x.org b@x.org"), "[email 3] [email 2]");
    }

    #[test]
    fn redact_text_leaves_ordinary_text_alone() {
        for t in [
            "A 2 kg block slides 3.5 m down a 30 degree slope.\nWhat is g?",
            "Internationalization and responsibilities are long words",
            "see 2024-10-15-weekly-sync-notes-final and my_long_snake_case_identifier_2",
            "https://example.com/a/b?c=d",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
    }

    fn jwt() -> String {
        [
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9",
            "eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4tRG9lXyJ9",
            "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c",
        ]
        .join(".")
    }

    #[test]
    fn a_jwt_is_one_key_even_with_dashes_and_underscores() {
        let t = format!("Authorization: Bearer {} thanks", jwt());
        assert_eq!(red(&t), "Authorization: Bearer [key 1] thanks");
        let t2 = format!("token={}.", jwt());
        assert_eq!(red(&t2), "token=[key 1].");
    }

    #[test]
    fn a_pem_private_key_block_is_redacted_through_its_end_line() {
        // Split so no whole PEM header sits in the source for a secret scan.
        const BEGIN: &str = "-----BEGIN ";
        const END: &str = "-----END ";
        let body = "MIIEvQIBADANBgkqhkiG9w0BAQEFAASC+x/Ab12=\nQ3fZ9o+Pk2lm/Zx0aB7y==";
        let t = format!(
            "my key:\n{BEGIN}PRIVATE KEY-----\n{body}\n{END}PRIVATE KEY-----\nwhy does it fail?"
        );
        assert_eq!(red(&t), "my key:\n[key 1]\nwhy does it fail?");
        // A block cut off before its end line goes to the end of the text.
        let cut = format!("{BEGIN}RSA PRIVATE KEY-----\n{body}");
        assert_eq!(red(&cut), "[key 1]");
        // A certificate is not a secret.
        let cert = "-----BEGIN CERTIFICATE-----\nMIIDdzCCAl+gAwIBAgIE\n-----END CERTIFICATE-----";
        assert_eq!(red(cert), cert);
    }

    #[test]
    fn more_vendor_prefixes_are_keys() {
        let keys = [
            ["npm_", "abcdEFGH1234abcdEFGH1234abcdEFGH1234"].concat(),
            [
                "ya29.",
                "a0AfH6SMBx-3Kq_Zp9wT7vLm2NcXy8Rd4-Zp9wT7vLm2NcXy8Rd4Kq",
            ]
            .concat(),
            [
                "SG.",
                "abcdEFGHijklMNOPqrstUV.abcdEFGHijklMNOPqrstUVwxyz0123456789ABCDEFG",
            ]
            .concat(),
            ["shpat_", "0123456789abcdef0123456789abcdef"].concat(),
            ["dop_v1_", "0123456789abcdef0123456789abcdef"].concat(),
            ["gsk_", "abcdEFGH1234abcdEFGH1234"].concat(),
            ["pypi-", "AgEIcHlwaS5vcmcCJGFiY2Q"].concat(),
        ];
        for k in keys {
            assert_eq!(red(&format!("use {k} here")), "use [key 1] here", "{k}");
        }
    }

    #[test]
    fn the_screenshot_rule_finds_the_same_shapes() {
        let key = ["npm_", "abcdEFGH1234abcd"].concat();
        let t = format!("{} and {key}", jwt());
        let spans = sensitive_spans(&t);
        let got = text_of(&t, spans);
        assert!(got.contains(&jwt().as_str()), "{got:?}");
        assert!(got.contains(&key.as_str()), "{got:?}");
    }

    #[test]
    fn many_pem_headers_without_newlines_are_scanned_in_linear_time() {
        let t = "-----BEGIN ".repeat(100_000);
        let start = std::time::Instant::now();
        let _ = red(&t);
        let t2 = "-----BEGIN PRIVATE KEY-----".repeat(20_000);
        let _ = red(&t2);
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "took {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn a_cut_off_pem_block_stops_at_a_blank_line_not_the_end_of_the_text() {
        const BEGIN: &str = "-----BEGIN ";
        let t = format!("{BEGIN}EC PRIVATE KEY-----\nMHcCAQEEIA+x/Ab12=\n\nwhy does it fail?");
        assert_eq!(red(&t), "[key 1]\n\nwhy does it fail?");
        let pgp = format!("{BEGIN}PGP PRIVATE KEY BLOCK-----\nlQdGBF+x/Ab12=\n-----END PGP PRIVATE KEY BLOCK-----\nok");
        assert_eq!(red(&pgp), "[key 1]\nok");
        // An END line of another label does not end the block.
        let mixed = format!("{BEGIN}PRIVATE KEY-----\nabc\n-----END CERTIFICATE-----\nmore");
        assert!(red(&mixed).contains("[key 1]"), "{}", red(&mixed));
    }

    #[test]
    fn dotted_vendor_keys_need_their_structure() {
        for t in [
            "open SG.Reports.Quarterly.xlsx now",
            "see SG.team-notes-2024.final.docx",
            "token ya29.short",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
        let sg = format!("SG.aB3dE6gH9jK2mN5pQ8sT1v.{}", "x".repeat(43));
        assert_eq!(red(&format!("k {sg} z")), "k [key 1] z");
    }

    #[test]
    fn prefixed_names_that_are_not_key_material_are_left_alone() {
        for t in [
            "set npm_config_cache_dir to a folder",
            "npm_lifecycle_event is test",
            "hf_hub_offline_mode=1",
            "sbp_command_line_options",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
    }

    #[test]
    fn short_look_alikes_of_the_new_shapes_are_left_alone() {
        for t in [
            "see 2024-10-15-weekly-sync-notes and npm_config",
            "version 1.2.3 of eyJ.x and SG.1 and ya29.",
            "-----BEGIN CERTIFICATE----- only",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
    }

    #[test]
    fn a_google_docs_url_with_a_long_mixed_case_id_survives() {
        for t in [
            "open https://docs.google.com/document/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/edit now",
            "docs.google.com/spreadsheets/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/edit#gid=0",
            "(https://drive.google.com/file/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms/view)",
            &format!("https://example.com/reset?{}={}", "t", "Ab3dE6gH9jK2mN5pQ8sT1vX4zC7fL0"),
        ] {
            assert_eq!(red(t), t, "{t}");
        }
    }

    #[test]
    fn a_vendor_prefixed_key_is_redacted_even_beside_a_url() {
        let key = fake_key();
        let t = format!("curl -H {key} and https://docs.google.com/document/d/1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms");
        let got = red(&t);
        assert!(
            got.starts_with("curl -H [key 1] and https://docs.google.com/"),
            "{got}"
        );
        assert!(
            got.contains("1BxiMVs0XRA5nFMdKvBdBZjgmUUqptlbs74OgvE2upms"),
            "{got}"
        );
    }

    #[test]
    fn a_long_bare_alphanumeric_run_with_a_digit_is_a_key() {
        assert_eq!(
            red(&format!(
                "token {} ok",
                ["wJalrXUtnFEMIK7MDENG", "bPxRfiCYEXAMPLEKEY1"].concat()
            )),
            "token [key 1] ok"
        );
    }

    #[test]
    fn a_luhn_valid_run_with_no_card_prefix_is_not_a_card() {
        // Valid Luhn, but they start with 1 and 9: tracking and order numbers.
        assert!(crate::payment_denylist::card_number_spans("1234567890000007").len() == 1);
        for t in [
            "tracking 1234567890000007",
            "order 9234567890000000 shipped",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
        assert_eq!(red(&format!("pay {CARD}")), "pay [card number 1]");
    }

    #[test]
    fn a_card_number_split_by_a_line_break_is_not_one_run() {
        let t = "4111 1111\n1111 1111";
        assert_eq!(red(t), t);
    }

    #[test]
    fn redact_text_handles_multi_line_and_non_ascii_text() {
        let t = format!("naïve line one\nCard: {CARD}\nbob@x.org – fin");
        assert_eq!(
            red(&t),
            "naïve line one\nCard: [card number 1]\n[email 1] – fin"
        );
    }

    #[test]
    fn redact_text_uses_one_marker_for_overlapping_matches() {
        // The key contains digits that also pass Luhn.
        let t = format!(
            "k {}4111111111111111FAKEFAKEFAKE9 z",
            ["sk-ant-", "api03-"].concat()
        );
        let got = red(&t);
        assert!(!got.contains("4111"), "{got}");
        assert_eq!(got, "k [key 1] z");
    }

    #[test]
    fn a_vendor_key_inside_a_url_is_still_a_key() {
        let key = ["AIza", "SyA1234567890abcdefgh"].concat();
        let t = format!("https://x.com/a?key={key}");
        assert_eq!(red(&t), "https://x.com/a?key=[key 1]");
    }

    #[test]
    fn a_mastercard_two_series_number_is_a_card() {
        // Luhn-valid, first four digits 2223 (inside 2221 to 2720).
        assert_eq!(red("pay 2223000048400011 now"), "pay [card number 1] now");
        // Luhn-valid but outside the range (2720 is the top, this is 2800).
        assert_eq!(red("order 2800000000000004"), "order 2800000000000004",);
    }

    #[test]
    fn aws_key_ids_need_the_exact_shape() {
        let id = ["AKIA", "IOSFODNN7EXAMPLE"].concat();
        assert_eq!(red(&format!("id {id} ok")), "id [key 1] ok");
        let temp = ["ASIA", "IOSFODNN7EXAMPLE"].concat();
        assert_eq!(red(&format!("({temp})")), "([key 1])");
        for t in [
            "ASIA-PACIFIC-REGION-REPORT",
            "AKIAS are a thing",
            "AKIAIOSFODNN7EXAMPLE1",
            "ASIAN-MARKET-REVIEW-2026",
        ] {
            assert_eq!(red(t), t, "{t}");
        }
    }

    #[test]
    fn numbering_starts_above_markers_already_in_the_input() {
        let mut r = Redactions::default();
        let out = r.redact_text("see [email 1] and a@x.org");
        assert_eq!(out, "see [email 1] and [email 2]");
        // Only the real one comes back; the literal text is left alone.
        assert_eq!(
            r.restore_in_json(r#"["[email 1]","[email 2]"]"#.to_string()),
            r#"["[email 1]","a@x.org"]"#
        );
    }

    #[test]
    fn a_literal_marker_in_a_later_field_is_still_not_reused() {
        let mut v = serde_json::json!({"a": "write a@x.org", "b": "keep [email 1] as is"});
        let mut r = Redactions::default();
        r.redact_value(&mut v);
        assert_eq!(v["a"], "write [email 2]");
        assert_eq!(v["b"], "keep [email 1] as is");
    }

    #[test]
    fn overlapping_matches_store_the_whole_union_as_the_original() {
        // The key-shaped run and the address share the start; the address
        // runs on past the key, so the union is the whole thing.
        let key = ["sk-ant-", "api03-", "FAKEFAKEFAKE12345"].concat();
        let t = format!("send {key}@example.com now");
        let mut r = Redactions::default();
        let out = r.redact_text(&t);
        assert_eq!(out, "send [key 1] now");
        assert_eq!(r.restore_in_json(format!("\"{out}\"")), format!("\"{t}\""));
    }

    #[test]
    fn restore_puts_the_real_values_back_and_leaves_invented_markers() {
        let mut r = Redactions::default();
        r.redact_text(&format!("me@example.com and {CARD}"));
        let answer = r#"{"to":"[email 1]","card":"[card number 1]","other":"[email 7]"}"#;
        assert_eq!(
            r.restore_in_json(answer.to_string()),
            format!(r#"{{"to":"me@example.com","card":"{CARD}","other":"[email 7]"}}"#)
        );
    }

    #[test]
    fn restore_escapes_the_value_so_the_json_stays_valid() {
        let mut r = Redactions::default();
        // A value with a quote and a backslash cannot come from the real
        // rules, so plant one to prove the escaping on its own.
        r.pairs.push(("[key 1]".to_string(), "a\"b\\c".to_string()));
        let out = r.restore_in_json(r#"{"k":"[key 1]"}"#.to_string());
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["k"], "a\"b\\c");
    }

    #[test]
    fn restore_with_nothing_redacted_returns_the_response_untouched() {
        let r = Redactions::default();
        assert!(r.is_empty());
        assert_eq!(r.restore_in_json("[email 1]".to_string()), "[email 1]");
    }

    #[test]
    fn debug_never_prints_the_originals() {
        let mut r = Redactions::default();
        r.redact_text("me@example.com");
        let shown = format!("{r:?}");
        assert!(!shown.contains("example"), "{shown}");
    }

    #[test]
    fn redact_value_redacts_nested_strings_but_not_structure_or_images() {
        let png = format!("iVBORw0KGgo{}", "A".repeat(60));
        let mut v = serde_json::json!({
            "model": "gpt-5-mini",
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "mail me@example.com"},
                    {"type": "image_url", "image_url": {"url": format!("data:image/png;base64,{png}")}}
                ]}
            ],
            "images": [png.clone()],
            "options": {"num_ctx": 4096}
        });
        let mut r = Redactions::default();
        assert!(r.redact_value(&mut v));
        assert_eq!(v["messages"][0]["content"][0]["text"], "mail [email 1]");
        assert_eq!(v["model"], "gpt-5-mini");
        assert_eq!(v["messages"][0]["role"], "user");
        assert_eq!(v["images"][0], png.as_str());
        assert_eq!(
            v["messages"][0]["content"][1]["image_url"]["url"],
            format!("data:image/png;base64,{png}")
        );
        assert_eq!(v["options"]["num_ctx"], 4096);
    }

    #[test]
    fn redact_value_reports_no_change_for_clean_bodies() {
        let mut v = serde_json::json!({"model": "m", "prompt": "hello there"});
        let before = v.clone();
        let mut r = Redactions::default();
        assert!(!r.redact_value(&mut v));
        assert!(r.is_empty());
        assert_eq!(v, before);
    }

    #[test]
    fn every_registered_schema_and_the_system_prompt_survive_redaction_unchanged() {
        // Every real request carries one of these. A rule that rewrote them
        // would silently break every cloud request.
        let ids = [
            "verdict",
            "calendar_event",
            "text_review",
            "form_fill",
            "text_answer",
            "action_items",
            "table",
            "error_explanation",
            "word_definition",
            "page_summary",
            "screen_explanation",
            "time_list",
            "scam_check",
        ];
        for id in ids {
            for want_difficulty in [false, true] {
                let schema = crate::actions::schema::schema_for(id, want_difficulty)
                    .unwrap_or_else(|| panic!("{id} has no schema"));
                let mut v = serde_json::json!({
                    "schema": schema,
                    "system": crate::provider::common::augmented_system_prompt(
                        "You check physics problems.", want_difficulty),
                });
                let before = v.clone();
                let mut r = Redactions::default();
                assert!(!r.redact_value(&mut v), "{id}");
                assert_eq!(v, before, "{id}");
            }
        }
    }

    // -- word mapping ----------------------------------------------------

    fn word(text: &str, x: f32) -> OcrWord {
        OcrWord {
            text: text.to_string(),
            x,
            y: 10.0,
            width: 40.0,
            height: 12.0,
        }
    }

    fn card_line() -> OcrLine {
        // "Card 4111 1111 1111 1111 ok": words 40 px wide, 50 px apart.
        let words: Vec<OcrWord> = ["Card", "4111", "1111", "1111", "1111", "ok"]
            .iter()
            .enumerate()
            .map(|(i, t)| word(t, 100.0 + 50.0 * i as f32))
            .collect();
        OcrLine {
            text: format!("Card {CARD} ok"),
            x: 100.0,
            y: 10.0,
            width: 290.0,
            height: 12.0,
            words,
        }
    }

    #[test]
    fn word_byte_ranges_follow_the_text_in_order() {
        let l = card_line();
        let r = word_byte_ranges(&l.text, &l.words).unwrap();
        assert_eq!(&l.text[r[0].clone()], "Card");
        assert_eq!(&l.text[r[3].clone()], "1111");
        assert_eq!(&l.text[r[5].clone()], "ok");
    }

    #[test]
    fn word_byte_ranges_is_none_when_a_word_is_missing() {
        let l = card_line();
        assert!(word_byte_ranges("something else entirely", &l.words).is_none());
    }

    #[test]
    fn a_card_number_paints_only_its_words_not_the_whole_line() {
        let rects = regions_for_lines(&[card_line()], 1000, 200);
        assert_eq!(rects.len(), 1);
        let r = rects[0];
        // Words 1..=4 span x 150..340; "Card" (100..140) and "ok" (350..390)
        // are outside it, padded by PAD.
        assert_eq!((r.x0, r.x1), (147, 343));
        assert_eq!((r.y0, r.y1), (7, 25));
    }

    #[test]
    fn a_line_with_nothing_sensitive_paints_nothing() {
        let mut l = card_line();
        l.text = "Card number 1234 5678".to_string();
        l.words.clear();
        assert!(regions_for_lines(&[l], 1000, 200).is_empty());
    }

    #[test]
    fn a_line_whose_words_do_not_match_is_painted_whole() {
        let mut l = card_line();
        l.words = vec![word("garbage", 0.0)];
        let rects = regions_for_lines(&[l], 1000, 200);
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].x0, rects[0].x1), (97, 393));
    }

    #[test]
    fn regions_are_clipped_to_the_image() {
        let mut l = card_line();
        l.x = -20.0;
        l.words.clear();
        let rects = regions_for_lines(&[l], 200, 15);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].x0, 0);
        assert_eq!(rects[0].x1, 200);
        assert_eq!(rects[0].y1, 15);
    }

    #[test]
    fn a_region_wholly_off_the_image_is_dropped() {
        let mut l = card_line();
        l.words.clear();
        assert!(regions_for_lines(&[l], 50, 200).is_empty());
    }

    // -- password fields (#506) ------------------------------------------

    fn sr(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_password_rect_becomes_a_padded_image_region() {
        let map = ScreenMap::for_capture(100, 0, 200, 100, 200, 100, None);
        let r = password_regions(&[sr(150, 40, 180, 50)], &map, 200, 100);
        assert_eq!(
            r,
            vec![PixelRect {
                x0: 47,
                y0: 37,
                x1: 83,
                y1: 53
            }]
        );
    }

    #[test]
    fn a_password_rect_on_a_downscaled_shot_lands_where_the_pixels_are() {
        // 400x200 monitor shown at 200x100.
        let map = ScreenMap::for_capture(0, 0, 400, 200, 200, 100, None);
        let r = password_regions(&[sr(100, 100, 200, 120)], &map, 200, 100);
        assert_eq!(r.len(), 1);
        assert!(r[0].x0 <= 50 && r[0].x1 >= 100 && r[0].y0 <= 50 && r[0].y1 >= 60);
        assert!(r[0].x1 <= 103 && r[0].y1 <= 63, "{r:?}");
    }

    #[test]
    fn password_rects_off_the_image_or_empty_paint_nothing_and_never_panic() {
        let map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        let rects = [
            sr(500, 500, 600, 600),
            sr(-500, -500, -400, -400),
            sr(10, 10, 10, 10),
            sr(50, 50, 20, 20),
        ];
        assert!(password_regions(&rects, &map, 100, 100).is_empty());
        // A rect that covers everything is clipped to the image.
        let all = password_regions(
            &[sr(i32::MIN, i32::MIN, i32::MAX, i32::MAX)],
            &map,
            100,
            100,
        );
        assert_eq!(
            all,
            vec![PixelRect {
                x0: 0,
                y0: 0,
                x1: 100,
                y1: 100
            }]
        );
    }

    #[test]
    fn a_password_rect_overlapping_one_edge_is_clipped_to_the_image() {
        let map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        let r = password_regions(&[sr(80, 80, 140, 120)], &map, 100, 100);
        assert_eq!(
            r,
            vec![PixelRect {
                x0: 77,
                y0: 77,
                x1: 100,
                y1: 100
            }]
        );
    }

    #[test]
    fn painting_a_password_region_boxes_it_and_leaves_the_rest() {
        let mut raw = white(100, 100);
        let map = ScreenMap::for_capture(0, 0, 100, 100, 100, 100, None);
        let regions = password_regions(&[sr(40, 40, 60, 50)], &map, 100, 100);
        paint(&mut raw, &regions);
        let px = |x: usize, y: usize| raw.rgba[(y * 100 + x) * 4..(y * 100 + x) * 4 + 4].to_vec();
        assert_eq!(px(50, 45), FILL.to_vec());
        assert_eq!(px(10, 10), vec![255; 4]);
    }

    #[test]
    fn painted_password_fields_are_counted_for_the_egress_log() {
        reset_image_counts();
        let mut raw = white(100, 100);
        let regions = [
            PixelRect {
                x0: 10,
                y0: 10,
                x1: 30,
                y1: 20,
            },
            PixelRect {
                x0: 10,
                y0: 40,
                x1: 30,
                y1: 50,
            },
        ];
        assert_eq!(paint_password_fields(&mut raw, &regions), 2);
        assert_eq!(
            counts_of(image_counts()),
            vec![("password field".to_string(), 2)]
        );
        reset_image_counts();
    }

    #[test]
    fn apply_looks_for_password_fields_and_real_captures_carry_a_screen_map() {
        // Wiring (#506): a password lookup nobody calls, or a capture with no
        // screen map, would pass every pure test above and paint nothing.
        let norm = |s: &str| s.replace('\r', "");
        let redact = norm(include_str!("redact.rs"));
        let apply_at = redact.find("pub fn apply(raw").expect("apply exists");
        let apply_body = &redact[apply_at..redact[apply_at..].find("\n}\n").unwrap() + apply_at];
        assert!(
            apply_body.contains("apply_password_fields(raw)?"),
            "{apply_body}"
        );

        let capture = norm(include_str!("capture.rs"));
        for f in ["pub fn grab_raw_for(", "pub fn grab_virtual_desktop_raw("] {
            let at = capture.find(f).unwrap_or_else(|| panic!("{f} exists"));
            let body = &capture[at..at + capture[at..].find("\n}\n").unwrap()];
            assert!(
                body.contains("ScreenMap::for_capture(") && body.contains("screen: Some("),
                "{f}"
            );
        }
        let at = capture.find("pub fn grab_raw_for(").unwrap();
        let body = &capture[at..at + capture[at..].find("\n}\n").unwrap()];
        assert!(body.contains("PasswordLookup::start"), "grab_raw_for");
        // The decision to look is one helper, used by the gate and by the
        // capture call sites.
        assert!(capture.contains("if needs_password_lookup(redact_setting, providers, mode)"));
        let app = norm(include_str!("app.rs"));
        assert_eq!(app.matches("capture::needs_password_lookup(").count(), 2);
        // The router's capture names the window under the palette.
        let app = norm(include_str!("app.rs"));
        let at = app.find("fn maybe_start_router(").expect("router fn");
        let body = &app[at..at + app[at..].find("\n    }\n").unwrap()];
        assert!(
            body.contains("LookupWindow::Window(underlying_window)"),
            "{body}"
        );
    }

    #[test]
    fn a_password_manager_window_is_painted_and_counted() {
        reset_image_counts();
        let mut raw = shot_with(None);
        raw.screen.as_mut().unwrap().manager_windows = Ok(vec![sr(50, 0, 100, 100)]);
        assert_eq!(apply_manager_windows(&mut raw).unwrap(), 1);
        assert_eq!(&raw.rgba[(50 * 100 + 75) * 4..][..4], &FILL);
        assert_eq!(&raw.rgba[(50 * 100 + 10) * 4..][..4], &[255; 4]);
        assert_eq!(image_counts().get("password manager"), Some(&1));
    }

    #[test]
    fn manager_windows_off_the_image_paint_nothing_and_a_bare_shot_is_left_alone() {
        let mut raw = shot_with(None);
        raw.screen.as_mut().unwrap().manager_windows = Ok(vec![
            sr(500, 500, 600, 600),
            sr(i32::MIN, i32::MIN, i32::MIN + 5, i32::MIN + 5),
            sr(30, 30, 10, 10),
        ]);
        assert_eq!(apply_manager_windows(&mut raw).unwrap(), 0);
        assert!(raw.rgba.iter().all(|b| *b == 255));
        let mut bare = white(10, 10);
        assert_eq!(apply_manager_windows(&mut bare).unwrap(), 0);
    }

    #[test]
    fn a_manager_list_that_could_not_be_built_fails_closed_and_paints_nothing() {
        let mut raw = shot_with(None);
        raw.screen.as_mut().unwrap().manager_windows = Err("too many".to_string());
        let err = apply_manager_windows(&mut raw).unwrap_err();
        assert!(format!("{err:#}").contains(FAILURE_NEEDLE), "{err:#}");
        assert!(raw.rgba.iter().all(|b| *b == 255));
    }

    #[test]
    fn apply_paints_manager_windows_and_capture_collects_them_for_a_looked_up_shot() {
        // Wiring (#575): rectangles nobody collects, or a paint nobody calls,
        // would pass the pure tests above and leave the manager in the image.
        let norm = |s: &str| s.replace('\r', "");
        let redact = norm(include_str!("redact.rs"));
        let at = redact.find("pub fn apply(raw").expect("apply exists");
        let body = &redact[at..redact[at..]
            .find(
                "
}
",
            )
            .unwrap()
            + at];
        assert!(body.contains("apply_manager_windows(raw)"), "{body}");
        let capture = norm(include_str!("capture.rs"));
        let at = capture.find("pub fn grab_raw_for(").unwrap();
        let body = &capture[at..at
            + capture[at..]
                .find(
                    "
}
",
                )
                .unwrap()];
        assert!(
            body.contains("screen.manager_windows = managers;")
                && body.contains("manager_windows_on("),
            "{body}"
        );
    }

    fn shot_with(lookup: Option<crate::inputs::uia::PasswordLookup>) -> RawShot {
        let mut raw = white(100, 100);
        raw.screen = Some(ScreenMap::for_capture(0, 0, 100, 100, 100, 100, lookup));
        raw
    }

    #[test]
    fn a_finished_lookup_is_painted_without_waiting() {
        use crate::inputs::uia::PasswordLookup;
        let mut raw = shot_with(Some(PasswordLookup::finished(Ok(vec![sr(40, 40, 60, 50)]))));
        assert_eq!(apply_password_fields(&mut raw).unwrap(), 1);
        assert_eq!(&raw.rgba[(45 * 100 + 50) * 4..][..4], &FILL);
        assert_eq!(&raw.rgba[..4], &[255; 4]);
    }

    #[test]
    fn timeouts_failures_and_a_missing_thread_fail_closed_with_the_generic_card() {
        use crate::inputs::uia::{LookupError, PasswordLookup};
        for lookup in [
            PasswordLookup::never_finishing(Duration::from_secs(10)),
            PasswordLookup::finished(Err(LookupError::TimedOut)),
            PasswordLookup::finished(Err(LookupError::Failed)),
        ] {
            let mut raw = shot_with(Some(lookup));
            let err = apply_password_fields(&mut raw).unwrap_err();
            assert!(format!("{err:#}").contains(FAILURE_NEEDLE), "{err:#}");
        }
    }

    #[test]
    fn an_administrator_app_fails_closed_with_its_own_sentence() {
        use crate::inputs::uia::{LookupError, PasswordLookup};
        let mut raw = shot_with(Some(PasswordLookup::finished(Err(
            LookupError::AccessDenied,
        ))));
        let err = apply_password_fields(&mut raw).unwrap_err();
        assert_eq!(format!("{err:#}"), ADMIN_APP_SENTENCE);
        assert!(!ADMIN_APP_SENTENCE.contains('\u{2014}'));
    }

    #[test]
    fn a_map_with_no_lookup_is_refused_while_redacting() {
        let mut raw = shot_with(None);
        let err = apply_password_fields(&mut raw).unwrap_err();
        assert!(format!("{err:#}").contains(FAILURE_NEEDLE), "{err:#}");
    }

    #[test]
    fn a_shot_with_no_screen_map_has_no_password_fields_to_paint() {
        let mut raw = white(10, 10);
        assert_eq!(apply_password_fields(&mut raw).unwrap(), 0);
        assert!(raw.rgba.iter().all(|b| *b == 255));
    }

    // -- painting --------------------------------------------------------

    fn white(w: u32, h: u32) -> RawShot {
        RawShot {
            rgba: vec![255; (w * h * 4) as usize],
            width: w,
            height: h,
            screen: None,
        }
    }

    #[test]
    fn paint_fills_inside_the_rect_and_leaves_the_rest_alone() {
        let mut raw = white(20, 10);
        paint(
            &mut raw,
            &[PixelRect {
                x0: 5,
                y0: 2,
                x1: 12,
                y1: 6,
            }],
        );
        for y in 0..10u32 {
            for x in 0..20u32 {
                let i = ((y * 20 + x) * 4) as usize;
                let inside = (5..12).contains(&x) && (2..6).contains(&y);
                let want: [u8; 4] = if inside { FILL } else { [255; 4] };
                assert_eq!(raw.rgba[i..i + 4], want, "({x},{y})");
            }
        }
    }

    #[test]
    fn paint_tolerates_a_rect_past_the_edge() {
        let mut raw = white(4, 4);
        paint(
            &mut raw,
            &[PixelRect {
                x0: 2,
                y0: 2,
                x1: 99,
                y1: 99,
            }],
        );
        assert_eq!(raw.rgba[(3 * 4 + 3) * 4..][..4], FILL);
        assert_eq!(raw.rgba[0..4], [255; 4]);
    }

    #[test]
    fn apply_lines_counts_regions_and_paints_them() {
        let mut raw = white(1000, 200);
        let n = apply_lines(&mut raw, &[card_line()]);
        assert_eq!(n, 1);
        // Centre of the number is painted; the "Card" word is not.
        let px = |x: usize, y: usize| raw.rgba[(y * 1000 + x) * 4..][..4].to_vec();
        assert_eq!(px(250, 16), FILL.to_vec());
        assert_eq!(px(120, 16), vec![255; 4]);
    }

    #[test]
    fn nothing_sensitive_leaves_the_pixels_untouched() {
        let mut raw = white(100, 20);
        let before = raw.rgba.clone();
        let mut l = card_line();
        l.text = "hello world".to_string();
        l.words.clear();
        assert_eq!(apply_lines(&mut raw, &[l]), 0);
        assert_eq!(raw.rgba, before);
    }

    // -- counts for the egress log (#508) ---------------------------------

    fn counts_of(m: std::collections::BTreeMap<String, usize>) -> Vec<(String, usize)> {
        m.into_iter().collect()
    }

    #[test]
    fn text_redactions_are_counted_by_kind_not_by_value() {
        let mut r = Redactions::default();
        r.redact_text("a@example.com b@example.com a@example.com paid with 4111 1111 1111 1111");
        assert_eq!(
            counts_of(r.counts()),
            vec![("card number".to_string(), 1), ("email".to_string(), 2)]
        );
    }

    #[test]
    fn nothing_redacted_counts_nothing() {
        let mut r = Redactions::default();
        r.redact_text("plain words only");
        assert!(r.counts().is_empty());
    }

    #[test]
    fn painted_regions_are_counted_by_kind() {
        reset_image_counts();
        let mut raw = white(1000, 200);
        apply_lines(&mut raw, &[card_line()]);
        assert_eq!(
            counts_of(image_counts()),
            vec![("card number".to_string(), 1)]
        );
        reset_image_counts();
        assert!(image_counts().is_empty());
    }

    #[test]
    fn image_counts_add_up_across_notes() {
        reset_image_counts();
        note_image_redactions("password field", 2);
        note_image_redactions("password field", 1);
        note_image_redactions("email", 0);
        assert_eq!(
            counts_of(image_counts()),
            vec![("password field".to_string(), 3)]
        );
        reset_image_counts();
    }

    // -- live (needs the real OCR engine) ---------------------------------

    /// The observable for "is redaction wired": render a card number with GDI,
    /// run the real OCR stage and the real encode, decode the PNG a provider
    /// would receive, and check the region is the fill colour and the
    /// unrelated corner is still white.
    ///
    /// Run manually (never a bare `cargo test`):
    /// `cargo test redact_live -- --ignored --nocapture`
    #[test]
    #[ignore = "uses the real Windows OCR engine"]
    fn redact_live_paints_a_gdi_rendered_card_number_in_the_encoded_png() {
        // A varied, Luhn-valid test number. MEASURED 2026-10-15: the Windows
        // OCR engine returns no line at all for "Card 4111 1111 1111 1111"
        // (the same four digits repeated), so that classic test number
        // cannot be used here. The text is centred: "Card" ends near x=295
        // and the number spans about x=305..690 in this 900x160 image.
        let (w, h) = (900u32, 160u32);
        let rgba =
            unsafe { crate::ocr::tests::render_gdi_text_rgba("Card 4539 1488 0343 6467", w, h) };
        let mut raw = RawShot {
            rgba,
            width: w,
            height: h,
            screen: None,
        };
        assert!(
            raw.rgba.contains(&0),
            "the render must have drawn dark text before redaction"
        );

        let started = std::time::Instant::now();
        let painted = apply(&mut raw).expect("OCR must run");
        eprintln!(
            "redact::apply took {} ms, {painted} region(s)",
            started.elapsed().as_millis()
        );
        assert!(painted >= 1, "the card number must be found");

        // What a provider would receive: the real encode, then decode.
        let shot = crate::capture::encode_unredacted(&raw).expect("encode");
        let decoded = image::load_from_memory(&shot.png)
            .expect("decode PNG")
            .to_rgba8();
        assert_eq!(decoded.get_pixel(w / 2, h / 2).0, FILL, "number is boxed");
        assert_eq!(decoded.get_pixel(2, 2).0, [255; 4], "corner is untouched");
        let glyph = |x: u32, y: u32| {
            let p = decoded.get_pixel(x, y).0;
            p != [255; 4] && p != FILL
        };
        let number_glyphs = (0..h)
            .flat_map(|y| (315..680).map(move |x| (x, y)))
            .filter(|&(x, y)| glyph(x, y))
            .count();
        assert_eq!(number_glyphs, 0, "no digit pixels may survive the box");
        let label_glyphs = (0..h)
            .flat_map(|y| (200..295).map(move |x| (x, y)))
            .filter(|&(x, y)| glyph(x, y))
            .count();
        assert!(label_glyphs > 0, "the word 'Card' is not private and stays");
    }
}
