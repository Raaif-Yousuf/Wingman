//! Masks card numbers, IBANs and national-id numbers in text before it is
//! chunked, so the store never holds one (spec: "Mask payment and national-id
//! shaped text"). A contract with an IBAN stays searchable; the database
//! just never holds the number.
//!
//! The whole parsed text is masked, not each chunk, so a number that falls
//! across the overlap of two chunks cannot survive in either, and the chunker's
//! offsets describe the text that was stored.
//!
//! Fail closed: the scans are pure string functions with no failure path. A
//! caller that cannot run this must not store the text.

use crate::payment_denylist::{card_number_spans, iban_spans, national_id_spans};
use std::ops::Range;

/// `text` with every card number, IBAN and national id replaced by a marker.
pub fn mask_sensitive(text: &str) -> String {
    let mut spans: Vec<(Range<usize>, &'static str)> = Vec::new();
    spans.extend(
        card_number_spans(text)
            .into_iter()
            .map(|r| (r, "[card number]")),
    );
    spans.extend(iban_spans(text).into_iter().map(|r| (r, "[IBAN]")));
    spans.extend(
        national_id_spans(text)
            .into_iter()
            .map(|r| (r, "[national id]")),
    );
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_by_key(|(r, _)| (r.start, r.end));
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (range, marker) in spans {
        if range.start < at {
            // Overlaps what was just masked: keep the earlier marker, but
            // extend past this span if it reaches further.
            at = at.max(range.end);
            continue;
        }
        let (Some(before), true) = (text.get(at..range.start), text.is_char_boundary(range.end))
        else {
            // A span that is not on character boundaries cannot be cut
            // cleanly; drop the rest rather than store unmasked text.
            return out;
        };
        out.push_str(before);
        out.push_str(marker);
        at = range.end;
    }
    out.push_str(text.get(at..).unwrap_or(""));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn card_iban_and_national_id_become_markers() {
        let text = "Card 4111 1111 1111 1111, IBAN GB82 WEST 1234 5698 7654 32, SSN 123-45-6789.";
        let masked = mask_sensitive(text);
        assert_eq!(
            masked,
            "Card [card number], IBAN [IBAN], SSN [national id]."
        );
    }

    #[test]
    fn ordinary_text_is_unchanged() {
        let text = "Call +1 555-123-4567 about SW1A 1AA, order 20240101.";
        assert_eq!(mask_sensitive(text), text);
    }

    #[test]
    fn a_number_across_a_chunk_overlap_is_masked_before_chunking() {
        let filler = "word ".repeat(398);
        let text = format!("{filler}4111 1111 1111 1111 {filler}");
        let masked = mask_sensitive(&text);
        assert!(!masked.contains("4111"));
        for c in crate::knowledge::chunk::chunk_text(&masked) {
            assert!(!c.text.contains("4111") && !c.text.contains("1111 1111"));
        }
    }

    #[test]
    fn overlapping_spans_do_not_duplicate_or_leak() {
        // The label span and the shape span cover the same digits.
        let masked = mask_sensitive("social security: 123-45-6789 end");
        assert_eq!(masked, "social security: [national id] end");
    }
}
