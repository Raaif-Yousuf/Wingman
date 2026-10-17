//! Splits parsed text into chunks for the full-text index.

pub const TARGET_CHARS: usize = 2000;
pub const OVERLAP_CHARS: usize = 200;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    pub ordinal: usize,
    pub text: String,
    pub heading: Option<String>,
    pub page: u32,
    pub start: usize,
    pub end: usize,
    pub tokens: usize,
}

const MAX_HEADING_CHARS: usize = 200;

/// An unbroken piece of text of at most `TARGET_CHARS`: a line, or a slice of
/// a longer line cut at a sentence or word.
struct Seg {
    start: usize,
    end: usize,
    chars: usize,
    heading: Option<usize>,
    page: u32,
    /// A heading or a page break starts here: the chunk closes before it and
    /// no overlap is carried across.
    force_break: bool,
}

struct Open {
    start: usize,
    end: usize,
    chars: usize,
    first_seg: usize,
}

/// Splits parsed text into chunks of about `TARGET_CHARS` characters, at a
/// heading, then a line or paragraph, then a sentence or word, and only as a
/// last resort inside a word. A line is never cut unless it is longer than a
/// whole chunk, so a table row stays whole. A form feed is a page break.
/// Every chunk is a contiguous slice of `text` (`start..end` are byte
/// offsets), and consecutive chunks of one section overlap by up to
/// `OVERLAP_CHARS`. Never panics, and runs in time linear in the text.
pub fn chunk_text(text: &str) -> Vec<Chunk> {
    let (segs, headings) = segments(text);
    let mut out: Vec<Chunk> = Vec::new();
    let mut cur: Option<Open> = None;
    for (i, seg) in segs.iter().enumerate() {
        let Some(open) = cur.as_mut() else {
            cur = Some(Open {
                start: seg.start,
                end: seg.end,
                chars: seg.chars,
                first_seg: i,
            });
            continue;
        };
        let gap = text
            .get(open.end..seg.start)
            .map_or(usize::MAX, |g| g.chars().count());
        if !seg.force_break
            && open.chars.saturating_add(gap).saturating_add(seg.chars) <= TARGET_CHARS
        {
            open.end = seg.end;
            open.chars += gap + seg.chars;
            continue;
        }
        let prev = cur.take().unwrap_or(Open {
            start: seg.start,
            end: seg.start,
            chars: 0,
            first_seg: i,
        });
        let own_start = segs.get(prev.first_seg).map_or(prev.start, |s| s.start);
        let overlap = if seg.force_break || gap.saturating_add(seg.chars) >= TARGET_CHARS {
            None
        } else {
            let budget = OVERLAP_CHARS.min(TARGET_CHARS - gap - seg.chars);
            overlap_start(text, own_start, prev.end, budget)
        };
        out.push(make_chunk(text, &prev, &segs, &headings, out.len()));
        let start = overlap.unwrap_or(seg.start);
        let chars = text
            .get(start..seg.end)
            .map_or(seg.chars, |t| t.chars().count());
        cur = Some(Open {
            start,
            end: seg.end,
            chars,
            first_seg: i,
        });
    }
    if let Some(last) = cur {
        out.push(make_chunk(text, &last, &segs, &headings, out.len()));
    }
    out
}

fn make_chunk(text: &str, open: &Open, segs: &[Seg], headings: &[String], ordinal: usize) -> Chunk {
    let body = text.get(open.start..open.end).unwrap_or("");
    let first = segs.get(open.first_seg);
    Chunk {
        ordinal,
        text: body.to_string(),
        heading: first
            .and_then(|s| s.heading)
            .and_then(|h| headings.get(h))
            .cloned(),
        page: first.map_or(1, |s| s.page),
        start: open.start,
        end: open.end,
        tokens: open.chars.div_ceil(4),
    }
}

/// Where the overlap for the next chunk starts: within the last `budget`
/// characters of `text[own_start..prev_end]`, moved forward to a line start,
/// or to a word start when the tail is inside one long line. `None` when the
/// tail is inside a table row (a row is never cut) or nothing is left.
fn overlap_start(text: &str, own_start: usize, prev_end: usize, budget: usize) -> Option<usize> {
    if budget == 0 {
        return None;
    }
    let prev = text.get(own_start..prev_end)?;
    let tail_start = match prev.char_indices().rev().nth(budget - 1) {
        Some((i, _)) => own_start + i,
        None => own_start,
    };
    let at_line_start = tail_start == 0
        || tail_start == own_start
        || text.get(..tail_start).is_some_and(|t| t.ends_with('\n'));
    let tail = text.get(tail_start..prev_end)?;
    let start = if at_line_start {
        tail_start
    } else if let Some(nl) = tail.find('\n') {
        tail_start + nl + 1
    } else {
        let line_start = text
            .get(..tail_start)
            .and_then(|t| t.rfind('\n'))
            .map_or(0, |i| i + 1);
        if text.get(line_start..)?.trim_start().starts_with('|') {
            return None;
        }
        let ws = tail.find(char::is_whitespace)?;
        tail_start + ws + 1
    };
    let rest = text.get(start..prev_end)?;
    let lead = rest.len() - rest.trim_start().len();
    let start = start + lead;
    (start < prev_end).then_some(start)
}

/// Lines, form-feed pages and long-line slices, in order.
fn segments(text: &str) -> (Vec<Seg>, Vec<String>) {
    let mut segs: Vec<Seg> = Vec::new();
    let mut headings: Vec<String> = Vec::new();
    let mut heading: Option<usize> = None;
    let mut page: u32 = 1;
    let mut break_pending = false;
    let mut line_start = 0usize;
    for line in text.split('\n') {
        let mut part_start = line_start;
        for (pi, part) in line.split('\u{c}').enumerate() {
            if pi > 0 {
                page = page.saturating_add(1);
                break_pending = true;
            }
            let t = part.trim_start();
            let content = t.trim_end();
            if !content.is_empty() {
                let start = part_start + (part.len() - t.len());
                let mut force = break_pending;
                break_pending = false;
                if let Some(h) = heading_text(content) {
                    force = true;
                    if !h.is_empty() {
                        headings.push(h);
                        heading = Some(headings.len() - 1);
                    }
                }
                push_line(&mut segs, content, start, heading, page, force);
            }
            part_start += part.len() + 1;
        }
        line_start += line.len() + 1;
    }
    (segs, headings)
}

/// The text of a `#` to `######` heading line, capped; `None` for any other
/// line (`#tag` is not a heading).
fn heading_text(line: &str) -> Option<String> {
    let level = line.chars().take_while(|&c| c == '#').count();
    if !(1..=6).contains(&level) {
        return None;
    }
    let rest = line.get(level..)?;
    if !(rest.is_empty() || rest.starts_with([' ', '\t'])) {
        return None;
    }
    let rest = rest.trim();
    let stripped = rest.trim_end_matches('#');
    let rest = if stripped.is_empty() || stripped.ends_with([' ', '\t']) {
        stripped.trim_end()
    } else {
        rest
    };
    Some(rest.chars().take(MAX_HEADING_CHARS).collect())
}

fn push_line(
    segs: &mut Vec<Seg>,
    content: &str,
    start: usize,
    heading: Option<usize>,
    page: u32,
    force: bool,
) {
    let mut rest = content;
    let mut offset = start;
    let mut first = true;
    loop {
        let window_end = rest.char_indices().nth(TARGET_CHARS).map(|(i, _)| i);
        let Some(window_end) = window_end else {
            segs.push(Seg {
                start: offset,
                end: offset + rest.len(),
                chars: rest.chars().count(),
                heading,
                page,
                force_break: force && first,
            });
            return;
        };
        let cut = best_cut(rest, window_end);
        let piece = rest.get(..cut).unwrap_or("").trim_end();
        if !piece.is_empty() {
            segs.push(Seg {
                start: offset,
                end: offset + piece.len(),
                chars: piece.chars().count(),
                heading,
                page,
                force_break: force && first,
            });
            first = false;
        }
        let next = rest.get(cut..).unwrap_or("");
        let lead = next.len() - next.trim_start().len();
        offset += cut + lead;
        rest = next.get(lead..).unwrap_or("");
        if rest.is_empty() {
            return;
        }
    }
}

/// Where to cut `rest` (whose first `window_end` bytes are `TARGET_CHARS`
/// characters): after the last sentence end in the second half of the window,
/// else at the last whitespace in it, else at the window end. Always a
/// character boundary and at least 1.
fn best_cut(rest: &str, window_end: usize) -> usize {
    let half = window_end / 2;
    let mut sentence: Option<usize> = None;
    let mut space: Option<usize> = None;
    for (i, c) in rest.char_indices() {
        if i >= window_end {
            break;
        }
        if c.is_whitespace() {
            if i >= half {
                space = Some(i);
            }
            continue;
        }
        let end = i + c.len_utf8();
        let terminator = match c {
            '.' | '!' | '?' => rest
                .get(end..)
                .and_then(|r| r.chars().next())
                .is_some_and(char::is_whitespace),
            '\u{3002}' | '\u{ff01}' | '\u{ff1f}' => true,
            _ => false,
        };
        if terminator && end >= half && end <= window_end {
            sentence = Some(end);
        }
    }
    sentence.or(space).filter(|&c| c > 0).unwrap_or(window_end)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chars(c: &Chunk) -> usize {
        c.text.chars().count()
    }

    /// Every chunk is a contiguous slice of the source, within the size
    /// target, with sequential ordinals.
    fn assert_well_formed(text: &str, chunks: &[Chunk]) {
        for (i, c) in chunks.iter().enumerate() {
            assert_eq!(c.ordinal, i);
            assert_eq!(&text[c.start..c.end], c.text, "chunk {i} is a slice");
            assert!(chars(c) <= TARGET_CHARS, "chunk {i} has {} chars", chars(c));
            assert!(!c.text.trim().is_empty(), "chunk {i} is blank");
            assert_eq!(c.tokens, chars(c).div_ceil(4));
        }
    }

    #[test]
    fn empty_and_blank_text_make_no_chunks() {
        assert!(chunk_text("").is_empty());
        assert!(chunk_text("  \n\n \t\n").is_empty());
    }

    #[test]
    fn short_text_is_one_chunk() {
        let c = chunk_text("Hello there.\nSecond line.");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "Hello there.\nSecond line.");
        assert_eq!(c[0].page, 1);
        assert_eq!(c[0].heading, None);
    }

    #[test]
    fn surrounding_blank_lines_are_not_in_the_chunk() {
        let c = chunk_text("\n\n  body text  \n\n");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].text, "body text");
    }

    #[test]
    fn a_heading_starts_a_new_chunk_and_is_recorded() {
        let text = "# Work\nI joined Acme in 2019.\n\n## Education\nBSc Physics.";
        let c = chunk_text(text);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].heading.as_deref(), Some("Work"));
        assert!(c[0].text.contains("Acme"));
        assert_eq!(c[1].heading.as_deref(), Some("Education"));
        assert!(c[1].text.contains("Physics"));
        assert_well_formed(text, &c);
    }

    #[test]
    fn text_before_the_first_heading_has_no_heading() {
        let c = chunk_text("preamble\n# Title\nbody");
        assert_eq!(c[0].heading, None);
        assert_eq!(c[1].heading.as_deref(), Some("Title"));
    }

    #[test]
    fn a_hash_without_a_space_is_not_a_heading() {
        let c = chunk_text("#hashtag and more\nsecond");
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].heading, None);
    }

    #[test]
    fn long_prose_splits_with_overlap_and_loses_nothing() {
        let sentence = "The quick brown fox jumps over the lazy dog near the river bank. ";
        let text = sentence.repeat(200); // about 13,000 chars, one line
        let c = chunk_text(&text);
        assert!(c.len() >= 6, "{} chunks", c.len());
        assert_well_formed(&text, &c);
        // Overlap: each chunk starts before the previous one ended.
        for w in c.windows(2) {
            assert!(w[1].start < w[0].end, "no overlap between chunks");
            assert!(w[1].start > w[0].start, "no progress");
            assert!(w[0].end - w[1].start <= OVERLAP_CHARS);
        }
        // Coverage: the first chunk starts at the text and the last reaches its end.
        assert_eq!(c[0].start, 0);
        assert_eq!(c.last().unwrap().end, text.trim_end().len());
        // A long line breaks at a sentence end, not mid-word.
        assert!(c[0].text.ends_with('.'));
    }

    #[test]
    fn many_paragraphs_break_between_paragraphs() {
        let para = "word ".repeat(100); // 500 chars
        let text = (0..10)
            .map(|i| format!("para{i} {para}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let c = chunk_text(&text);
        assert!(c.len() > 1);
        assert_well_formed(&text, &c);
        for ch in &c {
            assert!(ch.text.starts_with("para") || ch.text.contains("\n\npara"));
        }
    }

    #[test]
    fn a_single_giant_line_with_no_spaces_is_hard_split_without_panic() {
        let text = "x".repeat(10_000);
        let c = chunk_text(&text);
        assert_well_formed(&text, &c);
        assert!(c.len() >= 5);
        assert_eq!(c.last().unwrap().end, 10_000);
    }

    #[test]
    fn non_bmp_and_multibyte_text_never_splits_a_character() {
        let text = "\u{1F600}\u{1F4A1}".repeat(2_500); // 5,000 chars, 20,000 bytes
        let c = chunk_text(&text);
        assert_well_formed(&text, &c);
        let cjk = "漢字".repeat(3_000);
        assert_well_formed(&cjk, &chunk_text(&cjk));
        let accents = "caf\u{e9} na\u{ef}ve ".repeat(1_000);
        assert_well_formed(&accents, &chunk_text(&accents));
    }

    #[test]
    fn a_table_is_never_split_inside_a_row() {
        let row = "| Acme Corporation | Senior Engineer | 2019 to 2022 | Berlin |";
        let text = (0..100).map(|_| row).collect::<Vec<_>>().join("\n");
        let c = chunk_text(&text);
        assert!(c.len() > 1);
        assert_well_formed(&text, &c);
        for ch in &c {
            for line in ch.text.lines() {
                assert_eq!(line, row, "a row was cut: {line:?}");
            }
        }
    }

    #[test]
    fn form_feeds_are_page_breaks_and_set_the_page() {
        let text = "page one text\u{c}page two text\u{c}\u{c}page four text";
        let c = chunk_text(text);
        let pages: Vec<u32> = c.iter().map(|c| c.page).collect();
        assert_eq!(pages, vec![1, 2, 4]);
        assert_eq!(c[1].text, "page two text");
        assert_well_formed(text, &c);
    }

    #[test]
    fn heading_and_page_travel_together() {
        let c = chunk_text("# Intro\none\u{c}two");
        assert_eq!(c[1].page, 2);
        assert_eq!(c[1].heading.as_deref(), Some("Intro"));
    }

    #[test]
    fn a_two_megabyte_file_chunks_quickly_and_completely() {
        let text = "lorem ipsum dolor sit amet ".repeat(80_000); // about 2.1 MB
        let c = chunk_text(&text);
        assert_well_formed(&text, &c);
        assert_eq!(c.last().unwrap().end, text.trim_end().len());
    }

    #[test]
    fn two_megabytes_of_blank_lines_make_no_chunks() {
        assert!(chunk_text(&"\n".repeat(2_000_000)).is_empty());
    }

    #[test]
    fn a_very_long_heading_is_capped_in_the_record() {
        let text = format!("# {}\nbody", "h".repeat(5_000));
        let c = chunk_text(&text);
        assert!(c[0].heading.as_deref().unwrap().chars().count() <= 200);
    }
}
