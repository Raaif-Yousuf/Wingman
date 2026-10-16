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

use crate::capture::RawShot;
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
pub fn sensitive_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = crate::payment_denylist::card_number_spans(text);
    spans.extend(crate::payment_denylist::iban_spans(text));
    spans.extend(crate::egress::find_opaque_tokens(text));
    spans.extend(email_spans(text));
    spans.sort_by_key(|r| (r.start, r.end));
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
/// [`MIN_PREFIXED_KEY_CHARS`] characters on.
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
    "github_pat_",
    "xoxb-",
    "xoxp-",
    "xoxa-",
    "glpat-",
    "hf_",
    "whsec_",
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

/// A key with a recognisable vendor shape. Safe to apply inside a link too:
/// a vendor prefix is not something a document id happens to start with.
fn is_prefixed_key_run(run: &str) -> bool {
    is_aws_key_id(run)
        || (run.len() >= MIN_PREFIXED_KEY_CHARS && KEY_PREFIXES.iter().any(|p| run.starts_with(p)))
}

/// A long letters-and-digits run with no vendor shape. Not applied inside a
/// link, where long mixed-case ids are ordinary.
fn is_bare_key_run(run: &str) -> bool {
    run.len() >= MIN_BARE_KEY_CHARS
        && run.bytes().all(|b| b.is_ascii_alphanumeric())
        && run.bytes().any(|b| b.is_ascii_digit())
        && run.bytes().any(|b| b.is_ascii_alphabetic())
}

/// Key-shaped runs inside one whitespace-free chunk of `text`. A URL-shaped
/// chunk only gets the vendor-shape rule, never the bare-run rule.
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
                if is_prefixed_key_run(run) || (!in_link && is_bare_key_run(run)) {
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
pub fn regions_for_lines(lines: &[OcrLine], width: u32, height: u32) -> Vec<PixelRect> {
    let mut out = Vec::new();
    for line in lines {
        let spans = sensitive_spans(&line.text);
        if spans.is_empty() {
            continue;
        }
        let ranges = if line.words.is_empty() {
            None
        } else {
            word_byte_ranges(&line.text, &line.words)
        };
        for span in spans {
            let rect = match &ranges {
                Some(ranges) => union_of_overlapping(&line.words, ranges, &span),
                None => None,
            }
            .unwrap_or((line.x, line.y, line.x + line.width, line.y + line.height));
            if let Some(r) = clip(rect, width, height) {
                out.push(r);
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

/// Paints `lines`' sensitive regions into `raw` and returns how many regions
/// were painted. The pure half of [`apply`].
pub fn apply_lines(raw: &mut RawShot, lines: &[OcrLine]) -> usize {
    let rects = regions_for_lines(lines, raw.width, raw.height);
    paint(raw, &rects);
    rects.len()
}

/// Runs OCR over `raw` and paints over everything sensitive it reads. Returns
/// the number of regions painted. Blocks for the OCR call: worker thread only.
///
/// An OCR failure is an `Err`, never "nothing found": the caller must not
/// send a screenshot nobody checked.
pub fn apply(raw: &mut RawShot) -> Result<usize> {
    let out = crate::ocr::recognize(&raw.rgba, raw.width, raw.height, OCR_TIMEOUT)
        .context(FAILURE_NEEDLE)?;
    Ok(apply_lines(raw, &out.lines))
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

    // -- painting --------------------------------------------------------

    fn white(w: u32, h: u32) -> RawShot {
        RawShot {
            rgba: vec![255; (w * h * 4) as usize],
            width: w,
            height: h,
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
