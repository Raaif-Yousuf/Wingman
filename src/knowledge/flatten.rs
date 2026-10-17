//! CSV and JSON to searchable text (K1). Hand-written and pure: no crate, no
//! file or network access, linear time, and every length is checked, so no
//! file content can make it panic (release builds abort on a panic).
//!
//! CSV becomes the column names on one line, then one line per row of
//! `name: value` pairs. JSON becomes one `path: value` line per value. Both
//! stop once the text reaches the caller's cap, and say so.
//!
//! A line that starts with `#` gets a backslash first: the chunker reads a
//! `#` line as a heading, and a cell or a key must not make one.

use std::iter::Peekable;
use std::str::Chars;

use serde_json::Value;

/// Longest column name or key kept, in characters.
const MAX_NAME_CHARS: usize = 80;
/// Largest JSON text that is parsed. Past it the file is read as plain text,
/// which bounds the memory the parsed tree can take.
const MAX_JSON_BYTES: usize = 1024 * 1024;
/// Most cells kept per CSV row. The rest of a wider row is ignored.
const MAX_CELLS: usize = 256;

/// Lines gathered up to a byte cap.
struct Lines {
    out: String,
    cap: usize,
    full: bool,
}

impl Lines {
    fn new(cap: usize) -> Self {
        Self {
            out: String::new(),
            cap,
            full: false,
        }
    }

    /// Adds one line unless it would pass the cap (then nothing more is added).
    fn push(&mut self, line: &str) {
        if self.full {
            return;
        }
        let escape = line.starts_with('#');
        let extra = usize::from(escape) + usize::from(!self.out.is_empty());
        if self.out.len() + line.len() + extra > self.cap {
            self.full = true;
            return;
        }
        if !self.out.is_empty() {
            self.out.push('\n');
        }
        if escape {
            self.out.push('\\');
        }
        self.out.push_str(line);
    }

    fn finish(self) -> (String, bool) {
        (self.out, self.full)
    }
}

/// White space runs as one space, control characters as spaces, ends trimmed.
fn tidy(s: &str) -> String {
    let spaced: String = s
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    spaced.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn short(s: &str) -> String {
    tidy(s).chars().take(MAX_NAME_CHARS).collect()
}

/// The text with a backslash before every line that starts with `#`, so the
/// chunker reads no line of it as a heading.
pub fn escape_headings(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.split('\n').enumerate() {
        if i > 0 {
            out.push('\n');
        }
        if line.starts_with('#') {
            out.push('\\');
        }
        out.push_str(line);
    }
    out
}

// -- CSV --------------------------------------------------------------------

/// The separator the first line uses: comma, tab or semicolon, whichever
/// appears most outside quotes (comma when none does).
fn separator(text: &str) -> char {
    let (mut commas, mut tabs, mut semis) = (0usize, 0usize, 0usize);
    let mut quoted = false;
    for c in text.chars() {
        match c {
            '"' => quoted = !quoted,
            '\n' if !quoted => {
                if commas + tabs + semis > 0 {
                    break;
                }
            }
            ',' if !quoted => commas += 1,
            '\t' if !quoted => tabs += 1,
            ';' if !quoted => semis += 1,
            _ => {}
        }
    }
    if tabs > commas && tabs >= semis {
        '\t'
    } else if semis > commas {
        ';'
    } else {
        ','
    }
}

/// The next row, or `None` at the end. Quoted cells may hold the separator,
/// line breaks and doubled quotes. Only the first [`MAX_CELLS`] cells are kept.
fn next_row(chars: &mut Peekable<Chars<'_>>, sep: char) -> Option<Vec<String>> {
    chars.peek()?;
    let mut cells: Vec<String> = Vec::new();
    let mut cell = String::new();
    let mut quoted = false;
    let end_cell = |cell: &mut String, cells: &mut Vec<String>| {
        let done = std::mem::take(cell);
        if cells.len() < MAX_CELLS {
            cells.push(done);
        }
    };
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted => {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cell.push('"');
                } else {
                    quoted = false;
                }
            }
            '"' if cell.is_empty() => quoted = true,
            '\n' if !quoted => {
                end_cell(&mut cell, &mut cells);
                return Some(cells);
            }
            c if c == sep && !quoted => end_cell(&mut cell, &mut cells),
            c => cell.push(c),
        }
    }
    end_cell(&mut cell, &mut cells);
    Some(cells)
}

/// The text of a CSV file (line ends already `\n`) and whether it was cut at
/// `cap` bytes.
pub fn csv_to_text(text: &str, cap: usize) -> (String, bool) {
    let sep = separator(text);
    let mut chars = text.chars().peekable();
    let mut lines = Lines::new(cap);
    let mut names: Option<Vec<String>> = None;
    while let Some(row) = next_row(&mut chars, sep) {
        if lines.full {
            break;
        }
        if row.iter().all(|c| tidy(c).is_empty()) {
            continue;
        }
        let Some(names) = names.as_ref() else {
            let header: Vec<String> = row
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let name = short(c);
                    if name.is_empty() {
                        format!("column {}", i + 1)
                    } else {
                        name
                    }
                })
                .collect();
            lines.push(&header.join(" | "));
            names = Some(header);
            continue;
        };
        let pairs: Vec<String> = row
            .iter()
            .enumerate()
            .filter_map(|(i, cell)| {
                let value = tidy(cell);
                if value.is_empty() {
                    return None;
                }
                let name = names
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| format!("column {}", i + 1));
                Some(format!("{name}: {value}"))
            })
            .collect();
        lines.push(&pairs.join("; "));
    }
    // More input left when the cap was hit: `full` is set by the line that
    // did not fit, or here when the loop stopped on it.
    lines.finish()
}

// -- JSON -------------------------------------------------------------------

/// The text of a JSON document, or `None` when it is not valid JSON (the
/// caller then reads the file as plain text). Nesting past the parser's own
/// depth limit is an error, not a stack overflow.
pub fn json_to_text(text: &str, cap: usize) -> Option<(String, bool)> {
    // The parsed tree is several times the size of the text, so a bigger file
    // is read as plain text instead.
    if text.len() > MAX_JSON_BYTES {
        return None;
    }
    let value: Value = serde_json::from_str(text).ok()?;
    let mut lines = Lines::new(cap);
    let mut path = String::new();
    walk(&value, &mut path, &mut lines);
    Some(lines.finish())
}

fn walk(value: &Value, path: &mut String, lines: &mut Lines) {
    if lines.full {
        return;
    }
    let leaf = |text: String, path: &str, lines: &mut Lines| {
        if text.is_empty() {
            return;
        }
        if path.is_empty() {
            lines.push(&text);
        } else {
            lines.push(&format!("{path}: {text}"));
        }
    };
    match value {
        Value::Null => {}
        Value::Bool(b) => leaf(b.to_string(), path, lines),
        Value::Number(n) => leaf(n.to_string(), path, lines),
        Value::String(s) => leaf(tidy(s), path, lines),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if lines.full {
                    return;
                }
                let before = path.len();
                path.push_str(&format!("[{i}]"));
                walk(item, path, lines);
                path.truncate(before);
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                if lines.full {
                    return;
                }
                let before = path.len();
                let key = short(key);
                if !path.is_empty() {
                    path.push('.');
                }
                path.push_str(if key.is_empty() { "(no key)" } else { &key });
                walk(item, path, lines);
                path.truncate(before);
            }
        }
    }
}
