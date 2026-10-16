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
    "Wingman can't check an app running as administrator for password fields, so nothing was sent.";

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
/// run can contain digits that also pass Luhn); callers paint each one.
pub fn sensitive_spans(text: &str) -> Vec<Range<usize>> {
    let mut spans = crate::payment_denylist::card_number_spans(text);
    spans.extend(crate::payment_denylist::iban_spans(text));
    spans.extend(crate::egress::find_opaque_tokens(text));
    spans.extend(email_spans(text));
    spans.sort_by_key(|r| (r.start, r.end));
    spans
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
    let Some(lookup) = &map.lookup else {
        return Ok(0);
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
    paint(raw, &regions);
    Ok(regions.len())
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
    // Password fields: OCR cannot read a masked field, and a revealed one is
    // not key-shaped (#506). A lookup that fails fails closed, like OCR.
    let passwords = apply_password_fields(raw)?;
    Ok(apply_lines(raw, &out.lines) + passwords)
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
            assert!(body.contains("screen: Some(ScreenMap::for_capture("), "{f}");
            assert!(body.contains("PasswordLookup::start("), "{f}");
        }
        // The router's capture names the window under the palette.
        let app = norm(include_str!("app.rs"));
        let at = app.find("fn maybe_start_router(").expect("router fn");
        let body = &app[at..at + app[at..].find("\n    }\n").unwrap()];
        assert!(
            body.contains("LookupWindow::Window(underlying_window)"),
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
    fn a_map_with_no_lookup_paints_nothing() {
        let mut raw = shot_with(None);
        assert_eq!(apply_password_fields(&mut raw).unwrap(), 0);
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
