//! Shared guard around `DrawTextW` for every paint-time text call site in
//! `src/ui` other than `palette.rs` (issue #221).
//!
//! MEASURED 2026-09-17 (`src/ui/palette.rs`'s `draw_text_line` and its
//! regression test `draw_text_line_tolerates_an_empty_string`, commit
//! `453fe0b`; independently re-confirmed in this worktree by this module's
//! own `#[ignore]`d `raw_drawtextw_crashes_on_empty_text`, run in isolation
//! via `cargo test ui::text::tests::raw_drawtextw_crashes_on_empty_text --
//! ignored --nocapture`): calling `DrawTextW`, through this crate's
//! `windows` 0.62 binding, with a zero-length `&mut [u16]` buffer --
//! exactly what `text.encode_utf16().collect::<Vec<u16>>()` produces for
//! `text == ""` -- reliably crashes the process with exit code
//! `0xC0000005` (`STATUS_ACCESS_VIOLATION`), no panic, no backtrace. THEORY
//! (unverified): the binding reads the buffer's length as "scan for a null
//! terminator" rather than "nothing to draw" when it is zero, walking off
//! the end of the `Vec`'s dangling-but-valid empty-allocation pointer.
//!
//! This module's own everyday regression test below
//! (`draw_text_line_tolerates_an_empty_string`) does NOT run that raw,
//! unguarded call: doing so as part of an ordinary `cargo test` invocation
//! would take the whole test binary down with it
//! (`STATUS_ACCESS_VIOLATION` is not a catchable panic), losing every other
//! test in the same run. The raw crash is reproduced exactly once, above,
//! in a dedicated `#[ignore]`d test meant to be run alone.
//!
//! `card.rs`, `region.rs`, `ocr.rs`'s and `provider/mod.rs`'s test-only GDI
//! helpers all route their paint-time `DrawTextW` calls through
//! [`draw_text_line`] below instead of each re-deriving the same guard.
//! `palette.rs` still carries its own copy of this exact function (it was
//! fixed first, on a separate branch, before this module existed) --
//! pointing it at this one instead is left as a follow-up (see the issue
//! filed for it) rather than done here, since `palette.rs` is out of this
//! task's scope.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{DrawTextW, DRAW_TEXT_FORMAT, HDC};

/// UTF-16, *not* null-terminated -- for `DrawTextW`, which takes an explicit
/// length rather than scanning for a terminator.
fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().collect()
}

/// Draws `text` in `rect` with `DrawTextW`, or does nothing at all when
/// `text` is empty -- see the module doc comment for why an empty string
/// must never reach the real `DrawTextW` call.
///
/// Takes `rect` by value (not `&mut`) because every call site in this crate
/// discards its own copy right after the call; `DrawTextW` may still adjust
/// its *local* copy in place (e.g. under `DT_CALCRECT`), which callers that
/// need the adjusted rectangle should get from a dedicated measuring helper
/// instead (see `card.rs`'s `measure_wrapped`/`measure_label`, which already
/// guard `text.is_empty()` themselves and return the font's line height in
/// that case).
pub fn draw_text_line(hdc: HDC, text: &str, mut rect: RECT, format: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let mut buf = utf16(text);
    unsafe {
        DrawTextW(hdc, &mut buf, &mut rect, format);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
        BITMAPINFOHEADER, DIB_RGB_COLORS, DT_LEFT, DT_SINGLELINE, DT_VCENTER,
    };

    /// A tiny 8x8 top-down 32bpp DIB section selected into its own memory
    /// DC -- a real GDI device context `DrawTextW` can paint into, entirely
    /// off-screen. No named kernel object, registry value or file path is
    /// created (rule 9 is moot here: there is nothing to give a test-only
    /// name to), and the DC/bitmap are always released before returning.
    fn with_memory_dc(f: impl FnOnce(HDC)) {
        unsafe {
            let hdc = CreateCompatibleDC(None);
            assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: 8,
                    biHeight: -8, // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: 0, // BI_RGB
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let hbitmap = CreateDIBSection(Some(hdc), &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
                .expect("CreateDIBSection failed");
            let old_bitmap = SelectObject(hdc, hbitmap.into());

            f(hdc);

            SelectObject(hdc, old_bitmap);
            let _ = DeleteObject(hbitmap.into());
            let _ = DeleteDC(hdc);
        }
    }

    fn rect() -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: 8,
            bottom: 8,
        }
    }

    /// Regression check for the module doc comment: an empty string must
    /// not reach the real `DrawTextW` call at all. Before a guard like this
    /// existed anywhere in `src/ui`, the equivalent call in `palette.rs`
    /// crashed with `STATUS_ACCESS_VIOLATION` (MEASURED 2026-09-17, commit
    /// `453fe0b` -- see the module doc comment for why that crash is
    /// reasoned about here rather than re-triggered).
    #[test]
    fn draw_text_line_tolerates_an_empty_string() {
        with_memory_dc(|hdc| {
            draw_text_line(hdc, "", rect(), DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        });
    }

    /// Neighbouring case: a non-empty string must still actually reach
    /// `DrawTextW` (i.e. the guard must not swallow real text). There is no
    /// return value to assert on `DrawTextW` success alone doesn't prove
    /// pixels changed, so this only proves the call completes without
    /// crashing for the ordinary, non-empty path -- the crash this module
    /// exists to prevent is specific to the empty-string case above.
    #[test]
    fn draw_text_line_draws_non_empty_text() {
        with_memory_dc(|hdc| {
            draw_text_line(hdc, "hi", rect(), DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        });
    }

    /// Manual, `#[ignore]`d reproduction of the raw crash this module's
    /// guard exists to prevent -- calls the real, unguarded `DrawTextW`
    /// (bypassing `draw_text_line` entirely) with a zero-length buffer.
    /// `STATUS_ACCESS_VIOLATION` is not a catchable Rust panic, so this
    /// takes the whole test process down with it; never run as part of an
    /// ordinary `cargo test` (hence `#[ignore]`), only in isolation:
    /// `cargo test ui::text::tests::raw_drawtextw_crashes_on_empty_text -- --ignored --nocapture`.
    /// MEASURED 2026-09-17 (this exact invocation, this worktree, this
    /// crate's `windows` 0.62 binding): the process terminated immediately
    /// with exit code `0xC0000005` (`STATUS_ACCESS_VIOLATION`), no panic
    /// message, no backtrace -- confirming, independently of the palette
    /// branch's own MEASURED case (commit `453fe0b`), that this crash is
    /// real and specific to the zero-length buffer, not to anything
    /// `palette.rs` does differently.
    #[test]
    #[ignore = "crashes the process on purpose (STATUS_ACCESS_VIOLATION); run in isolation only"]
    fn raw_drawtextw_crashes_on_empty_text() {
        with_memory_dc(|hdc| unsafe {
            let mut buf: Vec<u16> = "".encode_utf16().collect(); // zero-length
            let mut r = rect();
            DrawTextW(hdc, &mut buf, &mut r, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        });
    }
}
