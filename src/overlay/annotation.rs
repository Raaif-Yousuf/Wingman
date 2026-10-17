//! What the overlay draws (spec "The anchor model"): one [`Annotation`] per
//! thing marked, and the words of the summary card that repeats every mark as
//! text (the accessibility floor: the overlay is never the only carrier).
//!
//! Pure. Label text comes from a detector or a model, so it is cleaned here
//! before anything draws it: no control characters, no em dashes (AGENTS.md
//! rule 11), clamped to [`MAX_LABEL_CHARS`] on a character boundary.

use crate::capture::ScreenRect;

/// The longest label, in characters, ellipsis included.
pub const MAX_LABEL_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnotationKind {
    /// A plain note beside the text it is about.
    Note,
}

/// One mark: the screen rectangles of the text it is about (one per line
/// fragment), what kind of mark, and its already-cleaned label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Annotation {
    pub rects: Vec<ScreenRect>,
    pub kind: AnnotationKind,
    pub label: String,
}

impl Annotation {
    /// A note, with `label` cleaned. `None` if nothing is left of the label
    /// or there is no rect to anchor to: such a mark could not be drawn.
    pub fn note(rects: Vec<ScreenRect>, label: &str) -> Option<Self> {
        let label = clean_label(label);
        let drawable = rects.iter().any(|r| r.left < r.right && r.top < r.bottom);
        if label.is_empty() || !drawable {
            return None;
        }
        Some(Self {
            rects,
            kind: AnnotationKind::Note,
            label,
        })
    }
}

/// Control characters dropped, em and en dashes turned into a comma, runs of
/// whitespace collapsed, then clamped to [`MAX_LABEL_CHARS`] characters.
pub fn clean_label(raw: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in raw.chars() {
        if c == '\u{2014}' || c == '\u{2013}' {
            // "a - b" and "a-b" both read as "a, b".
            if !out.is_empty() {
                out.push(',');
            }
            pending_space = true;
            continue;
        }
        if c.is_whitespace() || c.is_control() {
            pending_space = true;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(c);
    }
    clamp_chars(&out, MAX_LABEL_CHARS)
}

/// `s` cut to at most `max` characters, the last being an ellipsis when it
/// was cut.
fn clamp_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut cut: String = s.chars().take(max.saturating_sub(1)).collect();
    while cut.ends_with(' ') {
        cut.pop();
    }
    cut.push('\u{2026}');
    cut
}

/// The summary card's headline and detail once the marks are on screen.
/// `placed` of the annotations found a place; the list names all of them, so
/// a mark that could not be drawn is still on the card.
pub fn summary(annotations: &[Annotation], placed: usize) -> (String, String) {
    let unplaced = annotations.len().saturating_sub(placed);
    let mut headline = format!(
        "{placed} thing{} marked",
        if placed == 1 { "" } else { "s" }
    );
    if unplaced > 0 {
        headline.push_str(&format!(", {unplaced} could not be placed"));
    }
    (headline, listing(annotations, "Esc to close."))
}

/// The card's words when the marks could not be put on screen (nothing
/// anchored, or a guard could not run): the same list, so nothing is lost.
pub fn fallback(annotations: &[Annotation]) -> (String, String) {
    (
        "Could not place the marks on screen, here they are".to_string(),
        listing(annotations, ""),
    )
}

/// Every mark as a plain list, for a card that has to say what was marked once
/// the marks themselves are gone.
pub fn findings(annotations: &[Annotation]) -> String {
    listing(annotations, "")
}

/// The card's words when there was nothing to mark at all.
pub fn nothing_to_mark() -> (String, String) {
    ("Nothing to mark on screen".to_string(), String::new())
}

fn listing(annotations: &[Annotation], intro: &str) -> String {
    let mut out = intro.to_string();
    for a in annotations {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str("- ");
        out.push_str(&a.label);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    #[test]
    fn a_label_loses_control_characters_and_extra_space() {
        assert_eq!(
            clean_label("  Thu\t17:00\n your   time \u{0007}"),
            "Thu 17:00 your time"
        );
        assert_eq!(clean_label("\u{0000}\u{001b}"), "");
        assert_eq!(clean_label("   "), "");
    }

    #[test]
    fn a_label_never_carries_an_em_or_en_dash() {
        let cleaned = clean_label("Sign error \u{2014} line 3 \u{2013} check");
        assert!(!cleaned.contains('\u{2014}') && !cleaned.contains('\u{2013}'));
        assert_eq!(cleaned, "Sign error, line 3, check");
        assert_eq!(clean_label("\u{2014}start"), "start");
        assert_eq!(clean_label("a\u{2014}b"), "a, b");
    }

    #[test]
    fn a_long_label_is_cut_on_a_character_boundary() {
        let long = "\u{00e9}".repeat(200);
        let cleaned = clean_label(&long);
        assert_eq!(cleaned.chars().count(), MAX_LABEL_CHARS);
        assert!(cleaned.ends_with('\u{2026}'));
        // Exactly at the cap is left alone.
        let exact = "x".repeat(MAX_LABEL_CHARS);
        assert_eq!(clean_label(&exact), exact);
    }

    #[test]
    fn a_note_needs_a_label_and_something_to_point_at() {
        let one = r(10, 10, 50, 20);
        assert!(Annotation::note(vec![one], "Hello").is_some());
        assert!(Annotation::note(vec![one], "  \n").is_none());
        assert!(Annotation::note(vec![], "Hello").is_none());
        assert!(Annotation::note(vec![r(5, 5, 5, 9)], "Hello").is_none());
        let a = Annotation::note(vec![one], "  Hi\u{2014}there ").unwrap();
        assert_eq!(a.label, "Hi, there");
        assert_eq!(a.kind, AnnotationKind::Note);
    }

    #[test]
    fn the_summary_counts_and_lists_every_mark() {
        let a = Annotation::note(vec![r(0, 0, 9, 9)], "First").unwrap();
        let b = Annotation::note(vec![r(0, 20, 9, 29)], "Second").unwrap();
        let (h, d) = summary(std::slice::from_ref(&a), 1);
        assert_eq!(h, "1 thing marked");
        assert!(d.starts_with("Esc to close."));
        assert!(d.contains("- First"));
        let (h, d) = summary(&[a.clone(), b.clone()], 2);
        assert_eq!(h, "2 things marked");
        assert!(d.contains("- First") && d.contains("- Second"));
        // A mark with no place still reads on the card.
        let (h, d) = summary(&[a, b], 1);
        assert_eq!(h, "1 thing marked, 1 could not be placed");
        assert!(d.contains("- Second"));
    }

    #[test]
    fn findings_is_the_bare_list() {
        let a = Annotation::note(vec![r(0, 0, 9, 9)], "One").unwrap();
        let b = Annotation::note(vec![r(0, 20, 9, 29)], "Two").unwrap();
        assert_eq!(findings(&[a, b]), "- One\n- Two");
        assert_eq!(findings(&[]), "");
    }

    #[test]
    fn the_fallback_keeps_the_list_and_says_why_in_the_headline() {
        let a = Annotation::note(vec![r(0, 0, 9, 9)], "Only mark").unwrap();
        let (h, d) = fallback(&[a]);
        assert_eq!(h, "Could not place the marks on screen, here they are");
        assert_eq!(d, "- Only mark");
    }

    #[test]
    fn card_words_have_no_em_dash() {
        let a = Annotation::note(vec![r(0, 0, 9, 9)], "x").unwrap();
        let texts = [
            summary(std::slice::from_ref(&a), 0).0,
            summary(std::slice::from_ref(&a), 1).1,
            fallback(&[a]).0,
            nothing_to_mark().0,
        ];
        for text in texts {
            assert!(!text.contains('\u{2014}'), "{text:?}");
        }
    }
}
