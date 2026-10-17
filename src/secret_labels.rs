//! Finds the value written after a label that names a secret (`password: x`,
//! `PIN 1234`, `| token | x |`), for both text redaction (`redact.rs`, numbered
//! markers) and the knowledge store (`knowledge/mask.rs`, one fixed marker).
//!
//! Fail closed and panic-free on untrusted text: it works on characters, never
//! slices a string by guessed byte offsets, and reads ahead linearly.

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
    "secret key",
    "otp",
    "cvv",
    "cvc",
    "security code",
    "verification code",
    "recovery code",
    "one time code",
    "passkey",
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
    /// "password is hunter2": the next word, and only when it holds a digit or
    /// a symbol ("the password is incorrect" is prose).
    Is,
}

/// How far a value on the line below a bare `label:` may run, in characters.
const MAX_NEXT_LINE: usize = 120;

/// True when a `;` at `from - 1` is followed by another `Label:` cell of a
/// flattened row (or by nothing), so the value ends there. A `; ` inside a
/// value ("ab; cd") has no label after it and stays part of the value.
fn next_cell_is_label(chars: &[(usize, char)], from: usize) -> bool {
    let mut k = from;
    while chars.get(k).is_some_and(|&(_, c)| is_blank(c)) {
        k += 1;
    }
    let mut seen_text = false;
    for step in 0..40 {
        match chars.get(k + step).map(|&(_, c)| c) {
            None => return step == 0,
            Some(c) if is_eol(c) => return step == 0,
            Some(';') => return false,
            Some(':' | '=' | '：' | '＝') => return seen_text,
            Some(c) => seen_text |= c.is_alphanumeric(),
        }
    }
    false
}

/// Short words that may sit between a label and its value: "PIN number:",
/// "PIN code 1234", "password for wifi:", "Passwort lautet:". Returns the
/// index after the filler, or `j` when there is none. The caller still needs
/// a `:`/`=` or a digit after it.
/// The flag is true when the filler is "is" or "are" ([`Join::Is`]).
fn skip_filler(chars: &[(usize, char)], j: usize) -> (usize, bool) {
    let mut k = j;
    while chars.get(k).is_some_and(|&(_, c)| is_blank(c)) {
        k += 1;
    }
    if k == j {
        return (j, false);
    }
    let word_end = |from: usize| {
        let mut e = from;
        while chars.get(e).is_some_and(|&(_, c)| c.is_alphanumeric()) {
            e += 1;
        }
        e
    };
    for word in ["number", "no.", "code", "lautet", "est", "es", "is", "are"] {
        if let Some(e) = match_label(chars, k, word) {
            if word.ends_with('.') || !chars.get(e).is_some_and(|&(_, c)| c.is_alphanumeric()) {
                return (e, matches!(word, "is" | "are"));
            }
        }
    }
    if let Some(e) = match_label(chars, k, "for") {
        let mut w = e;
        while chars.get(w).is_some_and(|&(_, c)| is_blank(c)) {
            w += 1;
        }
        if w > e && word_end(w) > w {
            return (word_end(w), false);
        }
    }
    (j, false)
}

/// True when a value word after "is" reads as a secret: it holds a digit or a
/// symbol once trailing sentence punctuation is dropped.
fn looks_like_secret_word(word: &[(usize, char)]) -> bool {
    let end = word
        .iter()
        .rposition(|&(_, c)| !matches!(c, '.' | ',' | ';' | ':' | '!' | '?'))
        .map_or(0, |p| p + 1);
    word[..end]
        .iter()
        .any(|&(_, c)| c.is_ascii_digit() || !c.is_alphanumeric())
}

/// True when the whole line is one secret label, with optional emphasis,
/// quotes and a colon (`Password`, `**Password**`, `Password：`): a login
/// form's label above its field. A bare `pin` needs its colon, since a lone
/// "pin" is as likely a circuit label.
fn is_label_alone(text: &str) -> bool {
    let junk = |c: char| c.is_whitespace() || is_emphasis(c) || is_quote(c);
    let t = text.trim_matches(junk);
    let has_colon = t.ends_with([':', '：']);
    let t = t.trim_end_matches([':', '：']).trim_matches(junk);
    let chars: Vec<(usize, char)> = t.char_indices().collect();
    if chars.is_empty() {
        return false;
    }
    let whole = |label: &str| {
        let Some(e) = match_label(&chars, 0, label) else {
            return false;
        };
        e == chars.len() || (e + 1 == chars.len() && lower(chars[e].1) == 's')
    };
    SECRET_LABELS
        .iter()
        .any(|l| whole(l) && (has_colon || *l != "pin"))
}

/// True when `text` ends in a secret label that is waiting for its value on
/// the next line (`Password:`, `PIN =`, `**Password:**`), so a caller that
/// holds one line at a time (OCR) knows to hide the line that follows.
pub fn ends_with_open_label(text: &str) -> bool {
    if is_label_alone(text) {
        return true;
    }
    let trimmed = text.trim_end();
    if !trimmed.ends_with([':', '=', '：', '＝', '*']) {
        return false;
    }
    let probe = format!("{text}\nx");
    secret_spans(&probe, false)
        .iter()
        .any(|s| s.start > text.len())
}

/// Byte ranges of values that follow a secret label: `password: hunter2`,
/// `pwd=abc`, `"pin": "1234"`, `**Password:** x`, `| password | x |`,
/// `password,x`, `PIN 1234`. A quoted value is masked to its closing quote (or
/// the end of the line); see [`Join`] for the rest. A label alone does not
/// count ("the password policy"), nor does a value of only punctuation. Spans
/// are on character boundaries, and the scan is linear: every attempt that
/// reads ahead either becomes a span or stops at the first letter or digit.
///
/// `space_join` allows the `PIN 1234` form (a label, a blank and a word with a
/// digit). Documents turn it on; screen text leaves it off, since "pin 3
/// connects to pin 4" in a circuit problem is not a secret.
pub fn secret_spans(text: &str, space_join: bool) -> Vec<Range<usize>> {
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
        let label_end = j;
        // A plural label ("passwords:").
        if at(j).is_some_and(|c| lower(c) == 's') && !at(j + 1).is_some_and(char::is_alphanumeric) {
            j += 1;
        }
        if at(j).is_some_and(char::is_alphanumeric) {
            i += 1;
            continue;
        }
        let is_pin = match_label(&chars, i, "pin") == Some(label_end);
        let (after_filler, is_filler) = skip_filler(&chars, j);
        j = after_filler;
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
            _ if is_filler && !is_pin && j > blanks_start => Join::Is,
            _ if space_join && j > blanks_start => Join::Space,
            _ => {
                i += 1;
                continue;
            }
        };
        let after_sep = j;
        while at(j).is_some_and(is_blank) {
            j += 1;
        }
        // A bare `label:` (emphasis may hug the colon) with a short value on
        // the line below: `\n` or `\r\n` only, never a blank line.
        if join == Join::Colon {
            let mut k = j;
            if j == after_sep {
                while at(k).is_some_and(is_emphasis) {
                    k += 1;
                }
                while at(k).is_some_and(is_blank) {
                    k += 1;
                }
            }
            let line_start = match (at(k), at(k + 1)) {
                (Some('\n'), _) => Some(k + 1),
                (Some('\r'), Some('\n')) => Some(k + 2),
                _ => None,
            };
            if let Some(mut m) = line_start {
                while at(m).is_some_and(is_blank) {
                    m += 1;
                }
                let mut e = m;
                while e <= m + MAX_NEXT_LINE && at(e).is_some_and(|c| !is_eol(c)) {
                    e += 1;
                }
                let short = e <= m + MAX_NEXT_LINE;
                let mut last = e;
                while last > m && at(last - 1).is_some_and(is_blank) {
                    last -= 1;
                }
                if short && last > m && !at(last - 1).is_some_and(|c| matches!(c, ':' | '：')) {
                    j = m;
                }
            }
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
                                c == ';'
                                    && !at(e + 1).is_some_and(|n| !n.is_whitespace())
                                    && next_cell_is_label(&chars, e + 1)
                            }
                            Join::Pipe => c == '|',
                            Join::Comma => c == ',',
                            Join::Tab => c == '\t',
                            Join::Space | Join::Is => c.is_whitespace(),
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
            && match join {
                Join::Space => value.iter().any(|&(_, c)| c.is_ascii_digit()),
                Join::Is => at(value_begin).is_some_and(is_quote) || looks_like_secret_word(value),
                _ => true,
            };
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `text` with every span replaced by `#`.
    fn mask(text: &str, space_join: bool) -> String {
        let mut out = String::new();
        let mut at = 0;
        for s in secret_spans(text, space_join) {
            out.push_str(&text[at..s.start]);
            out.push('#');
            at = s.end;
        }
        out.push_str(&text[at..]);
        out
    }

    #[test]
    fn is_between_label_and_value_counts() {
        for (text, want) in [
            ("password is hunter2", "password is #"),
            (
                "Your temporary password is: Xk3!pQ",
                "Your temporary password is: #",
            ),
            ("the password is Xk3!pQ.", "the password is #"),
            ("PASSWORD IS hunter2", "PASSWORD IS #"),
            ("passwords are: abc", "passwords are: #"),
        ] {
            assert_eq!(mask(text, false), want, "input: {text}");
            assert_eq!(mask(text, true), want, "input: {text}");
        }
    }

    #[test]
    fn is_before_plain_prose_or_a_pin_stays_readable() {
        for text in [
            "the password is incorrect",
            "password is required.",
            "pin is 3",
            "pin 3 connects to pin 4",
            "the token is valid",
        ] {
            assert_eq!(mask(text, false), text, "input: {text}");
        }
    }

    #[test]
    fn a_value_on_the_next_line_is_masked_after_a_bare_colon() {
        for (text, want) in [
            ("Password:\nhunter2", "Password:\n#"),
            (
                "Password:  \r\n  hunter2\r\nUser: bob",
                "Password:  \r\n  #\r\nUser: bob",
            ),
            ("PIN =\n1234", "PIN =\n#"),
            ("Password:\n\"two words\"", "Password:\n#"),
        ] {
            assert_eq!(mask(text, false), want, "input: {text}");
        }
    }

    #[test]
    fn the_next_line_rule_is_bounded() {
        for text in [
            // Not a secret label.
            "Answer:\n42 m/s",
            "Problem 3:\nA 2 kg block slides down a slope",
            // A label followed by another label, not a value.
            "Password:\nUsername:",
            // A blank line, a line break that is not \n, or no line at all.
            "Password:\n\nhunter2",
            "password:\u{2028}hunter2",
            "password:",
            "password:\n",
            "password:\n   ",
            // A paragraph, not a value.
            &format!("password:\n{}", "word ".repeat(60)),
            // Punctuation only.
            "password:\n***",
        ] {
            assert_eq!(mask(text, false), text, "input: {text}");
        }
    }

    #[test]
    fn open_label_lines_are_recognised() {
        assert!(ends_with_open_label("Password:"));
        assert!(ends_with_open_label("  PIN = "));
        assert!(ends_with_open_label("**Password:**"));
        assert!(!ends_with_open_label("Answer:"));
        assert!(!ends_with_open_label("password: hunter2"));
        assert!(!ends_with_open_label("the password"));
        assert!(!ends_with_open_label(""));
    }

    #[test]
    fn a_label_alone_on_a_line_opens_the_next_line() {
        for yes in [
            "Password",
            "  PASSWORD  ",
            "Password：",
            "**Password**",
            "Passwords:",
            "Security code",
            "pin:",
        ] {
            assert!(ends_with_open_label(yes), "{yes}");
        }
        for no in [
            "Answer",
            "Problem 3:",
            "pin 3",
            "pin",
            "Pin",
            "Enter your password",
            "Password reset",
            "password policy:",
        ] {
            assert!(!ends_with_open_label(no), "{no}");
        }
        assert_eq!(mask("Password：\nhunter2", false), "Password：\n#");
        assert_eq!(
            mask("Password:\nUsername：", false),
            "Password:\nUsername："
        );
    }

    #[test]
    fn a_quoted_value_after_is_counts() {
        for (text, want) in [
            ("password is \"hunter\"", "password is #"),
            ("password is 'sunshine'", "password is #"),
            ("my password is `abc`", "my password is #"),
        ] {
            assert_eq!(mask(text, false), want, "input: {text}");
        }
        // A plain word after "is" is still prose.
        assert_eq!(
            mask("my password is sunshine", false),
            "my password is sunshine"
        );
        assert_eq!(mask("pin is \"3\"", false), "pin is \"3\"");
    }

    #[test]
    fn more_secret_labels_are_known() {
        for (text, want) in [
            ("OTP: 123456", "OTP: #"),
            ("cvv: 123", "cvv: #"),
            ("CVC=123", "CVC=#"),
            ("Security code: 4821", "Security code: #"),
            ("verification code: 551234", "verification code: #"),
            ("Recovery code: abcd-efgh", "Recovery code: #"),
            ("one-time code: 998877", "one-time code: #"),
            ("passkey: x1", "passkey: #"),
            ("SECRET_KEY=abc123", "SECRET_KEY=#"),
            ("secret key: abc", "secret key: #"),
            ("client_secret: abc", "client_secret: #"),
        ] {
            assert_eq!(mask(text, false), want, "input: {text}");
        }
        for text in ["the otpion is open", "cvvs", "a security code policy"] {
            assert_eq!(mask(text, false), text, "input: {text}");
        }
    }

    #[test]
    fn a_value_with_a_semicolon_is_masked_whole_but_csv_cells_stay() {
        for (text, want) in [
            ("password: ab; cd", "password: #"),
            ("password: ab; cd; ef", "password: #"),
            (
                "Name: Jane; Password: a b; Role: x",
                "Name: Jane; Password: #; Role: x",
            ),
            (
                "Password: abc; Role: Engineer",
                "Password: #; Role: Engineer",
            ),
            ("password: abc;", "password: #;"),
            ("password: abc; ", "password: #; "),
        ] {
            assert_eq!(mask(text, false), want, "input: {text}");
        }
    }

    #[test]
    fn new_rules_never_panic_on_odd_text() {
        for text in [
            "password is",
            "password is ",
            "password is \u{1F600}",
            "password:\n\u{1F600}",
            "password:\r",
            "password:\r\n",
            "password: a;",
            "password: a; \u{1F600}ñ",
            "otp",
            &"password:\n".repeat(20_000),
            &"password: a; ".repeat(20_000),
            &"password is ".repeat(20_000),
        ] {
            let _ = secret_spans(text, false);
            let _ = secret_spans(text, true);
            let _ = ends_with_open_label(text);
        }
    }
}
