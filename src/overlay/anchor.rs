//! Finding quoted text on screen (spec "The anchor model"). The model never
//! gives coordinates: it quotes text, and [`locate`] finds that quote in the
//! OCR word boxes of the capture and returns screen rectangles to draw at.
//!
//! Pure: a function over `&[OcrLine]`, a string and a [`Space`]. Nothing here
//! indexes or slices text it did not just measure, because the OCR text, the
//! quote and the rects all came from outside (release builds abort on panic).
//!
//! Decisions where the spec leaves a corner open:
//! - A quote with exactly one exact match ignores `occurrence`; with two or
//!   more, an `occurrence` past the end is `NotFound` (never "the last one").
//! - Punctuation stuck to either end of a word is ignored on both sides, so
//!   the quote `recieve` finds the OCR word `recieve.`.
//! - A word hyphenated at a line break is matched joined (`recei-` + `ve`
//!   finds `receive`), but only if the plain words matched nothing, so a real
//!   hyphen is never eaten.
//! - Fuzzy matching: a quote with a digit is exact-only, a quote under 9
//!   characters gets no edits (only the `rn`/`m`, `1`/`l`, `0`/`o` folds),
//!   and from 9 characters one edit per 8, rounded up.
//! - A word whose box is not finite or has no area is dropped from the
//!   stream. A line with no word boxes at all matches as its whole rect.

use crate::capture::ScreenRect;
use crate::ocr::OcrLine;

/// Shortest and longest quote, in characters.
const MIN_QUOTE_CHARS: usize = 2;
const MAX_QUOTE_CHARS: usize = 400;
/// A quote shorter than this is never matched fuzzily.
const MIN_FUZZY_CHARS: usize = 4;
/// A quote shorter than this gets no edits, only the OCR confusion folds.
const MIN_EDIT_CHARS: usize = 9;
/// One edit allowed per this many characters of quote (rounded up).
const FUZZY_CHARS_PER_EDIT: usize = 8;

/// What the model (or a detector) says to point at. Never coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorRef {
    pub text: String,
    /// 1-based. 0 is read as 1.
    pub occurrence: u32,
}

/// The outcome of looking for one quote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Located {
    /// One rect per line fragment, in reading order.
    Found(Vec<ScreenRect>),
    /// No match, or nothing drawable: drop the annotation.
    NotFound,
    /// Two or more equally near fuzzy matches: drop the annotation.
    Ambiguous,
}

/// Where the OCR image sits on the screen: the screen position of pixel
/// (0, 0), how many screen pixels one image pixel covers, and the image size
/// (so the result can be clipped to the monitor that was captured). The same
/// numbers as `capture::ScreenMap` plus the size of the image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Space {
    pub origin_x: f64,
    pub origin_y: f64,
    pub scale_x: f64,
    pub scale_y: f64,
    pub width: u32,
    pub height: u32,
}

impl Space {
    /// The space of the image OCR actually ran on: `width` x `height` are that
    /// image's own size (after any `fit_for_model` shrink, which is why the
    /// map's scale can be above 1), and `map` says where its pixel (0, 0) sits
    /// on the screen and how many screen pixels one image pixel covers. The
    /// OCR word boxes are in that image's pixels, so this and the boxes must
    /// come from the same image. `None` for a map that cannot be used.
    pub fn for_image(width: u32, height: u32, map: &crate::capture::ScreenMap) -> Option<Self> {
        let space = Self {
            origin_x: map.origin_x,
            origin_y: map.origin_y,
            scale_x: map.scale_x,
            scale_y: map.scale_y,
            width,
            height,
        };
        space_is_usable(&space).then_some(space)
    }

    /// The part of the screen the image covers: the captured monitor, which
    /// labels are kept inside. Rounded to whole pixels; saturating, since the
    /// numbers came from outside.
    pub fn monitor_rect(&self) -> ScreenRect {
        let edge = |origin: f64, scale: f64, n: u32| {
            (origin + scale * f64::from(n)).round().clamp(-1e9, 1e9) as i32
        };
        ScreenRect {
            left: self.origin_x.round().clamp(-1e9, 1e9) as i32,
            top: self.origin_y.round().clamp(-1e9, 1e9) as i32,
            right: edge(self.origin_x, self.scale_x, self.width),
            bottom: edge(self.origin_y, self.scale_y, self.height),
        }
    }
}

/// One OCR word that can be drawn at: its box in image pixels and its line.
struct WordBox {
    line: usize,
    rect: (f32, f32, f32, f32),
}

/// One whitespace-separated piece of normalised text and the boxes it came
/// from (two for a word joined across a line break).
#[derive(Clone)]
struct Tok {
    /// Normalised, punctuation still attached.
    raw: String,
    /// `raw` without punctuation at either end: what is compared.
    key: String,
    boxes: Vec<usize>,
}

/// Finds `anchor` in `lines` and maps it to screen rectangles.
pub fn locate(lines: &[OcrLine], anchor: &AnchorRef, space: &Space) -> Located {
    let quote = anchor.text.trim();
    let chars = quote.chars().count();
    if !(MIN_QUOTE_CHARS..=MAX_QUOTE_CHARS).contains(&chars)
        || crate::redact::contains_marker(quote)
        || !space_is_usable(space)
    {
        return Located::NotFound;
    }
    let qkeys: Vec<String> = normalise(quote).split_whitespace().map(key_of).collect();
    if qkeys.is_empty() {
        return Located::NotFound;
    }

    let (boxes, plain) = flatten(lines);
    let joined = join_line_breaks(&plain, &boxes);

    // Exact pass on the plain words, then on the joined ones.
    let mut stream = &plain;
    let mut starts = exact_starts(stream, &qkeys);
    if starts.is_empty() {
        if let Some(j) = &joined {
            stream = j;
            starts = exact_starts(stream, &qkeys);
        }
    }
    let wanted = anchor.occurrence.max(1) as usize;
    let start = match starts.len() {
        0 => {
            // Fuzzy only if the exact pass found nothing at all.
            return match fuzzy(&plain, joined.as_deref(), &qkeys) {
                Fuzzy::One(s, in_joined) => {
                    let st = match (in_joined, &joined) {
                        (true, Some(j)) => j,
                        _ => &plain,
                    };
                    to_screen(st, s, qkeys.len(), &boxes, space)
                }
                Fuzzy::Many => Located::Ambiguous,
                Fuzzy::None => Located::NotFound,
            };
        }
        1 => starts[0],
        _ => match starts.get(wanted - 1) {
            Some(s) => *s,
            None => return Located::NotFound,
        },
    };
    to_screen(stream, start, qkeys.len(), &boxes, space)
}

fn space_is_usable(s: &Space) -> bool {
    [s.origin_x, s.origin_y, s.scale_x, s.scale_y]
        .iter()
        .all(|v| v.is_finite())
        && s.scale_x > 0.0
        && s.scale_y > 0.0
        && s.width > 0
        && s.height > 0
}

// ---- flattening and normalising ----

/// Lowercases, maps typographic quotes and dashes to ASCII, drops zero-width
/// characters and soft hyphens. Whitespace is left for the caller to split.
fn normalise(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '\u{200b}' | '\u{200c}' | '\u{200d}' | '\u{2060}' | '\u{feff}' | '\u{ad}' => {}
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => out.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => out.push('"'),
            '\u{2010}'..='\u{2015}' | '\u{2212}' => out.push('-'),
            c => out.extend(c.to_lowercase()),
        }
    }
    out
}

/// `raw` without non-alphanumeric characters at either end. A token that is
/// all punctuation keeps itself, so a quote of `-` still means something.
fn key_of(raw: &str) -> String {
    let t = raw.trim_matches(|c: char| !c.is_alphanumeric());
    if t.is_empty() { raw } else { t }.to_string()
}

/// The words of `lines` in reading order as tokens, and the boxes they point
/// at. Words are trusted over `OcrLine::text`; a line with no word boxes
/// falls back to its text and its own rect.
fn flatten(lines: &[OcrLine]) -> (Vec<WordBox>, Vec<Tok>) {
    let mut boxes = Vec::new();
    let mut toks = Vec::new();
    for (li, line) in lines.iter().enumerate() {
        let mut push = |text: &str, rect: (f32, f32, f32, f32)| {
            if !usable(rect) {
                return;
            }
            let idx = boxes.len();
            boxes.push(WordBox { line: li, rect });
            for raw in normalise(text).split_whitespace() {
                toks.push(Tok {
                    raw: raw.to_string(),
                    key: key_of(raw),
                    boxes: vec![idx],
                });
            }
        };
        if line.words.is_empty() {
            push(
                &line.text,
                (line.x, line.y, line.x + line.width, line.y + line.height),
            );
        } else {
            for w in &line.words {
                push(&w.text, (w.x, w.y, w.x + w.width, w.y + w.height));
            }
        }
    }
    (boxes, toks)
}

fn usable(r: (f32, f32, f32, f32)) -> bool {
    [r.0, r.1, r.2, r.3].iter().all(|v| v.is_finite()) && r.2 > r.0 && r.3 > r.1
}

/// The same stream with each word hyphenated at a line break joined to the
/// first word of the next line, or `None` if nothing was joined.
fn join_line_breaks(toks: &[Tok], boxes: &[WordBox]) -> Option<Vec<Tok>> {
    let line_of = |t: &Tok| t.boxes.first().and_then(|b| boxes.get(*b)).map(|b| b.line);
    let mut out: Vec<Tok> = Vec::with_capacity(toks.len());
    let mut joined_any = false;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if let Some(next) = toks.get(i + 1) {
            let stem = t.raw.strip_suffix('-');
            let breaks = line_of(t) != line_of(next);
            let letter_before =
                stem.is_some_and(|s| s.chars().next_back().is_some_and(char::is_alphabetic));
            let letter_after = next.raw.chars().next().is_some_and(char::is_alphabetic);
            if let (Some(stem), true, true, true) = (stem, breaks, letter_before, letter_after) {
                let raw = format!("{stem}{}", next.raw);
                let mut b = t.boxes.clone();
                b.extend(&next.boxes);
                out.push(Tok {
                    key: key_of(&raw),
                    raw,
                    boxes: b,
                });
                joined_any = true;
                i += 2;
                continue;
            }
        }
        out.push(t.clone());
        i += 1;
    }
    joined_any.then_some(out)
}

// ---- exact pass ----

/// Where `qkeys` occurs in `stream`, as token indices, without overlaps.
fn exact_starts(stream: &[Tok], qkeys: &[String]) -> Vec<usize> {
    let n = qkeys.len();
    let mut out = Vec::new();
    let mut i = 0;
    while i + n <= stream.len() {
        if stream[i..i + n].iter().zip(qkeys).all(|(t, q)| t.key == *q) {
            out.push(i);
            i += n;
        } else {
            i += 1;
        }
    }
    out
}

// ---- fuzzy pass ----

enum Fuzzy {
    None,
    /// The window's first token, and whether it is in the joined stream.
    One(usize, bool),
    Many,
}

/// The only window whose text is within the edit budget of the quote. OCR
/// confusions (`rn` for `m`, `1` for `l`, `0` for `o`) are folded away first
/// so they cost nothing.
fn fuzzy(plain: &[Tok], joined: Option<&[Tok]>, qkeys: &[String]) -> Fuzzy {
    let q = qkeys.join(" ");
    let qlen = q.chars().count();
    // A number is exact-only: a misread digit is a different number, and
    // pointing at the wrong one is worse than not pointing.
    if qlen < MIN_FUZZY_CHARS || q.chars().any(|c| c.is_ascii_digit()) {
        return Fuzzy::None;
    }
    // Short quotes get no edits (only the confusion folds): "Send" is not
    // "Sent". From 9 characters, one edit per 8, rounded up.
    let budget = if qlen < MIN_EDIT_CHARS {
        0
    } else {
        qlen.div_ceil(FUZZY_CHARS_PER_EDIT)
    };
    let qf: Vec<char> = fold(&q).chars().collect();
    match fuzzy_in(plain, qkeys.len(), &qf, budget) {
        Fuzzy::None => match joined {
            Some(j) => match fuzzy_in(j, qkeys.len(), &qf, budget) {
                Fuzzy::One(s, _) => Fuzzy::One(s, true),
                other => other,
            },
            None => Fuzzy::None,
        },
        other => other,
    }
}

fn fuzzy_in(stream: &[Tok], n: usize, qf: &[char], budget: usize) -> Fuzzy {
    let folded: Vec<String> = stream.iter().map(|t| fold(&t.key)).collect();
    let mut hit = None;
    let mut i = 0;
    while i + n <= stream.len() {
        let window: Vec<char> = folded[i..i + n].join(" ").chars().collect();
        if within(qf, &window, budget) {
            if hit.is_some() {
                return Fuzzy::Many;
            }
            hit = Some(i);
        }
        i += 1;
    }
    match hit {
        Some(s) => Fuzzy::One(s, false),
        None => Fuzzy::None,
    }
}

fn fold(s: &str) -> String {
    s.replace("rn", "m").replace('1', "l").replace('0', "o")
}

/// Whether the edit distance between `a` and `b` is at most `k`, computed in
/// a band of width `2k + 1` so a long quote against a long window stays cheap.
fn within(a: &[char], b: &[char], k: usize) -> bool {
    let (n, m) = (a.len(), b.len());
    if n.abs_diff(m) > k {
        return false;
    }
    let inf = k + 1;
    let mut prev: Vec<usize> = (0..=m).map(|j| if j <= k { j } else { inf }).collect();
    for i in 1..=n {
        let mut cur = vec![inf; m + 1];
        if i <= k {
            cur[0] = i;
        }
        let mut best = cur[0];
        for j in i.saturating_sub(k).max(1)..=(i + k).min(m) {
            let sub = prev[j - 1] + usize::from(a[i - 1] != b[j - 1]);
            let v = sub.min(prev[j] + 1).min(cur[j - 1] + 1).min(inf);
            cur[j] = v;
            best = best.min(v);
        }
        if best > k {
            return false;
        }
        prev = cur;
    }
    prev[m] <= k
}

// ---- to screen ----

/// The rects of the `n` tokens from `start`, one per line, on the screen.
fn to_screen(stream: &[Tok], start: usize, n: usize, boxes: &[WordBox], space: &Space) -> Located {
    let Some(window) = stream.get(start..start.saturating_add(n)) else {
        return Located::NotFound;
    };
    let mut per_line: Vec<(usize, (f32, f32, f32, f32))> = Vec::new();
    let mut seen: Vec<usize> = Vec::new();
    for idx in window.iter().flat_map(|t| t.boxes.iter().copied()) {
        if seen.contains(&idx) {
            continue;
        }
        seen.push(idx);
        let Some(b) = boxes.get(idx) else { continue };
        match per_line.iter_mut().find(|(l, _)| *l == b.line) {
            Some((_, u)) => {
                *u = (
                    u.0.min(b.rect.0),
                    u.1.min(b.rect.1),
                    u.2.max(b.rect.2),
                    u.3.max(b.rect.3),
                );
            }
            None => per_line.push((b.line, b.rect)),
        }
    }
    let rects: Vec<ScreenRect> = per_line
        .into_iter()
        .filter_map(|(_, r)| map_rect(r, space))
        .collect();
    if rects.is_empty() {
        Located::NotFound
    } else {
        Located::Found(rects)
    }
}

/// `screen = origin + image * scale`, rounded outward and clipped to the
/// captured monitor. `None` when nothing is left.
fn map_rect(r: (f32, f32, f32, f32), s: &Space) -> Option<ScreenRect> {
    let to_i32 = |v: f64| v.clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32;
    let left = to_i32((s.origin_x + f64::from(r.0) * s.scale_x).floor());
    let top = to_i32((s.origin_y + f64::from(r.1) * s.scale_y).floor());
    let right = to_i32((s.origin_x + f64::from(r.2) * s.scale_x).ceil());
    let bottom = to_i32((s.origin_y + f64::from(r.3) * s.scale_y).ceil());
    let mon_right = to_i32((s.origin_x + f64::from(s.width) * s.scale_x).ceil());
    let mon_bottom = to_i32((s.origin_y + f64::from(s.height) * s.scale_y).ceil());
    let out = ScreenRect {
        left: left.max(to_i32(s.origin_x.floor())),
        top: top.max(to_i32(s.origin_y.floor())),
        right: right.min(mon_right),
        bottom: bottom.min(mon_bottom),
    };
    (out.left < out.right && out.top < out.bottom).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ocr::OcrWord;

    const CH: f32 = 8.0; // pixels per character in the synthetic lines
    const H: f32 = 10.0;

    fn space() -> Space {
        Space {
            origin_x: 0.0,
            origin_y: 0.0,
            scale_x: 1.0,
            scale_y: 1.0,
            width: 1000,
            height: 1000,
        }
    }

    /// A line starting at (x, y) with one word per whitespace token, `CH`
    /// pixels per character and a one-character gap.
    fn line(text: &str, x: f32, y: f32) -> OcrLine {
        let mut words = Vec::new();
        let mut cx = x;
        for w in text.split(' ') {
            let width = w.chars().count() as f32 * CH;
            words.push(OcrWord {
                text: w.to_string(),
                x: cx,
                y,
                width,
                height: H,
            });
            cx += width + CH;
        }
        OcrLine {
            text: text.to_string(),
            x,
            y,
            width: cx - CH - x,
            height: H,
            words,
        }
    }

    fn quote(text: &str, occurrence: u32) -> AnchorRef {
        AnchorRef {
            text: text.to_string(),
            occurrence,
        }
    }

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn found(l: Located) -> Vec<ScreenRect> {
        match l {
            Located::Found(r) => r,
            other => panic!("expected Found, got {other:?}"),
        }
    }

    // ---- exact pass ----

    #[test]
    fn an_exact_word_maps_to_its_box() {
        let lines = [line("the quick fox", 10.0, 20.0)];
        let got = found(locate(&lines, &quote("quick", 1), &space()));
        // "the " is 4 chars: quick starts at 10 + 32, 5 chars wide.
        assert_eq!(got, vec![rect(42, 20, 82, 30)]);
    }

    #[test]
    fn a_phrase_in_one_line_is_one_rect() {
        let lines = [line("the quick brown fox", 0.0, 0.0)];
        let got = found(locate(&lines, &quote("quick brown", 1), &space()));
        assert_eq!(got, vec![rect(32, 0, 120, 10)]);
    }

    #[test]
    fn occurrence_picks_the_nth_match() {
        let lines = [line("cat dog", 0.0, 0.0), line("cat bird", 0.0, 40.0)];
        let first = found(locate(&lines, &quote("cat", 1), &space()));
        let second = found(locate(&lines, &quote("cat", 2), &space()));
        assert_eq!(first, vec![rect(0, 0, 24, 10)]);
        assert_eq!(second, vec![rect(0, 40, 24, 50)]);
    }

    #[test]
    fn an_occurrence_past_the_end_of_several_matches_is_not_found() {
        let lines = [line("cat dog cat", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("cat", 3), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn a_unique_quote_ignores_the_occurrence() {
        // Models count badly; one match on screen is the match.
        let lines = [line("only one cat here", 0.0, 0.0)];
        let got = found(locate(&lines, &quote("cat", 2), &space()));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn occurrence_zero_reads_as_one() {
        let lines = [line("cat dog cat", 0.0, 0.0)];
        let zero = locate(&lines, &quote("cat", 0), &space());
        let one = locate(&lines, &quote("cat", 1), &space());
        assert_eq!(zero, one);
        assert!(matches!(zero, Located::Found(_)));
    }

    #[test]
    fn matching_ignores_case_and_extra_whitespace() {
        let lines = [line("The Quick Fox", 0.0, 0.0)];
        let got = found(locate(&lines, &quote("  the   QUICK ", 1), &space()));
        assert_eq!(got.len(), 1);
    }

    #[test]
    fn punctuation_stuck_to_a_word_does_not_block_a_match() {
        let lines = [line("Did you recieve. Thanks", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("recieve", 1), &space()),
            Located::Found(_)
        ));
        let lines = [line("see (recieve) now", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("recieve", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn typographic_quotes_and_dashes_match_ascii() {
        let lines = [line(
            "it\u{2019}s a well\u{2013}known \u{201C}fact\u{201D}",
            0.0,
            0.0,
        )];
        assert!(matches!(
            locate(&lines, &quote("it's a well-known \"fact\"", 1), &space()),
            Located::Found(_)
        ));
        // And the other way round.
        let lines = [line("it's well-known", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("it\u{2019}s well\u{2014}known", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn zero_width_characters_and_soft_hyphens_are_dropped() {
        let lines = [line("re\u{ad}ceive the\u{200b}m", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("receive them", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn a_match_across_a_line_break_is_one_rect_per_line() {
        let lines = [
            line("the quick brown", 0.0, 0.0),
            line("fox jumps over", 0.0, 100.0),
        ];
        let got = found(locate(&lines, &quote("brown fox", 1), &space()));
        // Never one rect covering the gap between the lines.
        assert_eq!(got, vec![rect(80, 0, 120, 10), rect(0, 100, 24, 110)]);
    }

    #[test]
    fn a_word_hyphenated_at_a_line_break_joins() {
        let lines = [
            line("please recei-", 0.0, 0.0),
            line("ve the parcel", 0.0, 50.0),
        ];
        let got = found(locate(&lines, &quote("receive", 1), &space()));
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].top, 0);
        assert_eq!(got[1].top, 50);
        // A quote that spells the break out as two words still matches.
        assert!(matches!(
            locate(&lines, &quote("recei- ve", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn a_real_hyphen_is_not_mistaken_for_a_line_break_join() {
        let lines = [line("a well-known fact", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("well-known", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn a_line_with_no_word_boxes_matches_as_the_whole_line() {
        let mut l = line("the quick fox", 10.0, 20.0);
        l.words.clear();
        let got = found(locate(&[l], &quote("quick", 1), &space()));
        assert_eq!(got, vec![rect(10, 20, 114, 30)]);
    }

    #[test]
    fn words_that_disagree_with_the_line_text_are_trusted_over_it() {
        let mut l = line("the quick fox", 0.0, 0.0);
        l.text = "totally different text".to_string();
        assert!(matches!(
            locate(&[l], &quote("quick", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn a_word_with_a_non_finite_box_is_ignored() {
        let mut l = line("the quick fox", 0.0, 0.0);
        l.words[1].x = f32::NAN;
        assert_eq!(
            locate(&[l], &quote("quick", 1), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn nothing_on_screen_is_not_found() {
        assert_eq!(locate(&[], &quote("cat", 1), &space()), Located::NotFound);
        let lines = [line("hello world", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("absent", 1), &space()),
            Located::NotFound
        );
    }

    // ---- fuzzy pass ----

    #[test]
    fn a_unique_near_match_is_found() {
        // OCR read "brovvn": two edits in 19 characters, budget 3.
        let lines = [line("the quick brovvn fox", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("the quick brown fox", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn common_ocr_confusions_survive() {
        // rn read as m, l read as 1, O read as 0.
        let lines = [line("a modem fo11ow-up in 2O24", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("modern", 1), &space()),
            Located::Found(_)
        ));
        assert!(matches!(
            locate(&lines, &quote("follow-up", 1), &space()),
            Located::Found(_)
        ));
        // A quote with a digit in it is exact-only: a misread number is a
        // different number.
        assert_eq!(
            locate(&lines, &quote("2024", 1), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn numbers_and_short_words_are_never_taken_for_a_near_one() {
        for (screen, q) in [
            ("total 1900 due", "1000"),
            ("please Sent it", "Send"),
            ("pay invoice 19000 now", "invoice 10000"),
            ("order 0123456789 shipped", "order 0123456780"),
        ] {
            let lines = [line(screen, 0.0, 0.0)];
            assert_eq!(
                locate(&lines, &quote(q, 1), &space()),
                Located::NotFound,
                "{q}"
            );
        }
        // The exact number is still found.
        let lines = [line("total 1000 due", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("1000", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn two_equally_near_matches_are_ambiguous() {
        let lines = [
            line("pay the invoyce now", 0.0, 0.0),
            line("send the invoyse", 0.0, 50.0),
        ];
        assert_eq!(
            locate(&lines, &quote("the invoice", 1), &space()),
            Located::Ambiguous
        );
    }

    #[test]
    fn fuzzy_is_not_tried_when_the_exact_pass_found_something() {
        // Two exact matches; asking for a third must not fall through to the
        // near "gooseberrx". The quote is long enough to be fuzzy-eligible,
        // so this fails if the guard is removed (Ambiguous, or Found).
        let lines = [line("gooseberry gooseberry gooseberrx", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("gooseberry", 3), &space()),
            Located::NotFound
        );
        // Sanity: with no exact match that near word is found.
        let lines = [line("one gooseberrx only", 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote("gooseberry", 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn short_quotes_are_never_fuzzy() {
        let lines = [line("a cut above", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("cat", 1), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn a_far_quote_is_not_found() {
        let lines = [line("the quick brown fox", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("a completely other sentence", 1), &space()),
            Located::NotFound
        );
    }

    // ---- guard rails ----

    #[test]
    fn too_short_and_too_long_quotes_are_not_found() {
        let lines = [line("a b c", 0.0, 0.0)];
        assert_eq!(locate(&lines, &quote("a", 1), &space()), Located::NotFound);
        assert_eq!(locate(&lines, &quote("", 1), &space()), Located::NotFound);
        assert_eq!(locate(&lines, &quote("  ", 1), &space()), Located::NotFound);
        let long = ["word"; 100].join(" ");
        assert!(long.chars().count() > 400);
        let lines = [line(&long, 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote(&long, 1), &space()),
            Located::NotFound
        );
        // Exactly 400 characters is allowed.
        let ok: String = ["abcdefghi"; 40].join(" ")[..399].to_string() + "x";
        assert_eq!(ok.chars().count(), 400);
        let lines = [line(&ok, 0.0, 0.0)];
        assert!(matches!(
            locate(&lines, &quote(&ok, 1), &space()),
            Located::Found(_)
        ));
    }

    #[test]
    fn a_quote_with_a_redaction_marker_is_not_found() {
        let lines = [line("card [card number 1] on file", 0.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("card [card number 1]", 1), &space()),
            Located::NotFound
        );
        assert_eq!(
            locate(&lines, &quote("[email 2]", 1), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn multibyte_text_matches_without_splitting_a_character() {
        let lines = [line(
            "Stra\u{df}e \u{4eca}\u{65e5}\u{306e}\u{5929}\u{6c17} \u{130}stanbul caf\u{e9}",
            0.0,
            0.0,
        )];
        for q in [
            "Stra\u{df}e",
            "\u{4eca}\u{65e5}\u{306e}\u{5929}\u{6c17}",
            "\u{130}stanbul",
            "caf\u{e9}",
            "caf\u{e8}",
            "\u{1F600}\u{1F600}",
        ] {
            // No panic is the point; the first four are found.
            let _ = locate(&lines, &quote(q, 1), &space());
        }
        assert!(matches!(
            locate(
                &lines,
                &quote("\u{4eca}\u{65e5}\u{306e}\u{5929}\u{6c17}", 1),
                &space()
            ),
            Located::Found(_)
        ));
        assert!(matches!(
            locate(&lines, &quote("CAF\u{c9}", 1), &space()),
            Located::Found(_)
        ));
    }

    // ---- to screen ----

    #[test]
    fn a_scaled_map_multiplies_and_adds_the_origin() {
        let lines = [line("hello", 10.0, 20.0)];
        let s = Space {
            origin_x: 100.0,
            origin_y: 50.0,
            scale_x: 2.0,
            scale_y: 2.0,
            width: 1000,
            height: 1000,
        };
        let got = found(locate(&lines, &quote("hello", 1), &s));
        // image (10,20)-(50,30) -> (120,90)-(200,110)
        assert_eq!(got, vec![rect(120, 90, 200, 110)]);
    }

    #[test]
    fn a_monitor_left_of_the_primary_has_negative_coordinates() {
        let lines = [line("hello", 10.0, 20.0)];
        let s = Space {
            origin_x: -1920.0,
            origin_y: -100.0,
            ..space()
        };
        let got = found(locate(&lines, &quote("hello", 1), &s));
        assert_eq!(got, vec![rect(-1910, -80, -1870, -70)]);
    }

    #[test]
    fn rects_round_outward() {
        let mut l = line("hello", 0.0, 0.0);
        l.words[0].x = 10.4;
        l.words[0].y = 20.4;
        l.words[0].width = 10.2;
        l.words[0].height = 5.2;
        let got = found(locate(&[l], &quote("hello", 1), &space()));
        // 10.4..20.6 -> 10..21, 20.4..25.6 -> 20..26
        assert_eq!(got, vec![rect(10, 20, 21, 26)]);
    }

    #[test]
    fn a_rect_is_clipped_to_the_capture_monitor() {
        let lines = [line("hello", 990.0, 995.0)];
        let s = Space {
            origin_x: 500.0,
            origin_y: 0.0,
            width: 1000,
            height: 1000,
            ..space()
        };
        let got = found(locate(&lines, &quote("hello", 1), &s));
        // Monitor spans x 500..1500, y 0..1000.
        assert_eq!(got, vec![rect(1490, 995, 1500, 1000)]);
    }

    #[test]
    fn a_rect_that_clips_to_nothing_is_not_found() {
        let lines = [line("hello", 2000.0, 0.0)];
        assert_eq!(
            locate(&lines, &quote("hello", 1), &space()),
            Located::NotFound
        );
    }

    #[test]
    fn an_unusable_map_is_not_found() {
        let lines = [line("hello", 0.0, 0.0)];
        for bad in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let s = Space {
                scale_x: bad,
                ..space()
            };
            assert_eq!(locate(&lines, &quote("hello", 1), &s), Located::NotFound);
        }
        let s = Space {
            origin_x: f64::NAN,
            ..space()
        };
        assert_eq!(locate(&lines, &quote("hello", 1), &s), Located::NotFound);
    }

    /// A monitor 1600x900 at x=1920, shrunk to 800x450 before OCR.
    fn shrunk_map() -> crate::capture::ScreenMap {
        crate::capture::ScreenMap::for_capture(1920, 0, 1600, 900, 800, 450, None)
    }

    #[test]
    fn the_space_describes_the_image_ocr_ran_on_not_the_native_monitor() {
        let sp = Space::for_image(800, 450, &shrunk_map()).unwrap();
        assert_eq!((sp.width, sp.height), (800, 450));
        assert_eq!((sp.scale_x, sp.scale_y), (2.0, 2.0));
        assert_eq!(sp.monitor_rect(), rect(1920, 0, 3520, 900));
    }

    #[test]
    fn a_word_found_in_a_shrunk_image_lands_on_the_native_pixels() {
        // The word sits at image x 100..140, y 50..60, so on the screen at
        // 1920 + 200 .. 1920 + 280 and y 100..120.
        let lines = [line("hello", 100.0, 50.0)];
        let sp = Space::for_image(800, 450, &shrunk_map()).unwrap();
        let Located::Found(got) = locate(&lines, &quote("hello", 1), &sp) else {
            panic!("not found");
        };
        assert_eq!(got, vec![rect(2120, 100, 2200, 120)]);
        // Described as the native size instead, the same box would land at
        // half the distance: the mistake this constructor exists to prevent.
        let wrong = Space {
            scale_x: 1.0,
            scale_y: 1.0,
            ..sp
        };
        let Located::Found(bad) = locate(&lines, &quote("hello", 1), &wrong) else {
            panic!("not found");
        };
        assert_ne!(bad, got);
    }

    #[test]
    fn an_unusable_map_or_an_empty_image_gives_no_space() {
        assert!(Space::for_image(0, 450, &shrunk_map()).is_none());
        assert!(Space::for_image(800, 0, &shrunk_map()).is_none());
        let mut bad = shrunk_map();
        bad.scale_x = f64::NAN;
        assert!(Space::for_image(800, 450, &bad).is_none());
        let mut zero = shrunk_map();
        zero.scale_y = 0.0;
        assert!(Space::for_image(800, 450, &zero).is_none());
    }

    #[test]
    fn the_monitor_rect_of_an_unscaled_capture_is_its_own_size() {
        let map = crate::capture::ScreenMap::for_capture(-1280, -200, 1280, 1024, 1280, 1024, None);
        let sp = Space::for_image(1280, 1024, &map).unwrap();
        assert_eq!(sp.monitor_rect(), rect(-1280, -200, 0, 824));
    }
}
