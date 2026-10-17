//! Built-in detectors: a name that turns the OCR text of a capture into
//! annotations with no model and no network (spec "Where the overlay hangs
//! off the action model"). Slice 2 has one, a debug check that marks a word;
//! the units and date detectors arrive with their own slices.
//!
//! Pure, and run on a worker thread: the OCR lines and the [`Space`] describe
//! the same image, and every quote goes through [`locate`], so what is drawn
//! is exactly what the matcher can find again.

use crate::ocr::OcrLine;

use super::anchor::{locate, AnchorRef, Located, Space};
use super::annotation::Annotation;

/// The debug detector: marks the longest word on screen. It exists so the
/// window, the hooks and the matcher have a real first caller to be checked
/// against; it is only listed in debug builds.
pub const MARK_WORD: &str = "mark_word";

/// The shortest word the debug detector will mark.
const MIN_WORD_CHARS: usize = 4;

/// Runs the detector named `name` over `lines`. An unknown name marks
/// nothing, which the caller reports as "nothing to mark" (never a panic).
pub fn run(name: &str, lines: &[OcrLine], space: &Space) -> Vec<Annotation> {
    match name {
        MARK_WORD => mark_longest_word(lines, space),
        _ => Vec::new(),
    }
}

/// Whether `name` is a detector [`run`] knows.
pub fn is_known(name: &str) -> bool {
    name == MARK_WORD
}

/// The longest word of letters or digits (first in reading order on a tie),
/// quoted back through the matcher like any model quote would be.
fn mark_longest_word(lines: &[OcrLine], space: &Space) -> Vec<Annotation> {
    let mut best: Option<&str> = None;
    for word in lines.iter().flat_map(|l| l.words.iter()) {
        let text = word.text.trim_matches(|c: char| !c.is_alphanumeric());
        let n = text.chars().count();
        if n < MIN_WORD_CHARS || !text.chars().all(char::is_alphanumeric) {
            continue;
        }
        match best {
            Some(b) if b.chars().count() >= n => {}
            _ => best = Some(text),
        }
    }
    let Some(word) = best else {
        return Vec::new();
    };
    let anchor = AnchorRef {
        text: word.to_string(),
        occurrence: 1,
    };
    match locate(lines, &anchor, space) {
        Located::Found(rects) => Annotation::note(rects, &format!("Longest word: {word}"))
            .into_iter()
            .collect(),
        Located::NotFound | Located::Ambiguous => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::ScreenRect;
    use crate::ocr::OcrWord;

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

    fn line(text: &str, x: f32, y: f32) -> OcrLine {
        let mut words = Vec::new();
        let mut cx = x;
        for w in text.split(' ') {
            let width = w.chars().count() as f32 * 8.0;
            words.push(OcrWord {
                text: w.to_string(),
                x: cx,
                y,
                width,
                height: 10.0,
            });
            cx += width + 8.0;
        }
        OcrLine {
            text: text.to_string(),
            x,
            y,
            width: cx - 8.0 - x,
            height: 10.0,
            words,
        }
    }

    #[test]
    fn it_marks_the_longest_word_beside_its_own_box() {
        let lines = [
            line("a tiny line", 10.0, 10.0),
            line("extraordinary day", 10.0, 40.0),
        ];
        let marks = run(MARK_WORD, &lines, &space());
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].label, "Longest word: extraordinary");
        assert_eq!(
            marks[0].rects,
            vec![ScreenRect {
                left: 10,
                top: 40,
                right: 10 + 13 * 8,
                bottom: 50
            }]
        );
    }

    #[test]
    fn the_first_of_two_equally_long_words_wins() {
        let lines = [line("alpha", 10.0, 10.0), line("omega", 10.0, 40.0)];
        let marks = run(MARK_WORD, &lines, &space());
        assert_eq!(marks[0].label, "Longest word: alpha");
        assert_eq!(marks[0].rects[0].top, 10);
    }

    #[test]
    fn punctuation_is_not_part_of_the_word_and_symbols_are_skipped() {
        let lines = [line("(wonderful), ====-==== ok", 0.0, 0.0)];
        let marks = run(MARK_WORD, &lines, &space());
        assert_eq!(marks[0].label, "Longest word: wonderful");
    }

    #[test]
    fn nothing_to_mark_is_an_empty_list_not_a_panic() {
        assert!(run(MARK_WORD, &[], &space()).is_empty());
        let short = [line("a to of", 0.0, 0.0)];
        assert!(run(MARK_WORD, &short, &space()).is_empty());
        // A line with no word boxes at all.
        let bare = OcrLine {
            text: "wonderful".to_string(),
            x: 0.0,
            y: 0.0,
            width: 50.0,
            height: 10.0,
            words: Vec::new(),
        };
        assert!(run(MARK_WORD, &[bare], &space()).is_empty());
    }

    #[test]
    fn a_multibyte_word_is_quoted_on_character_boundaries() {
        let lines = [line("na\u{00ef}vet\u{00e9} caf\u{00e9}", 0.0, 0.0)];
        let marks = run(MARK_WORD, &lines, &space());
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].label, "Longest word: na\u{00ef}vet\u{00e9}");
    }

    #[test]
    fn an_unknown_detector_marks_nothing() {
        let lines = [line("wonderful", 0.0, 0.0)];
        assert!(run("units", &lines, &space()).is_empty());
        assert!(!is_known("units"));
        assert!(is_known(MARK_WORD));
    }
}
