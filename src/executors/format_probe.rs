//! Shared Win32 clipboard-format probe (#410): tells "the clipboard is
//! empty" apart from "the clipboard holds something in a format this
//! crate's clipboard executors cannot read back", so `clipboard`'s and
//! `image_clipboard`'s `Undo` can say so honestly instead of silently
//! treating a failed read as "nothing to restore" (the gap #402 left
//! standing on purpose, per `executors::mod`'s `Effect` doc comment).
//!
//! `CountClipboardFormats` returns the number of formats currently on the
//! clipboard, 0 meaning genuinely empty, per its own documentation --
//! unlike `arboard::Error::ContentNotAvailable`, it does not conflate
//! "empty" with "present but not in the format I asked for".

use windows::Win32::System::DataExchange::{CloseClipboard, CountClipboardFormats, OpenClipboard};

/// True if the OS clipboard currently holds data in *any* format.
///
/// Best-effort on the open itself: if the clipboard cannot even be opened
/// (another process holds it, same transient condition
/// `image_clipboard::win32::OpenGuard` retries for), this errs toward
/// reporting content as present rather than claiming the certainty that it
/// was empty -- a false "something's there" only costs an executor an
/// honest-but-unnecessary "could not be preserved" note; a false "empty"
/// would silently drop real data, which is the exact bug #410 exists to
/// close.
pub(crate) fn any_clipboard_format_present() -> bool {
    unsafe {
        if OpenClipboard(None).is_err() {
            return true;
        }
        let count = CountClipboardFormats();
        let _ = CloseClipboard();
        count > 0
    }
}
