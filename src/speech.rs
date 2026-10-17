//! Issue #77: read a card aloud with the system voice (Windows SAPI).
//!
//! Two halves. [`spoken_text`] is pure: it decides what the voice is handed
//! (headline, then detail, tidied for the ear) and is unit-tested. [`Speaker`]
//! wraps `ISpVoice` and is Win32: it is checked by hand (the PR names the
//! observable).
//!
//! Threading: the card lives on the main STA thread, and so does the
//! `Speaker`. `Speak` is always called with `SPF_ASYNC`, so the UI thread
//! never blocks while the voice talks, and nothing here polls. When the
//! voice finishes it posts a window message (`SetNotifyWindowMessage`), so
//! the card can flip its label back without a timer (AGENTS.md rule 5).
//!
//! The first `Speaker::new` of a run runs on the UI thread, on the first Read
//! aloud click. MEASURED 2026-10-17 (debug test binary, fresh process each
//! run, five runs, `speaker_new_cold_time`): `CoInitializeEx` plus
//! `CoCreateInstance(SpVoice)` took 8.6 to 14.5 ms cold and about 0.4 ms
//! warm. That is well under the keyboard hook's budget, so the voice stays on
//! the UI thread (SAPI objects are apartment-bound, and moving it would mean
//! a second thread owning every call). THEORY (unverified): a machine with
//! a cold disk cache or a third-party voice could take longer.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Media::Speech::{
    ISpVoice, SpVoice, SPEI_END_INPUT_STREAM, SPEI_RESERVED1, SPEI_RESERVED2, SPEVENT, SPF_ASYNC,
    SPF_IS_NOT_XML, SPF_PURGEBEFORESPEAK,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};

/// The text the voice reads for a card: the headline, then the detail.
///
/// Whitespace runs (including newlines) collapse to one space so the voice
/// does not pause on layout. Markdown emphasis marks (`*`, backtick) are
/// dropped because the voice would read them out. A headline with no closing
/// punctuation gets a full stop, so it does not run into the detail. Empty
/// input gives an empty string, which the caller treats as "nothing to read".
pub(crate) fn spoken_text(headline: &str, detail: &str) -> String {
    let head = tidy(headline);
    let body = tidy(detail);
    match (head.is_empty(), body.is_empty()) {
        (true, true) => String::new(),
        (false, true) => head,
        (true, false) => body,
        (false, false) if head.ends_with(['.', '!', '?', ':', ';']) => format!("{head} {body}"),
        (false, false) => format!("{head}. {body}"),
    }
}

fn tidy(s: &str) -> String {
    let stripped: String = s.chars().filter(|c| !matches!(c, '*' | '`')).collect();
    stripped.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `SPFEI(e)` from sapi.h: the event bit, plus the two reserved bits the
/// event mask must carry to be valid.
fn event_mask(event: i32) -> u64 {
    (1u64 << event) | (1u64 << SPEI_RESERVED1.0) | (1u64 << SPEI_RESERVED2.0)
}

/// An `ISpVoice` bound to the thread that created it (the main STA thread).
pub(crate) struct Speaker {
    voice: ISpVoice,
}

impl Speaker {
    /// Creates the voice. `notify_hwnd` receives `notify_msg` (wparam and
    /// lparam 0) when an utterance finishes or is stopped.
    pub(crate) fn new(notify_hwnd: HWND, notify_msg: u32) -> anyhow::Result<Self> {
        unsafe {
            // Nothing else initializes COM on the main thread, so this call
            // is what makes it a single-threaded apartment. S_FALSE (already
            // an STA) is fine. RPC_E_CHANGED_MODE (the thread is already
            // another model) is an error: the voice's notify window needs an
            // STA, so it fails to the "Can't read aloud" card instead of
            // proceeding. Never uninitialized: the thread keeps its apartment
            // for the life of the process.
            CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                .ok()
                .map_err(|e| anyhow::anyhow!("Wingman: could not start the system voice: {e}"))?;
            let voice: ISpVoice = CoCreateInstance(&SpVoice, None, CLSCTX_ALL)
                .map_err(|e| anyhow::anyhow!("Wingman: no system voice is available: {e}"))?;
            voice.SetNotifyWindowMessage(notify_hwnd, notify_msg, WPARAM(0), LPARAM(0))?;
            // Notify and queue the end-of-stream event: the card drains the
            // queue in `ended_streams` each time the notification arrives, so
            // it never grows.
            let end = event_mask(SPEI_END_INPUT_STREAM.0);
            voice.SetInterest(end, end)?;
            Ok(Self { voice })
        }
    }

    /// Starts reading `text`, replacing anything already being read. Returns
    /// at once (`SPF_ASYNC`). `SPF_IS_NOT_XML` so a `<` in an answer is
    /// spoken, not parsed as SAPI markup.
    /// Returns the utterance's stream number, which [`Speaker::ended_streams`]
    /// later reports when it finishes.
    pub(crate) fn speak(&self, text: &str) -> anyhow::Result<u32> {
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        let flags = (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0 | SPF_IS_NOT_XML.0) as u32;
        let mut stream = 0u32;
        unsafe {
            self.voice
                .Speak(PCWSTR(wide.as_ptr()), flags, Some(&mut stream))
                .map_err(|e| anyhow::anyhow!("Wingman: the system voice failed: {e}"))?;
        }
        Ok(stream)
    }

    /// Drains the voice's queued events and returns the stream number of
    /// every utterance that has ended. Matching on the stream number (rather
    /// than asking "is it speaking now?") keeps the end of a replaced
    /// utterance from being mistaken for the end of the new one.
    pub(crate) fn ended_streams(&self) -> Vec<u32> {
        let mut ended = Vec::new();
        loop {
            let mut events = [SPEVENT::default(); 8];
            let mut fetched = 0u32;
            let ok = unsafe { self.voice.GetEvents(8, events.as_mut_ptr(), &mut fetched) }.is_ok();
            if !ok || fetched == 0 {
                break;
            }
            for ev in &events[..fetched as usize] {
                // eEventId is the low 16 bits of the bitfield.
                if (ev._bitfield & 0xFFFF) == SPEI_END_INPUT_STREAM.0 {
                    ended.push(ev.ulStreamNum);
                }
            }
        }
        ended
    }

    /// True while the voice is mid-utterance.
    #[cfg(test)]
    pub(crate) fn is_speaking(&self) -> bool {
        use windows::Win32::Media::Speech::{SPRS_IS_SPEAKING, SPVOICESTATUS};
        let mut status = SPVOICESTATUS::default();
        let mut bookmark = windows::core::PWSTR::null();
        unsafe {
            self.voice.GetStatus(&mut status, &mut bookmark).is_ok()
                && status.dwRunningState & (SPRS_IS_SPEAKING.0 as u32) != 0
        }
    }

    /// Cuts off whatever is being read. Harmless when nothing is.
    pub(crate) fn stop(&self) {
        let empty = [0u16];
        let flags = (SPF_ASYNC.0 | SPF_PURGEBEFORESPEAK.0 | SPF_IS_NOT_XML.0) as u32;
        unsafe {
            let _ = self.voice.Speak(PCWSTR(empty.as_ptr()), flags, None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn headline_then_detail() {
        assert_eq!(
            spoken_text("Correct.", "The force is 5 N."),
            "Correct. The force is 5 N."
        );
    }

    #[test]
    fn headline_without_punctuation_gets_a_full_stop() {
        assert_eq!(
            spoken_text("Answer is 5 N", "Because F = ma"),
            "Answer is 5 N. Because F = ma"
        );
    }

    #[test]
    fn headline_ending_in_question_or_colon_is_not_doubled() {
        assert_eq!(spoken_text("Is it right?", "Yes."), "Is it right? Yes.");
        assert_eq!(spoken_text("Steps:", "one two"), "Steps: one two");
    }

    #[test]
    fn empty_detail_reads_only_the_headline() {
        assert_eq!(spoken_text("Done", ""), "Done");
        assert_eq!(spoken_text("Done", "  \n "), "Done");
    }

    #[test]
    fn empty_headline_reads_only_the_detail() {
        assert_eq!(spoken_text("", "Just detail."), "Just detail.");
    }

    #[test]
    fn nothing_to_read_is_empty() {
        assert_eq!(spoken_text("", ""), "");
        assert_eq!(spoken_text(" \t", "\n"), "");
    }

    #[test]
    fn whitespace_and_newlines_collapse() {
        assert_eq!(
            spoken_text("Line  one\r\nline two.", "a\n\n  b\tc"),
            "Line one line two. a b c"
        );
    }

    #[test]
    fn markdown_emphasis_marks_are_not_read_out() {
        assert_eq!(spoken_text("**Bold.**", "use `ls` now"), "Bold. use ls now");
    }

    #[test]
    fn angle_brackets_survive_for_is_not_xml() {
        assert_eq!(spoken_text("x < 5.", "and y > 2"), "x < 5. and y > 2");
    }

    /// Manual measurement for issue #1202. Run with
    /// `cargo test speaker_new_cold_time -- --ignored --nocapture`.
    #[test]
    #[ignore]
    fn speaker_new_cold_time() {
        let t = std::time::Instant::now();
        let s = Speaker::new(HWND::default(), 0x8001);
        eprintln!("Speaker::new cold: {:?} res={:?}", t.elapsed(), s.as_ref().err());
        let t = std::time::Instant::now();
        let s2 = Speaker::new(HWND::default(), 0x8001);
        eprintln!("Speaker::new warm: {:?} ok={}", t.elapsed(), s2.is_ok());
    }
}
