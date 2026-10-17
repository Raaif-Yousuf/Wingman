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
                .is_some_and(|&(_, c)| is_blank(c) || matches!(c, '_' | '-'))
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

/// A line break of any kind: a value never runs past one.
fn is_eol(c: char) -> bool {
    matches!(
        c,
        '\n' | '\r' | '\u{b}' | '\u{c}' | '\u{85}' | '\u{2028}' | '\u{2029}'
    )
}

/// Whitespace that stays on the line (space, tab, no-break space, U+3000).
fn is_blank(c: char) -> bool {
    c.is_whitespace() && !is_eol(c)
}

/// Markdown emphasis around a label or value.
fn is_emphasis(c: char) -> bool {
    matches!(c, '*' | '_' | '~')
}

fn is_quote(c: char) -> bool {
    matches!(c, '"' | '\'' | '`')
}

/// How a label and its value are joined; decides where the value ends.
#[derive(Clone, Copy, PartialEq)]
enum Join {
    /// `:`, `=` (also full width): to `; ` or the end of the line, so a
    /// flattened CSV row ("Name: Jane; Password: a b; Role: x") keeps its
    /// other cells and a multi-word value is masked whole.
    Colon,
    /// A table cell: to the next `|`.
    Pipe,
    /// `password,hunter2`: to the next comma.
    Comma,
    /// `password<TAB>hunter2`: to the next tab.
    Tab,
    /// Only blanks: the next word, and only when it holds a digit.
    Space,
}

/// Short words that may sit between a label and its value: "PIN number:",
/// "PIN code 1234", "password for wifi:", "Passwort lautet:". Returns the
/// index after the filler, or `j` when there is none. The caller still needs
/// a `:`/`=` or a digit after it.
fn skip_filler(chars: &[(usize, char)], j: usize) -> usize {
    let mut k = j;
    while chars.get(k).is_some_and(|&(_, c)| is_blank(c)) {
        k += 1;
    }
    if k == j {
        return j;
    }
    let word_end = |from: usize| {
        let mut e = from;
        while chars.get(e).is_some_and(|&(_, c)| c.is_alphanumeric()) {
            e += 1;
        }
        e
    };
    for word in ["number", "no.", "code", "lautet", "est", "es"] {
        if let Some(e) = match_label(chars, k, word) {
            if word.ends_with('.') || !chars.get(e).is_some_and(|&(_, c)| c.is_alphanumeric()) {
                return e;
            }
        }
    }
    if let Some(e) = match_label(chars, k, "for") {
        let mut w = e;
        while chars.get(w).is_some_and(|&(_, c)| is_blank(c)) {
            w += 1;
        }
        if w > e && word_end(w) > w {
            return word_end(w);
        }
    }
    j
}

/// Byte ranges of values that follow a secret label: `password: hunter2`,
/// `pwd=abc`, `"pin": "1234"`, `**Password:** x`, `| password | x |`,
/// `password,x`, `PIN 1234`. A quoted value is masked to its closing quote (or
/// the end of the line); see [`Join`] for the rest. A label alone does not
/// count ("the password policy"), nor does a value of only punctuation. Spans
/// are on character boundaries, and the scan is linear: every attempt that
/// reads ahead either becomes a span or stops at the first letter or digit.
fn secret_spans(text: &str) -> Vec<Range<usize>> {
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let at = |k: usize| chars.get(k).map(|&(_, c)| c);
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
        if at(j).is_some_and(|c| lower(c) == 's') && !at(j + 1).is_some_and(char::is_alphanumeric) {
            j += 1;
        }
        if at(j).is_some_and(char::is_alphanumeric) {
            i += 1;
            continue;
        }
        j = skip_filler(&chars, j);
        // Emphasis or quotes closing the label (`**Password**:`, `"pin":`).
        while at(j).is_some_and(|c| is_emphasis(c) || is_quote(c)) {
            j += 1;
        }
        let blanks_start = j;
        let mut has_tab = false;
        while let Some(c) = at(j).filter(|&c| is_blank(c)) {
            has_tab |= c == '\t';
            j += 1;
        }
        let join = match at(j) {
            Some(':' | '=' | '：' | '＝') => {
                j += 1;
                if at(j) == Some('>') {
                    j += 1;
                }
                Join::Colon
            }
            Some('|') => {
                j += 1;
                Join::Pipe
            }
            Some(',') if j == blanks_start && at(j + 1).is_some_and(|c| !c.is_whitespace()) => {
                j += 1;
                Join::Comma
            }
            _ if has_tab => Join::Tab,
            _ if j > blanks_start => Join::Space,
            _ => {
                i += 1;
                continue;
            }
        };
        while at(j).is_some_and(is_blank) {
            j += 1;
        }
        // Emphasis after the separator: `**Password:** x` closes before the
        // value, `**x**` is part of it.
        let mut value_begin = j;
        while at(value_begin).is_some_and(is_emphasis) {
            value_begin += 1;
        }
        let mut span_begin = j;
        if value_begin > j && at(value_begin).is_some_and(is_blank) {
            while at(value_begin).is_some_and(is_blank) {
                value_begin += 1;
            }
            span_begin = value_begin;
        }
        let (stop, content) = match at(value_begin).filter(|&c| is_quote(c)) {
            Some(q) => {
                let mut e = value_begin + 1;
                while at(e).is_some_and(|c| c != q && !is_eol(c)) {
                    e += 1;
                }
                let stop = if at(e) == Some(q) { e + 1 } else { e };
                (stop, value_begin + 1..e)
            }
            None => {
                let mut e = value_begin;
                while let Some(c) = at(e) {
                    let ends = is_eol(c)
                        || match join {
                            Join::Colon => {
                                c == ';' && !at(e + 1).is_some_and(|n| !n.is_whitespace())
                            }
                            Join::Pipe => c == '|',
                            Join::Comma => c == ',',
                            Join::Tab => c == '\t',
                            Join::Space => c.is_whitespace(),
                        };
                    if ends {
                        break;
                    }
                    e += 1;
                }
                while e > value_begin && at(e - 1).is_some_and(is_blank) {
                    e -= 1;
                }
                (e, value_begin..e)
            }
        };
        let value = &chars[content];
        let holds_value = value.iter().any(|&(_, c)| c.is_alphanumeric())
            && (join != Join::Space || value.iter().any(|&(_, c)| c.is_ascii_digit()));
        if !holds_value || span_begin >= stop {
            i += 1;
            continue;
        }
        let from = chars[span_begin].0;
        let to = chars.get(stop).map_or(text.len(), |&(b, _)| b);
        spans.push(from..to);
        i = stop;
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
