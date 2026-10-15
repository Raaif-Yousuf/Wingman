//! The text of the page in the foreground browser or reader, read through UI
//! Automation (#104). The `page_text` input kind of `actions.toml`: a long
//! article is sent as text, not as a screenshot, so the summary covers what
//! is below the fold and costs far fewer tokens.
//!
//! Two layers, like `inputs::selection`:
//!
//! - **Pure** (top of this file): choosing the page among the document
//!   elements a window exposes ([`pick_page_text`]), the length cap and its
//!   truncation flag, and the retry around a browser that has not built its
//!   accessibility tree yet ([`read_with_retry`]). Unit-tested with fakes.
//! - **Win32** ([`com`]): find the window's `Document` elements, skip any
//!   that is a password field, and read each one's `TextPattern`
//!   `DocumentRange` with a length limit.
//!
//! Call [`read_page_text`] from a worker thread, never the keyboard
//! hook's thread (a hung provider can block for the UIA timeout).
//!
//! `THEORY (unverified)`: password detection is per `Document` element only.
//! A password `<input>` inside a web page is not a document of its own, so
//! its characters (bullets in Chrome and Edge) could appear in the page
//! text; no real secret should, since browsers expose the masked value.
//! Manual check: open a login page with a typed password, run Summarize this
//! page, and confirm the request has no password text.
//!
//! Wired observable: a Chrome article's Summarize this page request contains
//! a "Page text:" section and no image.
//!
//! `THEORY (unverified)`: Chrome and Edge build their web accessibility tree
//! when the first UIA client asks, so the first read right after the browser
//! started can find an empty document. [`read_with_retry`] tries a second
//! time after a short pause. The real browser read is a manual check.
//!
//! Nothing here sends anything anywhere: the caller (the generic action
//! path) decides what goes to the model, and marks the text as data.

use std::time::Duration;

use super::selection::truncate_bounded;

/// Longest page text kept, in characters (about 15,000 tokens). A long
/// article is 5,000 to 10,000 words, so this covers one whole; anything
/// longer is cut and says so.
pub const DEFAULT_MAX_CHARS: usize = 60_000;

/// Most `Document` elements read per window. A browser has one per tab view
/// at most; the cap keeps a strange tree from costing many `GetText` calls.
pub const MAX_CANDIDATES: usize = 8;

/// How many times the read is tried when no document had any text.
pub const ATTEMPTS: usize = 2;

/// The pause before a retry.
pub const RETRY_PAUSE: Duration = Duration::from_millis(400);

/// One `Document` element's text as read, before choosing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocCandidate {
    /// The text read from its `TextPattern` (at most `max_chars + 1`
    /// characters, so a longer document is noticed). Empty when the element
    /// has no `TextPattern`, is a password field or could not be read.
    pub text: String,
    /// The length of the text as UIA returned it, in UTF-16 units (what
    /// `GetText`'s limit counts). Reaching the limit means the document may
    /// have been cut, which a scalar count of `text` cannot show for text
    /// with characters outside the BMP (an emoji is one char, two units).
    pub utf16_len: usize,
    /// `IsPassword` was true (or could not be read: fail closed). Its text
    /// is never used, and the reader never asks for it.
    pub is_password: bool,
}

/// The page text and whether it was cut to the limit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageText {
    pub text: String,
    pub truncated: bool,
}

/// Chooses the page among a window's document elements: the one with the
/// most text, ignoring password fields and blank text. The page is by far the
/// largest document in a browser or reader window (the others are an address
/// bar's popup or an embedded frame), and it is what the user is looking at.
/// The first of equals wins, so the order the tree gave decides ties.
pub fn pick_page_text(candidates: Vec<DocCandidate>, max_chars: usize) -> Option<PageText> {
    let best = candidates
        .into_iter()
        .filter(|c| !c.is_password)
        .map(|c| (c.text.trim().chars().count(), c))
        .filter(|(n, _)| *n > 0)
        // `max_by_key` returns the LAST of equal maxima, so compare reversed
        // positions: keep the first by only replacing on a strictly larger.
        .fold(
            None,
            |best: Option<(usize, DocCandidate)>, next| match best {
                Some(b) if b.0 >= next.0 => Some(b),
                _ => Some(next),
            },
        )?;
    let hit_limit = best.1.utf16_len > max_chars;
    let (mut text, cut_by_chars) = truncate_bounded(best.1.text.trim(), max_chars);
    let truncated = hit_limit || cut_by_chars;
    if truncated && text.ends_with('\u{FFFD}') {
        // `GetText` can cut between the two halves of a surrogate pair; the
        // lossy BSTR conversion leaves a replacement character there.
        text.pop();
    }
    Some(PageText { text, truncated })
}

/// Runs `read` up to `attempts` times, pausing between tries, until
/// [`pick_page_text`] finds a page. Only a read that worked but found no
/// text is retried (a browser still building its tree). A read that fails
/// ends at once with `None`: a hung provider already cost the UIA timeout,
/// and a second try would cost it again. No text means the action falls back
/// to the screen.
pub fn read_with_retry(
    mut read: impl FnMut() -> anyhow::Result<Vec<DocCandidate>>,
    max_chars: usize,
    attempts: usize,
    mut pause: impl FnMut(),
) -> Option<PageText> {
    for attempt in 0..attempts {
        if attempt > 0 {
            pause();
        }
        let candidates = read().ok()?;
        if let Some(page) = pick_page_text(candidates, max_chars) {
            return Some(page);
        }
    }
    None
}

/// Reads the page text of `hwnd` (a browser or reader window). `None` when
/// the window exposes no readable document: the caller then falls back to
/// whatever else the action lists. Blocks; worker thread only.
pub fn read_page_text(
    hwnd: windows::Win32::Foundation::HWND,
    max_chars: usize,
) -> Option<PageText> {
    read_with_retry(
        || com::read_documents(hwnd, max_chars),
        max_chars,
        ATTEMPTS,
        || std::thread::sleep(RETRY_PAUSE),
    )
}

mod com {
    use super::{DocCandidate, MAX_CANDIDATES};
    use windows::core::Interface;
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{
        IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern, TreeScope_Descendants,
        UIA_ControlTypePropertyId, UIA_DocumentControlTypeId, UIA_TextPatternId,
    };

    /// Same RAII pairing as `inputs::selection::com::ComApartment`,
    /// duplicated because that type is private to its file.
    struct ComApartment;

    impl ComApartment {
        fn enter() -> anyhow::Result<Self> {
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok()?;
            Ok(Self)
        }
    }

    impl Drop for ComApartment {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }

    /// UIA returns a successful HRESULT with a null pointer for a pattern a
    /// control lacks; calling through it is undefined behaviour.
    fn is_null<I: Interface>(i: &I) -> bool {
        i.as_raw().is_null()
    }

    /// Every `Document` element under `hwnd` (at most [`MAX_CANDIDATES`]),
    /// each with its text. The condition is a control-type filter in the one
    /// `FindAll` call, so a browser's thousands of other elements are never
    /// walked.
    pub(super) fn read_documents(
        hwnd: HWND,
        max_chars: usize,
    ) -> anyhow::Result<Vec<DocCandidate>> {
        let _apartment = ComApartment::enter()?;
        let automation: IUIAutomation =
            unsafe { crate::inputs::uia_automation::create_automation() }?;
        let root = crate::inputs::uia_automation::describe_timeout(
            unsafe { automation.ElementFromHandle(hwnd) },
            "ElementFromHandle",
        )?;
        let condition = unsafe {
            automation.CreatePropertyCondition(
                UIA_ControlTypePropertyId,
                &VARIANT::from(UIA_DocumentControlTypeId.0),
            )
        }?;
        let found = crate::inputs::uia_automation::describe_timeout(
            unsafe { root.FindAll(TreeScope_Descendants, &condition) },
            "FindAll",
        )?;
        let total = unsafe { found.Length() }?.max(0) as usize;
        let mut out = Vec::new();
        for i in 0..total.min(MAX_CANDIDATES) {
            let element = unsafe { found.GetElement(i as i32) }?;
            out.push(read_document(&element, max_chars));
        }
        Ok(out)
    }

    /// One document's text. A password element is reported as such without
    /// asking for its text; a failed read of the flag fails closed, like
    /// `inputs::uia` and `inputs::selection`.
    fn read_document(element: &IUIAutomationElement, max_chars: usize) -> DocCandidate {
        let is_password = unsafe { element.CurrentIsPassword() }
            .map(|b| b.as_bool())
            .unwrap_or(true);
        if is_password {
            return DocCandidate {
                text: String::new(),
                utf16_len: 0,
                is_password: true,
            };
        }
        let (text, utf16_len) = (|| -> anyhow::Result<(String, usize)> {
            let unknown = unsafe { element.GetCurrentPattern(UIA_TextPatternId) }?;
            if is_null(&unknown) {
                return Ok((String::new(), 0));
            }
            let pattern: IUIAutomationTextPattern = unknown.cast()?;
            let range = unsafe { pattern.DocumentRange() }?;
            // One more than the cap, so a longer document reads as truncated.
            let limit = i32::try_from(max_chars.saturating_add(1)).unwrap_or(i32::MAX);
            let bstr = unsafe { range.GetText(limit) }?;
            Ok((bstr.to_string(), bstr.len()))
        })()
        .unwrap_or_default();
        DocCandidate {
            text,
            utf16_len,
            is_password: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn doc(text: &str) -> DocCandidate {
        DocCandidate {
            text: text.to_string(),
            utf16_len: text.encode_utf16().count(),
            is_password: false,
        }
    }

    #[test]
    fn the_largest_document_is_the_page() {
        let p = pick_page_text(
            vec![doc("menu"), doc("the whole article body"), doc("ad")],
            100,
        )
        .unwrap();
        assert_eq!(p.text, "the whole article body");
        assert!(!p.truncated);
    }

    #[test]
    fn the_first_of_equal_documents_wins() {
        let p = pick_page_text(vec![doc("aaaa"), doc("bbbb")], 100).unwrap();
        assert_eq!(p.text, "aaaa");
    }

    #[test]
    fn a_password_document_is_never_used_even_if_largest() {
        let secret = DocCandidate {
            text: "hunter2 hunter2 hunter2".into(),
            utf16_len: 23,
            is_password: true,
        };
        let p = pick_page_text(vec![doc("page"), secret.clone()], 100).unwrap();
        assert_eq!(p.text, "page");
        assert_eq!(pick_page_text(vec![secret], 100), None);
    }

    #[test]
    fn nothing_or_blank_text_is_no_page() {
        assert_eq!(pick_page_text(vec![], 100), None);
        assert_eq!(pick_page_text(vec![doc(""), doc(" \r\n\t ")], 100), None);
    }

    #[test]
    fn text_over_the_limit_is_cut_and_flagged() {
        let p = pick_page_text(vec![doc("abcdefghij")], 4).unwrap();
        assert_eq!(p.text, "abcd");
        assert!(p.truncated);
        // Exactly at the limit is not truncated.
        let p = pick_page_text(vec![doc("abcd")], 4).unwrap();
        assert_eq!(p.text, "abcd");
        assert!(!p.truncated);
    }

    #[test]
    fn the_limit_counts_characters_not_bytes() {
        let p = pick_page_text(vec![doc("\u{e9}\u{e9}\u{e9}\u{e9}\u{e9}")], 3).unwrap();
        assert_eq!(p.text, "\u{e9}\u{e9}\u{e9}");
        assert!(p.truncated);
    }

    #[test]
    fn an_empty_first_read_is_retried_once_after_a_pause() {
        let reads = Cell::new(0);
        let pauses = Cell::new(0);
        let page = read_with_retry(
            || {
                reads.set(reads.get() + 1);
                Ok(if reads.get() == 1 {
                    vec![doc("")]
                } else {
                    vec![doc("now it is built")]
                })
            },
            100,
            2,
            || pauses.set(pauses.get() + 1),
        );
        assert_eq!(page.unwrap().text, "now it is built");
        assert_eq!((reads.get(), pauses.get()), (2, 1));
    }

    #[test]
    fn a_page_on_the_first_read_is_not_retried() {
        let reads = Cell::new(0);
        let page = read_with_retry(
            || {
                reads.set(reads.get() + 1);
                Ok(vec![doc("page")])
            },
            100,
            2,
            || panic!("no pause expected"),
        );
        assert!(page.is_some());
        assert_eq!(reads.get(), 1);
    }

    #[test]
    fn a_failed_read_is_not_retried() {
        let reads = Cell::new(0);
        let pauses = Cell::new(0);
        let page = read_with_retry(
            || {
                reads.set(reads.get() + 1);
                Err(anyhow::anyhow!("UI Automation timed out"))
            },
            100,
            2,
            || pauses.set(pauses.get() + 1),
        );
        assert_eq!(page, None);
        assert_eq!((reads.get(), pauses.get()), (1, 0));
    }

    #[test]
    fn no_documents_at_all_is_retried_then_gives_up() {
        let reads = Cell::new(0);
        let page = read_with_retry(
            || {
                reads.set(reads.get() + 1);
                Ok(vec![])
            },
            100,
            2,
            || {},
        );
        assert_eq!(page, None);
        assert_eq!(reads.get(), 2);
    }

    #[test]
    fn emoji_heavy_text_cut_by_the_utf16_limit_is_flagged() {
        // 40,000 astral characters are 80,000 UTF-16 units. UIA is asked for
        // max + 1 units, so what comes back is 30,000 whole characters and a
        // lone half of the next pair (a replacement character after the
        // lossy conversion): fewer than max characters, yet cut.
        let max = 60_000;
        let mut text = "\u{1F600}".repeat(30_000);
        text.push('\u{FFFD}');
        let cand = DocCandidate {
            utf16_len: max + 1,
            text,
            is_password: false,
        };
        let p = pick_page_text(vec![cand], max).unwrap();
        assert!(p.truncated);
        assert_eq!(p.text.chars().count(), 30_000);
        assert!(!p.text.ends_with('\u{FFFD}'));
    }

    #[test]
    fn a_whole_document_just_under_the_limit_is_not_flagged() {
        let p = pick_page_text(vec![doc("\u{1F600}\u{1F600}")], 4).unwrap();
        assert!(!p.truncated);
        assert_eq!(p.text.chars().count(), 2);
    }
}
