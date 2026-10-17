//! Plain text and Markdown to text, for the chunker.

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
}

impl Parser {
    pub fn name(self) -> &'static str {
        match self {
            Parser::Text => "txt",
            Parser::Markdown => "md",
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
        _ => None,
    }
}

/// Reads a file's bytes as text. Never panics: size caps, binary detection and
/// character-boundary-safe truncation make every input either text or an error.
pub fn parse(parser: Parser, bytes: &[u8]) -> Result<Parsed, ParseError> {
    let decoded = decode(bytes)?;
    let (normalized, truncated) = truncate(normalize(&decoded), MAX_TEXT_BYTES);
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
    }
}

/// UTF-8 (with or without a BOM) and UTF-16 with a BOM. A NUL byte in
/// anything else, or mostly undecodable bytes, means a binary file.
fn decode(bytes: &[u8]) -> Result<String, ParseError> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ParseError::TooLarge);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        let units = rest
            .chunks_exact(2)
            .map(|p| u16::from_le_bytes([p[0], p[1]]));
        return Ok(char::decode_utf16(units)
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect());
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        let units = rest
            .chunks_exact(2)
            .map(|p| u16::from_be_bytes([p[0], p[1]]));
        return Ok(char::decode_utf16(units)
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect());
    }
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    if bytes.contains(&0) {
        return Err(ParseError::NotText);
    }
    let text = String::from_utf8_lossy(bytes).into_owned();
    let bad = text.chars().filter(|&c| c == '\u{FFFD}').count();
    if bad > 0 && bad * 10 > text.chars().count() {
        return Err(ParseError::NotText);
    }
    Ok(text)
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

/// Longest line whose links are rewritten. Past this the line is kept as is,
/// which keeps the link scan linear on adversarial input.
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
            out.push(line.to_string());
            continue;
        }
        let Some(line) = strip_comments(line, &mut in_comment) else {
            continue;
        };
        if let Some((level, heading)) = atx_heading(&line) {
            if first_heading.is_none() && !heading.is_empty() {
                first_heading = Some(heading.clone());
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
        (!v.is_empty()).then(|| v.to_string())
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
    let label_end =
        (open + 1..chars.len().min(open + 1 + MAX_LABEL_CHARS)).find(|&j| chars[j] == ']')?;
    if chars.get(label_end + 1) != Some(&'(') {
        return None;
    }
    let url_start = label_end + 2;
    let url_end =
        (url_start..chars.len().min(url_start + MAX_URL_CHARS)).find(|&k| chars[k] == ')')?;
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
    fn markdown_goes_through_the_same_byte_checks() {
        assert_eq!(parse(Parser::Markdown, b"# t\0"), Err(ParseError::NotText));
        assert_eq!(
            md("\u{feff}# Title\r\nbody").title.as_deref(),
            Some("Title")
        );
    }
}
