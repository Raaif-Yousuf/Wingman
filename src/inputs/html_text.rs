//! A small, safe HTML-to-text stripper for fetched pages (#127). Pure logic:
//! no network, no dependency, linear time on any input.
//!
//! The text goes to a model as data, so the goals are modest: drop everything
//! that is not prose (scripts, styles, comments, tags), keep the title and
//! the line structure, decode the common entities, and remove characters that
//! can hide or reorder text. It does not run scripts, follow links or load
//! anything, and it never fails: malformed markup just yields less text.

/// The text of an HTML document: the title first (when there is one), then
/// the body text with one line per block element. Whitespace runs collapse,
/// blank lines are dropped, and invisible or direction-changing characters
/// are removed.
pub fn html_to_text(html: &str) -> String {
    let bytes = html.as_bytes();
    let mut body = String::new();
    let mut title = String::new();
    let mut in_title = false;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'<' && starts_markup(bytes, i) {
            if html[i..].starts_with("<!--") {
                match html[i + 4..].find("-->") {
                    Some(p) => i += 4 + p + 3,
                    None => break,
                }
                continue;
            }
            let Some(end) = tag_end(bytes, i + 1) else {
                break;
            };
            let inner = &html[i + 1..end];
            i = end + 1;
            let (closing, name) = tag_name(inner);
            if !closing && SKIPPED.contains(&name.as_str()) {
                // Everything up to the matching end tag is not prose.
                let Some(close) = find_ci(bytes, format!("</{name}").as_bytes(), i) else {
                    break;
                };
                let Some(gt) = bytes[close..].iter().position(|&b| b == b'>') else {
                    break;
                };
                i = close + gt + 1;
                continue;
            }
            if name == "title" {
                in_title = !closing;
            } else if BLOCKS.contains(&name.as_str()) {
                body.push('\n');
            }
            continue;
        }
        // A text run: up to the next `<` that starts markup.
        let mut next = i + 1;
        while next < bytes.len() && !(bytes[next] == b'<' && starts_markup(bytes, next)) {
            next += 1;
        }
        let run = decode_entities(&html[i..next]);
        let target = if in_title { &mut title } else { &mut body };
        target.extend(run.chars().map(|c| if c.is_whitespace() { ' ' } else { c }));
        i = next;
    }
    let title = clean(&title, false);
    let body = clean(&body, false);
    match (title.is_empty(), body.is_empty()) {
        (true, _) => body,
        (false, true) => title,
        (false, false) => format!("{title}\n{body}"),
    }
}

/// Plain text made safe the same way: control and invisible characters
/// dropped, line endings normalised, blank runs collapsed to one blank line.
pub fn plain_to_text(text: &str) -> String {
    clean(&text.replace("\r\n", "\n").replace('\r', "\n"), true)
}

/// Elements whose content is not prose.
const SKIPPED: &[&str] = &[
    "script", "style", "noscript", "template", "svg", "iframe", "object",
];

/// Elements that start a new line.
const BLOCKS: &[&str] = &[
    "p",
    "div",
    "br",
    "li",
    "ul",
    "ol",
    "tr",
    "table",
    "section",
    "article",
    "header",
    "footer",
    "nav",
    "main",
    "aside",
    "blockquote",
    "pre",
    "hr",
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "dt",
    "dd",
    "dl",
    "figure",
    "figcaption",
    "form",
    "fieldset",
    "address",
    "body",
    "html",
];

/// Whether the `<` at `i` starts a tag, comment or declaration (a letter,
/// `/`, `!` or `?` follows). Any other `<` is just a character.
fn starts_markup(bytes: &[u8], i: usize) -> bool {
    bytes
        .get(i + 1)
        .is_some_and(|b| b.is_ascii_alphabetic() || matches!(b, b'/' | b'!' | b'?'))
}

/// The index of the `>` that ends a tag whose text starts at `from`, not
/// counting a `>` inside a quoted attribute value. `None` when it never ends.
fn tag_end(bytes: &[u8], from: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut prev = 0u8;
    for (off, &b) in bytes[from..].iter().enumerate() {
        match quote {
            Some(q) => {
                if b == q {
                    quote = None;
                }
            }
            None => match b {
                b'>' => return Some(from + off),
                b'"' | b'\'' if prev == b'=' => quote = Some(b),
                _ => {}
            },
        }
        if !b.is_ascii_whitespace() {
            prev = b;
        }
    }
    None
}

/// Whether a tag is a closing one, and its lower-cased name (empty for a
/// declaration such as `!doctype`).
fn tag_name(inner: &str) -> (bool, String) {
    let (closing, rest) = match inner.strip_prefix('/') {
        Some(r) => (true, r),
        None => (false, inner),
    };
    let name = rest
        .chars()
        .take_while(char::is_ascii_alphanumeric)
        .collect::<String>()
        .to_ascii_lowercase();
    (closing, name)
}

/// The first index at or after `from` where `needle` occurs in `hay`,
/// ignoring ASCII case.
fn find_ci(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (from..=hay.len() - needle.len())
        .find(|&p| hay[p..p + needle.len()].eq_ignore_ascii_case(needle))
}

/// Decodes the common named entities and every numeric one. Anything it does
/// not recognise stays as typed; a number that is not a character (NUL, a
/// surrogate, past U+10FFFF) is dropped.
fn decode_entities(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        // The `;` must come soon: the longest name or number is short.
        let semi = rest.char_indices().take(12).find(|&(_, c)| c == ';');
        match semi.and_then(|(semi, _)| Some((semi, decode_one(&rest[1..semi])?))) {
            Some((semi, decoded)) => {
                out.push_str(&decoded);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text for one entity body (between `&` and `;`), or `None` when it is
/// not an entity. A valid number that is not a character gives `Some("")`.
fn decode_one(body: &str) -> Option<String> {
    if let Some(num) = body.strip_prefix('#') {
        let (digits, radix) = match num.strip_prefix(['x', 'X']) {
            Some(hex) => (hex, 16),
            None => (num, 10),
        };
        if digits.is_empty() || digits.len() > 8 {
            return None;
        }
        let n = u32::from_str_radix(digits, radix).ok()?;
        return Some(match char::from_u32(n) {
            Some(c) if n != 0 => c.to_string(),
            _ => String::new(),
        });
    }
    let c = match body {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        "nbsp" | "ensp" | "emsp" | "thinsp" => " ",
        "shy" => "",
        "ndash" => "\u{2013}",
        // A dash with spaces: this crate keeps the em dash out of its string
        // literals (rule 11), and the model reads either the same.
        "mdash" => " - ",
        "hellip" => "\u{2026}",
        "lsquo" => "\u{2018}",
        "rsquo" => "\u{2019}",
        "ldquo" => "\u{201C}",
        "rdquo" => "\u{201D}",
        "bull" => "\u{2022}",
        "middot" => "\u{B7}",
        "laquo" => "\u{AB}",
        "raquo" => "\u{BB}",
        "copy" => "\u{A9}",
        "reg" => "\u{AE}",
        "trade" => "\u{2122}",
        "euro" => "\u{20AC}",
        "pound" => "\u{A3}",
        "cent" => "\u{A2}",
        "deg" => "\u{B0}",
        "times" => "\u{D7}",
        "divide" => "\u{F7}",
        "plusmn" => "\u{B1}",
        _ => return None,
    };
    Some(c.to_string())
}

/// Characters that hide or reorder text, or carry data no reader sees:
/// controls, zero-width and direction marks and isolates, the soft hyphen,
/// the byte order mark and the invisible "tag" block.
fn is_hidden(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00AD}'
                | '\u{061C}'
                | '\u{180E}'
                | '\u{200B}'..='\u{200F}'
                | '\u{202A}'..='\u{202E}'
                | '\u{2060}'..='\u{206F}'
                | '\u{FEFF}'
                | '\u{FFF9}'..='\u{FFFB}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// Lines trimmed, runs of spaces collapsed, hidden characters dropped. Blank
/// lines are dropped, or (`keep_blank`) shrunk to one blank line per run.
fn clean(text: &str, keep_blank: bool) -> String {
    let mut lines: Vec<String> = Vec::new();
    let mut last_blank = true;
    for raw in text.split('\n') {
        let mut line = String::with_capacity(raw.len());
        let mut pending_space = false;
        for c in raw.chars() {
            if c.is_whitespace() {
                pending_space = !line.is_empty();
            } else if !is_hidden(c) {
                if pending_space {
                    line.push(' ');
                    pending_space = false;
                }
                line.push(c);
            }
        }
        if line.is_empty() {
            if keep_blank && !last_blank {
                lines.push(String::new());
            }
            last_blank = true;
        } else {
            lines.push(line);
            last_blank = false;
        }
    }
    while lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_are_removed_and_block_elements_become_lines() {
        let t = html_to_text(
            "<html><body><h1>Title</h1><p>One <b>bold</b> word.</p><p>Two.</p></body></html>",
        );
        assert_eq!(t, "Title\nOne bold word.\nTwo.");
    }

    #[test]
    fn the_title_comes_first_and_the_head_is_otherwise_dropped() {
        let t = html_to_text(
            "<html><head><title>My page</title><meta charset=utf-8><style>p{color:red}</style></head><body><p>Hello</p></body></html>",
        );
        assert_eq!(t, "My page\nHello");
    }

    #[test]
    fn script_style_and_comments_are_dropped_with_their_content() {
        let t = html_to_text(
            "<p>a</p><script>alert('x<p>y</p>')</script><style>.a{}</style><!-- <p>hidden</p> --><noscript>no js</noscript><p>b</p>",
        );
        assert_eq!(t, "a\nb");
    }

    #[test]
    fn script_end_tag_matching_ignores_case() {
        let t = html_to_text("<p>a</p><SCRIPT>bad()</ScRiPt><p>b</p>");
        assert_eq!(t, "a\nb");
    }

    #[test]
    fn a_greater_than_sign_inside_a_quoted_attribute_does_not_end_the_tag() {
        let t = html_to_text("<a title=\"1 > 0\" href='x>y'>link text</a> after");
        assert_eq!(t, "link text after");
    }

    #[test]
    fn an_unterminated_tag_or_comment_drops_the_rest_instead_of_leaking_markup() {
        assert_eq!(html_to_text("<p>keep</p><div class=\"x"), "keep");
        assert_eq!(
            html_to_text("<p>keep</p><!-- never closed <p>no</p>"),
            "keep"
        );
        assert_eq!(html_to_text("<p>keep</p><script>never closed"), "keep");
    }

    #[test]
    fn common_entities_are_decoded() {
        let t = html_to_text(
            "<p>Fish &amp; chips &lt;3 &quot;q&quot; it&#39;s&nbsp;ok &#x41;&#66; &mdash; &hellip;</p>",
        );
        assert_eq!(t, "Fish & chips <3 \"q\" it's ok AB - \u{2026}");
    }

    #[test]
    fn unknown_or_broken_entities_stay_as_typed_and_bad_numbers_are_dropped() {
        assert_eq!(
            html_to_text("<p>&bogus; &amp &#; &#x;</p>"),
            "&bogus; &amp &#; &#x;"
        );
        // NUL, a surrogate and an out-of-range number decode to nothing.
        assert_eq!(html_to_text("<p>a&#0;b&#xD800;c&#x110000;d</p>"), "abcd");
    }

    #[test]
    fn decoded_markup_is_text_not_a_tag() {
        // `&lt;script&gt;` must stay the literal text, never be re-parsed.
        assert_eq!(
            html_to_text("<p>&lt;script&gt;x&lt;/script&gt;</p>"),
            "<script>x</script>"
        );
    }

    #[test]
    fn whitespace_collapses_and_blank_lines_are_dropped() {
        let t = html_to_text("<p>a   \n\t b</p>\n\n\n<p></p><p></p><div>\n</div><p>c</p>");
        assert_eq!(t, "a b\nc");
    }

    #[test]
    fn br_and_list_items_break_lines() {
        assert_eq!(
            html_to_text("a<br>b<br/>c<ul><li>x</li><li>y</li></ul>"),
            "a\nb\nc\nx\ny"
        );
    }

    #[test]
    fn invisible_and_direction_characters_are_removed() {
        let t = html_to_text("<p>pa\u{200B}ss\u{202E}word\u{FEFF} \u{2066}x\u{2069}\u{0007}</p>");
        assert_eq!(t, "password x");
    }

    #[test]
    fn the_invisible_tag_block_used_to_smuggle_text_is_removed() {
        let t = html_to_text("<p>hi\u{E0049}\u{E0067}\u{E006E} there</p>");
        assert_eq!(t, "hi there");
    }

    #[test]
    fn a_lone_less_than_sign_is_text() {
        assert_eq!(html_to_text("<p>1 < 2 and 3 <4</p>"), "1 < 2 and 3 <4");
    }

    #[test]
    fn plain_text_is_cleaned_the_same_way() {
        assert_eq!(
            plain_to_text("a\r\nb\u{200B}\r\n\r\n\r\n\r\nc\u{0000}"),
            "a\nb\n\nc"
        );
    }

    #[test]
    fn hostile_input_does_not_panic_or_blow_up() {
        let nested = "<div>".repeat(50_000);
        let _ = html_to_text(&nested);
        let lt = "<".repeat(100_000);
        let _ = html_to_text(&lt);
        let amp = "&#".repeat(100_000);
        let _ = html_to_text(&amp);
        let multibyte = "<p>\u{1F600}\u{00E9}</p><".to_string();
        assert_eq!(html_to_text(&multibyte), "\u{1F600}\u{00E9}\n<");
    }
}
