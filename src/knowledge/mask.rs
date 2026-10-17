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

/// Words that label a secret, lower case. A space inside a label matches a run
/// of spaces, `_` and `-`, so `api key`, `API_KEY` and `api-key` all match.
const SECRET_LABELS: &[&str] = &[
    "password",
    "passwd",
    "passcode",
    "passphrase",
    "pwd",
    "pin",
    "secret",
    "token",
    "api key",
    "apikey",
    "access key",
    "passwort",
    "kennwort",
    "geheimzahl",
    "mot de passe",
    "mdp",
    "code secret",
    "contraseña",
    "contrasena",
    "clave secreta",
];

/// Marker for a value that followed a secret label.
const SECRET_MARKER: &str = "[secret]";

fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Index after `label` if it matches `chars` at `at`, ignoring case.
fn match_label(chars: &[(usize, char)], at: usize, label: &str) -> Option<usize> {
    let mut i = at;
    for lc in label.chars() {
        if lc == ' ' {
            let start = i;
            while chars
                .get(i)
                .is_some_and(|&(_, c)| matches!(c, ' ' | '_' | '-' | '\t'))
            {
                i += 1;
            }
            if i == start {
                return None;
            }
        } else {
            let &(_, c) = chars.get(i)?;
            if lower(c) != lc {
                return None;
            }
            i += 1;
        }
    }
    Some(i)
}

fn is_blank(c: char) -> bool {
    c == ' ' || c == '\t'
}

/// Byte ranges of values that follow a secret label: `password: hunter2`,
/// `pwd=abc`, `"pin": "1234"`, `PIN 1234`. The value runs to the next
/// whitespace, never past the end of the line. A label alone does not count
/// ("the password policy"): a `:` or `=` must follow it, or, with only a space,
/// the next word must hold a digit. Spans are on character boundaries.
fn secret_spans(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut spans = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let at_word_start = i == 0 || !chars[i - 1].1.is_alphanumeric();
        let first = lower(chars[i].1);
        let end = if at_word_start {
            SECRET_LABELS
                .iter()
                .filter(|l| l.starts_with(first))
                .filter_map(|l| match_label(&chars, i, l))
                .max()
        } else {
            None
        };
        let Some(mut j) = end else {
            i += 1;
            continue;
        };
        // A plural label ("passwords:").
        if chars.get(j).is_some_and(|&(_, c)| lower(c) == 's')
            && !chars.get(j + 1).is_some_and(|&(_, c)| c.is_alphanumeric())
        {
            j += 1;
        }
        if chars.get(j).is_some_and(|&(_, c)| c.is_alphanumeric()) {
            i += 1;
            continue;
        }
        // A closing quote around the label (`"pin":`).
        if chars
            .get(j)
            .is_some_and(|&(_, c)| matches!(c, '"' | '\'' | '`'))
        {
            j += 1;
        }
        let blanks_start = j;
        while chars.get(j).is_some_and(|&(_, c)| is_blank(c)) {
            j += 1;
        }
        let spaced = j > blanks_start;
        let explicit = chars.get(j).is_some_and(|&(_, c)| c == ':' || c == '=');
        if explicit {
            j += 1;
            if chars.get(j).is_some_and(|&(_, c)| c == '>') {
                j += 1;
            }
            while chars.get(j).is_some_and(|&(_, c)| is_blank(c)) {
                j += 1;
            }
        } else if !spaced {
            i += 1;
            continue;
        }
        let value_start = j;
        while chars.get(j).is_some_and(|&(_, c)| !c.is_whitespace()) {
            j += 1;
        }
        let value = &chars[value_start..j];
        if value.is_empty() || (!explicit && !value.iter().any(|&(_, c)| c.is_ascii_digit())) {
            i += 1;
            continue;
        }
        let start = chars[value_start].0;
        let stop = chars.get(j).map_or(text.len(), |&(b, _)| b);
        spans.push(start..stop);
        i = j;
    }
    spans
}

/// `text` with every card number, IBAN, national id and labelled secret
/// replaced by a marker.
pub fn mask_sensitive(text: &str) -> String {
    let mut spans: Vec<(Range<usize>, &'static str)> = Vec::new();
    spans.extend(secret_spans(text).into_iter().map(|r| (r, SECRET_MARKER)));
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
    fn a_labelled_secret_value_is_masked_and_the_label_kept() {
        for (text, want) in [
            ("password: hunter2", "password: [secret]"),
            (
                "Password = hunter2 and more",
                "Password = [secret] and more",
            ),
            ("pwd=abc123;port=1", "pwd=[secret]"),
            ("PIN 1234 for the door", "PIN [secret] for the door"),
            ("Secret: s3cr3t.", "Secret: [secret]"),
            ("db_password=xyz", "db_password=[secret]"),
            ("API key: sk_live_abc", "API key: [secret]"),
            ("api-key=abc", "api-key=[secret]"),
            ("client_secret: abc", "client_secret: [secret]"),
            (
                "{\"pin\": \"1234\", \"x\": 1}",
                "{\"pin\": [secret] \"x\": 1}",
            ),
            ("passwords: a1", "passwords: [secret]"),
            ("Passwort: geheim", "Passwort: [secret]"),
            ("Mot de passe : Zz9", "Mot de passe : [secret]"),
            ("Contraseña: Zz9", "Contraseña: [secret]"),
            ("CONTRASEÑA=Zz9", "CONTRASEÑA=[secret]"),
            ("token:\tabc", "token:\t[secret]"),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
    }

    #[test]
    fn prose_about_passwords_is_left_alone() {
        for text in [
            "the password policy says to change it",
            "Reset your password.",
            "A pin is a small nail. Pinned to the wall.",
            "password hunter",
            "tokenizer: fast",
            "mypassword: x",
            "password:",
            "password:   \nnext line",
            "secretary: Ann",
            "the token",
        ] {
            assert_eq!(mask_sensitive(text), text, "input: {text}");
        }
    }

    #[test]
    fn a_secret_value_stops_at_the_end_of_the_line() {
        assert_eq!(
            mask_sensitive("password: hunter2\nuser: bob"),
            "password: [secret]\nuser: bob"
        );
    }

    #[test]
    fn secret_masking_never_panics_on_odd_text() {
        for text in [
            "",
            "pin",
            "pin ",
            "password:",
            "ñ password: ñ𝄞",
            "pin 𝄞1",
            "password\u{0}=\u{0}",
            "mot de ",
            "passwords",
            &"pin ".repeat(5000),
            &format!("pin{}", " ".repeat(50_000)),
        ] {
            let _ = mask_sensitive(text);
        }
        assert_eq!(mask_sensitive("ñ password: ñ𝄞"), "ñ password: [secret]");
    }

    #[test]
    fn overlapping_spans_do_not_duplicate_or_leak() {
        // The label span and the shape span cover the same digits.
        let masked = mask_sensitive("social security: 123-45-6789 end");
        assert_eq!(masked, "social security: [national id] end");
    }
}
