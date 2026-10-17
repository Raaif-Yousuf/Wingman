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
use crate::secret_labels::secret_spans;
use std::ops::Range;

/// Marker for a value that followed a secret label.
const SECRET_MARKER: &str = "[secret]";

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
            ("Password = hunter2 and more", "Password = [secret]"),
            ("pwd=abc123;port=1", "pwd=[secret]"),
            ("PIN 1234 for the door", "PIN [secret] for the door"),
            ("Secret: s3cr3t.", "Secret: [secret]"),
            ("db_password=xyz", "db_password=[secret]"),
            ("API key: sk_live_abc", "API key: [secret]"),
            ("api-key=abc", "api-key=[secret]"),
            ("client_secret: abc", "client_secret: [secret]"),
            (
                "{\"pin\": \"1234\", \"x\": 1}",
                "{\"pin\": [secret], \"x\": 1}",
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
    fn quoted_values_are_masked_whole_without_the_comma() {
        for (text, want) in [
            ("password: 'a b c'", "password: [secret]"),
            (
                "password: \"a b c\" and more",
                "password: [secret] and more",
            ),
            (
                "{\"password\": \"correct horse battery\"}",
                "{\"password\": [secret]}",
            ),
            (
                "{\"pin\": \"x y\", \"user\": \"bob\"}",
                "{\"pin\": [secret], \"user\": \"bob\"}",
            ),
            ("pwd = `a b`", "pwd = [secret]"),
            ("password: \"never closed words", "password: [secret]"),
            (
                "password: \"closed on line one\nnext: line",
                "password: [secret]\nnext: line",
            ),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
    }

    #[test]
    fn markdown_emphasis_around_label_and_value_is_skipped() {
        for (text, want) in [
            ("**Password:** hunter2", "**Password:** [secret]"),
            ("**Password**: hunter2", "**Password**: [secret]"),
            ("__pin__: 1234", "__pin__: [secret]"),
            ("*secret*: abc", "*secret*: [secret]"),
            ("**pin** 1234", "**pin** [secret]"),
            ("password: **hunter2**", "password: [secret]"),
            ("password: _abc", "password: [secret]"),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
        // Nothing but punctuation after the separator is no value.
        for text in ["password: ***", "password: ---", "pin: ** \nx"] {
            assert_eq!(mask_sensitive(text), text, "input: {text}");
        }
    }

    #[test]
    fn odd_blanks_and_full_width_separators_count() {
        for (text, want) in [
            ("password:\u{a0}hunter2", "password:\u{a0}[secret]"),
            (
                "password\u{3000}:\u{3000}hunter2",
                "password\u{3000}:\u{3000}[secret]",
            ),
            ("password：hunter2", "password：[secret]"),
            ("pin＝1234", "pin＝[secret]"),
            ("PIN\u{a0}1234", "PIN\u{a0}[secret]"),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
        // A line separator still ends the value.
        assert_eq!(
            mask_sensitive("password:\u{2028}hunter2"),
            "password:\u{2028}hunter2"
        );
    }

    #[test]
    fn table_cells_and_flattened_csv_rows_are_masked() {
        for (text, want) in [
            ("| password | hunter2 |", "| password | [secret] |"),
            ("|**PIN**|1234|", "|**PIN**|[secret]|"),
            ("password,hunter2,bob", "password,[secret],bob"),
            ("password\thunter2\tbob", "password\t[secret]\tbob"),
            // The CSV reader writes "header: value; header: value".
            (
                "Name: Jane; Password: correct horse; Role: Engineer",
                "Name: Jane; Password: [secret]; Role: Engineer",
            ),
            // The JSON reader writes "path: value", one per line.
            (
                "login.password: two words\nuser: bob",
                "login.password: [secret]\nuser: bob",
            ),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
        assert_eq!(
            mask_sensitive("password, then click"),
            "password, then click"
        );
    }

    #[test]
    fn a_short_filler_between_label_and_value_is_allowed() {
        for (text, want) in [
            ("PIN number: 1234", "PIN number: [secret]"),
            ("PIN no. 1234", "PIN no. [secret]"),
            ("PIN code 1234", "PIN code [secret]"),
            ("pin code: 1234", "pin code: [secret]"),
            ("password for wifi: x", "password for wifi: [secret]"),
            ("Passwort lautet: x", "Passwort lautet: [secret]"),
            ("mot de passe est : x", "mot de passe est : [secret]"),
            ("contraseña es: x", "contraseña es: [secret]"),
            ("password for wifi hunter2", "password for wifi [secret]"),
        ] {
            assert_eq!(mask_sensitive(text), want, "input: {text}");
        }
    }

    #[test]
    fn a_filler_without_a_separator_or_digit_is_prose() {
        for text in [
            "the PIN number policy",
            "the PIN code is printed on the card",
            "password for the router",
            "Passwort lautet geheim",
        ] {
            assert_eq!(mask_sensitive(text), text, "input: {text}");
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
            &"password: ** ".repeat(20_000),
            &"password: \"".repeat(20_000),
            &"| pin |".repeat(20_000),
            &"password,".repeat(20_000),
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
