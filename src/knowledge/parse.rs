//! Plain text, Markdown, HTML, CSV and JSON to text, for the chunker.

use super::flatten;

pub const MAX_FILE_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
pub const PARSER_VERSION: i64 = 1;

#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    TooLarge,
    NotText,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ParseError::TooLarge => write!(f, "the file is too large to index"),
            ParseError::NotText => write!(f, "the file does not look like text"),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Parsed {
    pub title: Option<String>,
    pub text: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Parser {
    Text,
    Markdown,
    /// Visible text only: scripts, styles and comments are dropped.
    Html,
    /// A header line, then one "name: value" line per row.
    Csv,
    /// One "path: value" line per value; invalid JSON is read as plain text.
    Json,
}

impl Parser {
    pub fn name(self) -> &'static str {
        match self {
            Parser::Text => "txt",
            Parser::Markdown => "md",
            Parser::Html => "html",
            Parser::Csv => "csv",
            Parser::Json => "json",
        }
    }
}

/// Which parser reads a file, by its extension. Anything else is not indexed
/// by this slice.
pub fn parser_for(file_name: &str) -> Option<Parser> {
    let (stem, ext) = file_name.rsplit_once('.')?;
    if stem.is_empty() {
        return None;
    }
    match ext.to_ascii_lowercase().as_str() {
        "txt" | "text" => Some(Parser::Text),
        "md" | "markdown" => Some(Parser::Markdown),
        "html" | "htm" => Some(Parser::Html),
        "csv" | "tsv" => Some(Parser::Csv),
        "json" => Some(Parser::Json),
        _ => None,
    }
}

/// Reads a file's bytes as text. Never panics: size caps, binary detection and
/// character-boundary-safe truncation make every input either text or an error.
pub fn parse(parser: Parser, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let (decoded, cut_input) = decode(bytes)?;
    let (normalized, cut_text) = truncate(normalize(&decoded), MAX_TEXT_BYTES);
    let truncated = cut_input || cut_text;
    match parser {
        Parser::Text => Ok(Parsed {
            title: None,
            text: normalized,
            truncated,
        }),
        Parser::Markdown => {
            let (title, text) = markdown_to_text(&normalized);
            let (text, cut) = truncate(text, MAX_TEXT_BYTES);
            Ok(Parsed {
                title,
                text,
                truncated: truncated || cut,
            })
        }
        Parser::Html => {
            // Only the first 2 MB of the file was decoded above, so the markup
            // is bounded before it is stripped.
            let text =
                flatten::escape_headings(&crate::inputs::html_text::html_to_text(&normalized));
            let (text, cut) = truncate(text, MAX_TEXT_BYTES);
            Ok(Parsed {
                title: None,
                text,
                truncated: truncated || cut,
            })
        }
        Parser::Csv => {
            let (text, cut) = flatten::csv_to_text(&normalized, MAX_TEXT_BYTES);
            Ok(Parsed {
                title: None,
                text,
                truncated: truncated || cut,
            })
        }
        Parser::Json => match flatten::json_to_text(&normalized, MAX_TEXT_BYTES) {
            Some((text, cut)) => Ok(Parsed {
                title: None,
                text,
                truncated: truncated || cut,
            }),
            // Not valid JSON (or cut short): the words are still worth finding.
            None => Ok(Parsed {
                title: None,
                text: normalized,
                truncated,
            }),
        },
    }
}

/// UTF-8 (with or without a BOM) and UTF-16 with a BOM. Bytes that are not
/// valid UTF-8 are read in the system ANSI code page. A NUL byte in anything
/// else, or bytes that do not decode to mostly letters, means a binary file.
///
/// Only the first `MAX_TEXT_BYTES` bytes are decoded (the text is cut to that
/// size anyway), so a 25 MB file never costs 25 MB of copies. The flag says the
/// input was cut.
fn decode(bytes: &[u8]) -> Result<(String, bool), ParseError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge);
    }
    // UTF-32 is not supported; its BOMs would otherwise read as UTF-16 (LE)
    // or as NUL-ridden bytes.
    if bytes.starts_with(&[0xFF, 0xFE, 0, 0]) || bytes.starts_with(&[0, 0, 0xFE, 0xFF]) {
        return Err(ParseError::NotText);
    }
    let cut = bytes.len() > MAX_TEXT_BYTES;
    let bytes = bytes.get(..MAX_TEXT_BYTES).unwrap_or(bytes);
    let text = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        decode_utf16(rest, u16::from_le_bytes)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        decode_utf16(rest, u16::from_be_bytes)
    } else {
        let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        if bytes.contains(&0) {
            return Err(ParseError::NotText);
        }
        decode_8bit(bytes, cut, ansi_code_page())?
    };
    // Mostly undecodable means a binary file, in either encoding. A cut that
    // lands inside a character adds one replacement, far under the ratio.
    let bad = text.chars().filter(|&c| c == '\u{FFFD}').count();
    if bad > 0 && bad * 10 > text.chars().count() {
        return Err(ParseError::NotText);
    }
    Ok((text, cut))
}

/// The system ANSI code page (`GetACP`), 1252 on a Western install.
fn ansi_code_page() -> u32 {
    // SAFETY: GetACP takes no arguments and only reads system state.
    unsafe { windows::Win32::Globalization::GetACP() }
}

/// Valid UTF-8 as is. Anything else (no NULs, checked by the caller) is taken
/// to be saved in `code_page`, as old notes and exports often are, unless the
/// result does not look like text, which means a binary file.
///
/// `cut` says the input was cut at the size cap, so a character split at the
/// very end is a cut, not a sign of another encoding.
fn decode_8bit(bytes: &[u8], cut: bool, code_page: u32) -> Result<String, ParseError> {
    match std::str::from_utf8(bytes) {
        Ok(s) => Ok(s.to_string()),
        Err(e) if cut && e.error_len().is_none() => Ok(String::from_utf8_lossy(bytes).into_owned()),
        Err(_) if code_page == CP_UTF8 || is_mostly_utf8(bytes) => {
            // Already UTF-8 as far as it goes (the ANSI page is UTF-8, or a
            // few stray bytes sit in a UTF-8 file): keep it, lossy.
            let text = String::from_utf8_lossy(bytes).into_owned();
            // The replacements are expected here; `decode` bounds their share.
            if looks_like_text(bytes, &text.replace('\u{FFFD}', "")) {
                Ok(text)
            } else {
                Err(ParseError::NotText)
            }
        }
        Err(_) => {
            let text = crate::inputs::web_page::decode_code_page(bytes, code_page)
                .ok_or(ParseError::NotText)?;
            if looks_like_text(bytes, &text) {
                Ok(text)
            } else {
                Err(ParseError::NotText)
            }
        }
    }
}

/// The UTF-8 code page id.
const CP_UTF8: u32 = 65001;

/// True when the bytes hold valid multibyte UTF-8 and no more invalid bytes
/// than valid multibyte characters: a UTF-8 file with a stray byte, not a
/// single-byte file whose accents happen to look like a lead byte.
fn is_mostly_utf8(bytes: &[u8]) -> bool {
    let (mut multibyte, mut invalid) = (0usize, 0usize);
    for chunk in bytes.utf8_chunks() {
        multibyte += chunk.valid().chars().filter(|c| !c.is_ascii()).count();
        invalid += chunk.invalid().len();
    }
    multibyte > 0 && invalid <= multibyte
}

/// A code-page decode never fails, so binary data must be told apart by what
/// came out: real text is mostly letters, and has few control bytes.
fn looks_like_text(bytes: &[u8], decoded: &str) -> bool {
    let controls = bytes
        .iter()
        .filter(|&&b| (b < 0x20 && !matches!(b, b'\t' | b'\n' | b'\r' | 0x0c)) || b == 0x7f)
        .count();
    if controls * 50 > bytes.len() {
        return false;
    }
    let (mut letters, mut others) = (0usize, 0usize);
    for c in decoded.chars().filter(|c| !c.is_whitespace()) {
        if c.is_alphanumeric() {
            letters += 1;
        } else {
            others += 1;
        }
    }
    // Digits and punctuation are fine in a log or a table, so the bar is low;
    // replacement characters and symbol runs are what binary data produces.
    letters * 2 >= letters + others && !decoded.contains('\u{FFFD}')
}

fn decode_utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> String {
    let units = bytes.chunks_exact(2).map(|p| unit([p[0], p[1]]));
    char::decode_utf16(units)
        .map(|r| r.unwrap_or('\u{FFFD}'))
        .collect()
}

/// Newlines to `\n`; control characters to spaces, except tab, newline and
/// form feed (the page break the chunker reads); stray BOMs removed.
fn normalize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() != Some(&'\n') {
                    out.push('\n');
                }
            }
            '\n' | '\t' | '\u{c}' => out.push(c),
            '\u{feff}' => {}
            c if c.is_control() => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// Cuts to at most `max` bytes on a character boundary.
fn truncate(mut text: String, max: usize) -> (String, bool) {
    if text.len() <= max {
        return (text, false);
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    (text, true)
}

const MAX_TITLE_CHARS: usize = 200;
/// Longest line whose links are rewritten. Past this the line is kept as is.
/// Within it, each link scan stops at the next `[` or `(` (see `link_at`), so
/// the work per line is bounded by a small multiple of its length.
const MAX_LINK_LINE_CHARS: usize = 5_000;
const MAX_LABEL_CHARS: usize = 500;
const MAX_URL_CHARS: usize = 2_000;
const FRONT_MATTER_MAX_LINES: usize = 200;

/// Markdown to searchable text. Headings stay as `#` lines (the chunker reads
/// them), links and images keep their words, front matter, comments and code
/// fence markers go. Line by line with no recursion, so nesting depth costs
/// nothing.
fn markdown_to_text(text: &str) -> (Option<String>, String) {
    let mut lines: Vec<&str> = text.split('\n').collect();
    let mut title = None;
    if let Some((fm_title, skip)) = front_matter(&lines) {
        title = fm_title;
        lines.drain(..skip);
    }
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut in_fence = false;
    let mut in_comment = false;
    let mut first_heading: Option<String> = None;
    for line in lines {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            // Code stays literal, but a `#` comment line must not read as a
            // heading to the chunker, so it gets a backslash (searchable
            // words are unchanged).
            if trimmed.starts_with('#') {
                out.push(format!("\\{trimmed}"));
            } else {
                out.push(line.to_string());
            }
            continue;
        }
        let Some(line) = strip_comments(line, &mut in_comment) else {
            continue;
        };
        if let Some((level, heading)) = atx_heading(&line) {
            if first_heading.is_none() && !heading.is_empty() {
                first_heading = Some(heading.chars().take(MAX_TITLE_CHARS).collect());
            }
            out.push(format!("{} {}", "#".repeat(level), heading));
            continue;
        }
        out.push(strip_links(&line));
    }
    (title.or(first_heading), out.join("\n"))
}

/// `Some((title, lines to skip))` when the file opens with a closed `---`
/// block.
fn front_matter(lines: &[&str]) -> Option<(Option<String>, usize)> {
    if lines.first().map(|l| l.trim_end()) != Some("---") {
        return None;
    }
    let close = lines
        .iter()
        .enumerate()
        .skip(1)
        .take(FRONT_MATTER_MAX_LINES)
        .find(|(_, l)| matches!(l.trim_end(), "---" | "..."))?
        .0;
    let title = lines.get(1..close)?.iter().find_map(|l| {
        let v = l.strip_prefix("title:")?.trim();
        let v = v.trim_matches(|c| c == '"' || c == '\'').trim();
        (!v.is_empty()).then(|| v.chars().take(MAX_TITLE_CHARS).collect())
    });
    Some((title, close + 1))
}

/// `Some(line)` with HTML comments removed; `None` when the whole line is
/// inside a comment that started on an earlier line.
fn strip_comments(line: &str, in_comment: &mut bool) -> Option<String> {
    if !*in_comment && !line.contains("<!--") {
        return Some(line.to_string());
    }
    let started_inside = *in_comment;
    let mut out = String::new();
    let mut rest = line;
    let mut closed_here = false;
    loop {
        if *in_comment {
            let Some(i) = rest.find("-->") else {
                return (!started_inside || closed_here).then_some(out);
            };
            rest = rest.get(i + 3..).unwrap_or("");
            *in_comment = false;
            closed_here = true;
        } else {
            let Some(i) = rest.find("<!--") else {
                out.push_str(rest);
                return Some(out);
            };
            out.push_str(rest.get(..i).unwrap_or(""));
            rest = rest.get(i + 4..).unwrap_or("");
            *in_comment = true;
        }
    }
}

/// `(level, text)` for `## Title ##` style headings (up to three spaces of
/// indent, one to six hashes, then a space or the end of the line).
fn atx_heading(line: &str) -> Option<(usize, String)> {
    let body = line.trim_start_matches(' ');
    if line.len() - body.len() > 3 {
        return None;
    }
    let level = body.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let after = body.get(level..)?;
    if !(after.is_empty() || after.starts_with(' ') || after.starts_with('\t')) {
        return None;
    }
    let rest = after.trim();
    // A closing run of hashes only counts when a space separates it from the
    // text ("C#" keeps its hash).
    let stripped = rest.trim_end_matches('#');
    let text = if stripped.is_empty() || stripped.ends_with([' ', '\t']) {
        stripped.trim_end()
    } else {
        rest
    };
    Some((level, text.to_string()))
}

/// `[text](url)` becomes `text` and `![alt](url)` becomes `alt`.
fn strip_links(line: &str) -> String {
    if !line.contains('[') {
        return line.to_string();
    }
    let chars: Vec<char> = line.chars().collect();
    if chars.len() > MAX_LINK_LINE_CHARS {
        return line.to_string();
    }
    let mut out = String::with_capacity(line.len());
    let mut i = 0;
    while let Some(&c) = chars.get(i) {
        let open = if c == '!' && chars.get(i + 1) == Some(&'[') {
            i + 1
        } else {
            i
        };
        if let Some((label_end, url_end)) = link_at(&chars, open) {
            out.extend(chars.get(open + 1..label_end).unwrap_or(&[]));
            i = url_end + 1;
        } else {
            out.push(c);
            i += 1;
        }
    }
    out
}

/// For a `[` at `open`: the index of its `]` and of the `)` that closes the
/// `(url)` right after it, within the label and url caps.
fn link_at(chars: &[char], open: usize) -> Option<(usize, usize)> {
    if chars.get(open) != Some(&'[') {
        return None;
    }
    // The label ends at the first `]`; another `[` first means this one is
    // not a link (a later `[` gets its own try). The url ends at the first
    // `)`; a `(` first means it is not a simple link. Stopping at the next
    // delimiter keeps the scans over a line disjoint, so they are linear.
    let label_end = (open + 1..chars.len().min(open + 1 + MAX_LABEL_CHARS))
        .find(|&j| matches!(chars[j], ']' | '['))
        .filter(|&j| chars[j] == ']')?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let url_start = label_end + 2;
    let url_end = (url_start..chars.len().min(url_start + MAX_URL_CHARS))
        .find(|&k| matches!(chars[k], ')' | '('))
        .filter(|&k| chars[k] == ')')?;
    Some((label_end, url_end))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(b: &[u8]) -> Parsed {
        parse(Parser::Text, b).unwrap()
    }
    fn md(s: &str) -> Parsed {
        parse(Parser::Markdown, s.as_bytes()).unwrap()
    }

    #[test]
    fn the_extension_picks_the_parser_case_insensitively() {
        assert_eq!(parser_for("notes.txt"), Some(Parser::Text));
        assert_eq!(parser_for("NOTES.TXT"), Some(Parser::Text));
        assert_eq!(parser_for("a.md"), Some(Parser::Markdown));
        assert_eq!(parser_for("a.markdown"), Some(Parser::Markdown));
        assert_eq!(parser_for("resume.pdf"), None);
        assert_eq!(parser_for("README"), None);
        assert_eq!(parser_for(""), None);
        assert_eq!(
            parser_for(".md"),
            None,
            "a dotfile with no stem is not a note"
        );
    }

    #[test]
    fn plain_text_passes_through() {
        let p = txt(b"Hello\nworld");
        assert_eq!(p.text, "Hello\nworld");
        assert_eq!(p.title, None);
        assert!(!p.truncated);
    }

    #[test]
    fn a_utf8_bom_is_dropped() {
        assert_eq!(txt(b"\xEF\xBB\xBFhello").text, "hello");
    }

    #[test]
    fn utf16_with_a_bom_is_decoded_both_ways() {
        let le: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("h\u{e9}llo".encode_utf16().flat_map(|u| u.to_le_bytes()))
            .collect();
        assert_eq!(txt(&le).text, "h\u{e9}llo");
        let be: Vec<u8> = [0xFE, 0xFF]
            .into_iter()
            .chain("h\u{e9}llo".encode_utf16().flat_map(|u| u.to_be_bytes()))
            .collect();
        assert_eq!(txt(&be).text, "h\u{e9}llo");
        // An odd trailing byte is ignored, not a panic.
        let mut odd = le.clone();
        odd.push(0x41);
        assert_eq!(txt(&odd).text, "h\u{e9}llo");
    }

    #[test]
    fn crlf_and_lone_cr_become_newlines() {
        assert_eq!(txt(b"a\r\nb\rc\n").text, "a\nb\nc\n");
    }

    #[test]
    fn a_windows_1252_file_keeps_its_accents() {
        // "café, naïve, 5 € and “quotes”" as Windows-1252 bytes.
        let bytes = b"caf\xe9, na\xefve, 5 \x80 and \x93quotes\x94";
        assert_eq!(
            decode_8bit(bytes, false, 1252).as_deref(),
            Ok("café, naïve, 5 € and “quotes”")
        );
        // Through the public entry point. This only says something on a
        // machine whose ANSI page is 1252, so it is skipped elsewhere; the
        // assertion above is the machine-independent one.
        if ansi_code_page() == 1252 {
            assert_eq!(txt(bytes).text, "café, naïve, 5 € and “quotes”");
        }
    }

    #[test]
    fn the_utf8_ansi_page_reads_a_non_utf8_file_lossily() {
        // "Beta: Use Unicode UTF-8" makes the ANSI page 65001.
        assert!(decode_8bit(b"caf\xe9", false, 65001).is_ok());
    }

    #[test]
    fn one_stray_byte_does_not_send_a_utf8_file_through_the_ansi_page() {
        let mut b = "日本語".repeat(20).into_bytes();
        b.push(0xff);
        let text = decode_8bit(&b, false, 1252).unwrap();
        assert!(text.starts_with("日本語日本語"));
    }

    #[test]
    fn other_code_pages_decode_by_the_page_given() {
        // Cyrillic "привет мир" in Windows-1251 (page 1251).
        let bytes = b"\xef\xf0\xe8\xe2\xe5\xf2 \xec\xe8\xf0";
        assert_eq!(decode_8bit(bytes, false, 1251).as_deref(), Ok("привет мир"));
    }

    #[test]
    fn valid_utf8_is_never_reread_as_a_code_page() {
        let s = "café ünï 日本語";
        assert_eq!(decode_8bit(s.as_bytes(), false, 1252).as_deref(), Ok(s));
        // With a BOM, through parse().
        let mut b = vec![0xEF, 0xBB, 0xBF];
        b.extend_from_slice(s.as_bytes());
        assert_eq!(txt(&b).text, s);
    }

    #[test]
    fn a_character_split_by_the_size_cap_is_a_cut_not_a_code_page() {
        // "é" is C3 A9; cut after C3.
        assert_eq!(
            decode_8bit(b"abc\xc3", true, 1252).as_deref(),
            Ok("abc\u{FFFD}")
        );
        // Not cut: a lone C3 at the end is a Windows-1252 "Ã".
        assert_eq!(decode_8bit(b"abc\xc3", false, 1252).as_deref(), Ok("abcÃ"));
    }

    #[test]
    fn binary_with_high_bytes_is_still_refused() {
        // Control bytes.
        let mut b = vec![0xE9u8; 10];
        b.extend((1..8u8).cycle().take(100));
        assert_eq!(decode_8bit(&b, false, 1252), Err(ParseError::NotText));
        // Symbols only.
        let junk: Vec<u8> = (0..500u32).map(|i| 0xA1 + (i % 0x10) as u8).collect();
        assert_eq!(decode_8bit(&junk, false, 1252), Err(ParseError::NotText));
    }

    #[test]
    fn utf16_without_a_bom_is_not_read_as_a_code_page() {
        // Every other byte is NUL, so it never reaches the code-page path.
        let b: Vec<u8> = "hello world"
            .encode_utf16()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        assert_eq!(parse(Parser::Text, &b), Err(ParseError::NotText));
    }

    #[test]
    fn a_nul_byte_means_binary() {
        assert_eq!(parse(Parser::Text, b"abc\0def"), Err(ParseError::NotText));
        assert_eq!(
            parse(Parser::Markdown, b"\x7fELF\0\0\0"),
            Err(ParseError::NotText)
        );
    }

    #[test]
    fn mostly_invalid_utf8_means_binary_but_a_stray_byte_is_tolerated() {
        let junk: Vec<u8> = (0..1000u32).map(|i| 0x80 + (i % 0x40) as u8).collect();
        assert_eq!(parse(Parser::Text, &junk), Err(ParseError::NotText));
        let mut mostly = b"a plain sentence with one bad byte: ".to_vec();
        mostly.push(0xFF);
        mostly.extend_from_slice(b" and the rest.");
        let p = txt(&mostly);
        assert!(p.text.starts_with("a plain sentence"));
        assert!(p.text.ends_with("and the rest."));
    }

    #[test]
    fn control_characters_become_spaces_but_tabs_newlines_and_page_breaks_stay() {
        assert_eq!(
            txt(b"a\x01b\x1bc\td\x0ce\nf\x7fg").text,
            "a b c\td\u{c}e\nf g"
        );
    }

    #[test]
    fn a_stray_zero_width_no_break_space_is_removed() {
        assert_eq!(txt("a\u{feff}b".as_bytes()).text, "ab");
    }

    #[test]
    fn a_file_over_the_size_cap_is_refused_before_it_is_decoded() {
        let big = vec![b'a'; MAX_FILE_BYTES + 1];
        assert_eq!(parse(Parser::Text, &big), Err(ParseError::TooLarge));
    }

    #[test]
    fn text_over_the_cap_is_cut_on_a_character_boundary_and_flagged() {
        let big = "\u{e9}".repeat(MAX_TEXT_BYTES); // 2 bytes each, 4 MB of text
        let p = txt(big.as_bytes());
        assert!(p.truncated);
        assert!(p.text.len() <= MAX_TEXT_BYTES);
        assert!(p.text.chars().all(|c| c == '\u{e9}'));
        let small = txt(b"fine");
        assert!(!small.truncated);
    }

    #[test]
    fn empty_input_is_empty_text_not_an_error() {
        let p = txt(b"");
        assert_eq!(p.text, "");
    }

    #[test]
    fn markdown_front_matter_is_dropped_and_its_title_used() {
        let p = md("---\ntitle: \"My Notes\"\ntags: [a, b]\n---\n# Heading\nbody\n");
        assert_eq!(p.title.as_deref(), Some("My Notes"));
        assert!(!p.text.contains("tags:"));
        assert!(p.text.starts_with("# Heading"));
    }

    #[test]
    fn an_unclosed_front_matter_is_left_as_text() {
        let p = md("---\ntitle: x\nbody without a close");
        assert!(p.text.contains("body without a close"));
        assert!(p.text.contains("title: x"));
    }

    #[test]
    fn the_first_heading_is_the_title_and_headings_stay_as_hash_lines() {
        let p = md("intro\n\n## Second\n# First\n###### Six\n");
        assert_eq!(p.title.as_deref(), Some("Second"));
        assert!(p.text.contains("## Second"));
        assert!(p.text.contains("# First"));
    }

    #[test]
    fn a_closing_hash_run_is_trimmed_from_a_heading() {
        let p = md("## Title ##\nbody");
        assert!(p.text.starts_with("## Title\n"), "{:?}", p.text);
    }

    #[test]
    fn links_and_images_keep_their_words_and_lose_the_url() {
        let p = md("See [the docs](https://example.com/a?b=1) and ![a diagram](img/x.png).");
        assert_eq!(p.text, "See the docs and a diagram.");
    }

    #[test]
    fn an_unfinished_link_is_left_alone() {
        let p = md("a [broken link](no close and [another");
        assert_eq!(p.text, "a [broken link](no close and [another");
    }

    #[test]
    fn code_fences_are_dropped_but_their_content_and_hashes_stay_literal() {
        let p = md("before\n```rust\n# not a heading [x](y)\nlet a = 1;\n```\nafter");
        assert_eq!(p.title, None, "a # inside a fence is not a heading");
        assert!(p.text.contains("# not a heading [x](y)"));
        assert!(p.text.contains("let a = 1;"));
        assert!(!p.text.contains("```"));
    }

    #[test]
    fn an_unclosed_fence_keeps_the_rest_as_code() {
        let p = md("```\nline one\nline two");
        assert!(p.text.contains("line two"));
    }

    #[test]
    fn html_comments_are_dropped_even_across_lines() {
        let p = md("keep <!-- gone --> this\n<!-- multi\nline\ncomment -->\nand this");
        // The start and end lines survive (empty); the line inside is skipped.
        assert_eq!(p.text, "keep  this\n\n\nand this");
    }

    #[test]
    fn deeply_nested_structures_do_not_blow_up() {
        let quotes = format!("{} deep", ">".repeat(100_000));
        let p = md(&quotes);
        assert!(p.text.contains("deep"));
        let brackets = format!("{}x{}", "[".repeat(50_000), "]".repeat(50_000));
        assert!(md(&brackets).text.contains('x'));
        let nested_links = "[a](".repeat(20_000);
        assert!(!md(&nested_links).text.is_empty());
        let lists: String = (0..5_000)
            .map(|i| format!("{}- item {i}\n", " ".repeat(i % 400)))
            .collect();
        assert!(md(&lists).text.contains("item 4999"));
        let many_opens = "![".repeat(30_000);
        assert!(!md(&many_opens).text.is_empty());
    }

    #[test]
    fn link_heavy_lines_under_the_line_limit_stay_fast() {
        // Each fixture is many lines just under MAX_LINK_LINE_CHARS, so the
        // link scan really runs on every line (a longer line is skipped).
        let fixtures = [
            "[a](".repeat(1_200),
            "[".repeat(4_900),
            "![".repeat(2_400),
            format!("[a]({}", "x".repeat(4_900)),
            "[a](x [b](y ".repeat(400),
            "[x]".repeat(1_600),
        ];
        for f in fixtures {
            assert!(
                f.chars().count() < MAX_LINK_LINE_CHARS,
                "fixture is vacuous"
            );
            let doc = vec![f.as_str(); 400].join("\n");
            let start = std::time::Instant::now();
            let p = md(&doc);
            assert!(!p.text.is_empty());
            assert!(
                start.elapsed() < std::time::Duration::from_secs(3),
                "took {:?}",
                start.elapsed()
            );
        }
    }

    #[test]
    fn parser_names_are_the_stored_ones() {
        // Stored in `document.parser` and `document.parser_ver`.
        assert_eq!(Parser::Text.name(), "txt");
        assert_eq!(Parser::Markdown.name(), "md");
        assert_eq!(PARSER_VERSION, 1);
    }

    #[test]
    fn a_link_with_a_title_still_loses_its_url() {
        assert_eq!(md("[x](http://a.b \"the title\") y").text, "x y");
    }

    #[test]
    fn a_utf32_bom_is_not_text() {
        assert_eq!(
            parse(Parser::Text, b"\xFF\xFE\x00\x00h\x00\x00\x00"),
            Err(ParseError::NotText)
        );
        assert_eq!(
            parse(Parser::Text, b"\x00\x00\xFE\xFF\x00\x00\x00h"),
            Err(ParseError::NotText)
        );
    }

    #[test]
    fn utf16_garbage_is_refused_like_utf8_garbage() {
        // Unpaired surrogates only: every unit decodes to a replacement.
        let mut b = vec![0xFF, 0xFE];
        for _ in 0..200 {
            b.extend_from_slice(&0xD800u16.to_le_bytes());
        }
        assert_eq!(parse(Parser::Text, &b), Err(ParseError::NotText));
    }

    #[test]
    fn titles_are_capped_like_headings() {
        let long = "t".repeat(5_000);
        let p = md(&format!("---\ntitle: {long}\n---\nbody"));
        assert_eq!(p.title.unwrap().chars().count(), 200);
        let p = md(&format!("# {long}\nbody"));
        assert_eq!(p.title.unwrap().chars().count(), 200);
    }

    #[test]
    fn hash_lines_in_a_code_fence_do_not_become_chunk_headings() {
        use crate::knowledge::chunk::chunk_text;
        let p = md("# Real\ntext\n```sh\n# a shell comment\nls\n```\nmore");
        assert!(p.text.contains("# a shell comment"));
        let chunks = chunk_text(&p.text);
        assert_eq!(chunks.len(), 1, "{chunks:?}");
        assert_eq!(chunks[0].heading.as_deref(), Some("Real"));
    }

    #[test]
    fn a_huge_input_is_cut_before_it_is_decoded() {
        let big = vec![b'a'; MAX_FILE_BYTES];
        let p = parse(Parser::Text, &big).unwrap();
        assert!(p.truncated);
        assert!(p.text.len() <= MAX_TEXT_BYTES);
        // UTF-16 too.
        let mut u16s = vec![0xFF, 0xFE];
        u16s.extend(std::iter::repeat_n(b'a', MAX_FILE_BYTES - 2));
        let p = parse(Parser::Text, &u16s).unwrap();
        assert!(p.truncated);
        assert!(p.text.len() <= MAX_TEXT_BYTES);
    }

    #[test]
    fn markdown_goes_through_the_same_byte_checks() {
        assert_eq!(parse(Parser::Markdown, b"# t\0"), Err(ParseError::NotText));
        assert_eq!(
            md("\u{feff}# Title\r\nbody").title.as_deref(),
            Some("Title")
        );
    }

    // -- html, csv, json (K1 readers) -------------------------------------------

    fn read(parser: Parser, s: &str) -> Parsed {
        parse(parser, s.as_bytes()).unwrap()
    }

    #[test]
    fn html_csv_and_json_files_get_their_own_readers() {
        assert_eq!(parser_for("a.html"), Some(Parser::Html));
        assert_eq!(parser_for("A.HTM"), Some(Parser::Html));
        assert_eq!(parser_for("t.csv"), Some(Parser::Csv));
        assert_eq!(parser_for("t.tsv"), Some(Parser::Csv));
        assert_eq!(parser_for("d.json"), Some(Parser::Json));
        assert_eq!(parser_for("d.jsonl"), None);
        assert_eq!(Parser::Html.name(), "html");
        assert_eq!(Parser::Csv.name(), "csv");
        assert_eq!(Parser::Json.name(), "json");
    }

    #[test]
    fn html_gives_visible_text_only() {
        let p = read(
            Parser::Html,
            "<html><head><title>My page</title><style>p{color:red}</style>\
             <script>var secret = 'nope';</script></head>\
             <body><!-- hidden note --><h1>Hello &amp; welcome</h1>\
             <p>Fish&nbsp;&amp;&nbsp;chips &#8364;5 &lt;b&gt;</p>\
             <noscript>enable js</noscript><img src=\"http://x.test/a.png\"></body></html>",
        );
        assert!(p.text.contains("My page"), "{}", p.text);
        assert!(p.text.contains("Hello & welcome"), "{}", p.text);
        assert!(p.text.contains("Fish & chips \u{20AC}5 <b>"), "{}", p.text);
        for hidden in ["secret", "color:red", "hidden note", "enable js", "x.test"] {
            assert!(!p.text.contains(hidden), "{hidden}: {}", p.text);
        }
        assert!(!p.truncated);
    }

    #[test]
    fn hostile_html_never_panics_and_stays_capped() {
        let cases = [
            "<".to_string(),
            "<<<<<<".to_string(),
            "<script>never closed".to_string(),
            "<!-- never closed".to_string(),
            "<div class=\"never closed".to_string(),
            "&#xFFFFFFFF; &#0; &#55296; &;&&&&;".to_string(),
            "日本語<p>🙂</p>é<".to_string(),
            "<div>".repeat(300_000),
            "&amp;".repeat(500_000),
            "<script>".repeat(100_000),
        ];
        for html in &cases {
            let p = parse(Parser::Html, html.as_bytes()).unwrap();
            assert!(p.text.len() <= MAX_TEXT_BYTES);
        }
        assert_eq!(
            read(Parser::Html, "日本語<p>🙂</p>é<").text,
            "日本語\n🙂\né<"
        );
    }

    #[test]
    fn html_never_makes_a_heading_for_the_chunker() {
        let p = read(Parser::Html, "<p># not a heading</p><p>ok</p><p>a # b</p>");
        assert!(p.text.lines().all(|l| !l.starts_with('#')), "{}", p.text);
        assert!(p.text.contains("# not a heading"), "{}", p.text);
    }

    #[test]
    fn json_over_a_megabyte_or_cut_short_is_indexed_as_plain_text() {
        // Valid, but past the 1 MB the JSON reader will parse.
        let mut big = String::from("[");
        while big.len() < 1_200_000 {
            big.push_str("{\"k\":\"word\"},");
        }
        big.push_str("{\"k\":\"end\"}]");
        let p = parse(Parser::Json, big.as_bytes()).unwrap();
        assert!(
            p.text.starts_with("[{\"k\":\"word\"}"),
            "kept as the raw text"
        );
        assert!(!p.text.contains("[0].k:"), "not flattened");
        // Over 2 MB the file is cut, which is no longer valid JSON: also text.
        let mut huge = String::from("[");
        while huge.len() < 2_300_000 {
            huge.push_str("{\"k\":\"word\"},");
        }
        huge.push_str("{\"k\":\"end\"}]");
        let p = parse(Parser::Json, huge.as_bytes()).unwrap();
        assert!(p.truncated);
        assert!(
            p.text.starts_with("[{\"k\":\"word\"}"),
            "kept as the raw text"
        );
        assert!(!p.text.contains("[0].k:"), "not flattened");
    }

    #[test]
    fn html_goes_through_the_same_byte_checks() {
        assert_eq!(
            parse(Parser::Html, b"<p>a\0b</p>"),
            Err(ParseError::NotText)
        );
    }

    #[test]
    fn csv_rows_are_read_with_their_header() {
        let p = read(
            Parser::Csv,
            "Name,Role,Start\nJane,\"Engineer, senior\",2019\nBob,Designer,\n",
        );
        assert_eq!(
            p.text,
            "Name | Role | Start\n\
             Name: Jane; Role: Engineer, senior; Start: 2019\n\
             Name: Bob; Role: Designer"
        );
        assert!(!p.truncated);
    }

    #[test]
    fn csv_handles_quotes_newlines_other_separators_and_ragged_rows() {
        // A quoted newline and a doubled quote stay inside one value.
        let p = read(Parser::Csv, "a,b\n\"line one\nline \"\"two\"\"\",x\n");
        assert_eq!(p.text, "a | b\na: line one line \"two\"; b: x");
        // Semicolon and tab separators are found from the first line.
        assert_eq!(read(Parser::Csv, "a;b\n1;2\n").text, "a | b\na: 1; b: 2");
        assert_eq!(read(Parser::Csv, "a\tb\n1\t2\n").text, "a | b\na: 1; b: 2");
        // Too many cells get a column number; too few are just absent.
        assert_eq!(
            read(Parser::Csv, "a,b\n1,2,3\n4\n").text,
            "a | b\na: 1; b: 2; column 3: 3\na: 4"
        );
        // Blank lines and an empty file.
        assert_eq!(
            read(Parser::Csv, "a,b\n\n\n1,2\n").text,
            "a | b\na: 1; b: 2"
        );
        assert_eq!(read(Parser::Csv, "").text, "");
        // CRLF line ends.
        assert_eq!(
            read(Parser::Csv, "a,b\r\n1,2\r\n").text,
            "a | b\na: 1; b: 2"
        );
    }

    #[test]
    fn csv_never_makes_a_heading_for_the_chunker() {
        let p = read(Parser::Csv, "#tag,b\n1,2\n");
        assert!(p.text.lines().all(|l| !l.starts_with('#')), "{}", p.text);
        assert!(p.text.contains("tag: 1"), "{}", p.text);
    }

    #[test]
    fn hostile_csv_never_panics_and_stays_capped() {
        let mut big = String::from("h1,h2\n");
        while big.len() < 3 * 1024 * 1024 {
            big.push_str("some value,another value\n");
        }
        let p = parse(Parser::Csv, big.as_bytes()).unwrap();
        assert!(p.text.len() <= MAX_TEXT_BYTES);
        assert!(p.truncated);
        for s in [
            "\"never closed,a\nb,c".to_string(),
            "\"".repeat(100_001),
            ",".repeat(1_000_000),
            "日本語,🙂\né,ü\n".to_string(),
            format!("{}\n1\n", "x,".repeat(100_000)),
        ] {
            let p = parse(Parser::Csv, s.as_bytes()).unwrap();
            assert!(p.text.len() <= MAX_TEXT_BYTES);
        }
    }

    #[test]
    fn json_is_flattened_to_path_value_lines() {
        let p = read(
            Parser::Json,
            r#"{"name":"Jane","address":{"city":"Paris","zip":75001},
                "skills":["rust","win32"],"jobs":[{"firm":"Acme","years":3}],
                "retired":false,"nothing":null,"note":"two\nlines"}"#,
        );
        assert_eq!(
            p.text,
            "name: Jane\n\
             address.city: Paris\n\
             address.zip: 75001\n\
             skills[0]: rust\n\
             skills[1]: win32\n\
             jobs[0].firm: Acme\n\
             jobs[0].years: 3\n\
             retired: false\n\
             note: two lines"
        );
        assert_eq!(read(Parser::Json, "[1,\"a\"]").text, "[0]: 1\n[1]: a");
        assert_eq!(read(Parser::Json, "\"just text\"").text, "just text");
        assert_eq!(read(Parser::Json, "{}").text, "");
    }

    #[test]
    fn json_that_is_not_valid_reads_as_plain_text() {
        let p = read(Parser::Json, "{\"a\": 1,\n trailing words");
        assert_eq!(p.text, "{\"a\": 1,\n trailing words");
        // Nested far past the parser's depth limit: an error, not a crash.
        let deep = format!("{}1{}", "[".repeat(5_000), "]".repeat(5_000));
        assert!(parse(Parser::Json, deep.as_bytes()).is_ok());
    }

    #[test]
    fn json_keys_never_make_a_heading_and_odd_text_is_safe() {
        let p = read(
            Parser::Json,
            r##"{"# title":"x","日本":"🙂","":"empty key"}"##,
        );
        assert!(p.text.lines().all(|l| !l.starts_with('#')), "{}", p.text);
        assert!(p.text.contains("title: x"), "{}", p.text);
        assert!(p.text.contains("日本: 🙂"), "{}", p.text);
        assert!(p.text.contains(": empty key"), "{}", p.text);
    }

    #[test]
    fn big_json_is_capped_and_marked_truncated() {
        let mut big = String::from("[");
        while big.len() < 1_500_000 {
            big.push_str("{\"k\":\"some longer value here\"},");
        }
        big.push_str("{\"k\":1}]");
        let p = parse(Parser::Json, big.as_bytes()).unwrap();
        assert!(p.text.len() <= MAX_TEXT_BYTES);
        let small = format!("[{}0]", "\"x\",".repeat(220_000));
        let p = parse(Parser::Json, small.as_bytes()).unwrap();
        assert!(p.text.len() <= MAX_TEXT_BYTES);
        assert!(p.truncated);
        // A long key is cut, never a panic on a character boundary.
        let key = "é".repeat(5_000);
        let p = read(Parser::Json, &format!("{{\"{key}\":\"v\"}}"));
        assert!(p.text.chars().count() < 1_000, "{}", p.text.len());
    }
}
