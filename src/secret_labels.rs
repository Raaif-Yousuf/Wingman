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
pub fn secret_spans(text: &str) -> Vec<Range<usize>> {
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
