//! The `"image_clipboard"` executor (#29): copies a captured region's RGBA
//! pixels to the clipboard as `CF_DIB`, the classic Windows raster clipboard
//! format every image-aware Windows app reads. Companion to
//! `executors::clipboard` (plain text), same shape -- an injectable
//! clipboard trait so tests never touch the real OS clipboard (rule 9), an
//! `Undo` that restores whatever was there before -- but over image bytes
//! and a different clipboard format, so this is its own module rather than
//! an extension of that one.
//!
//! The proposal value carries the pixels as base64 (`"rgba_base64"` plus
//! `"width"`/`"height"`), the same JSON currency every other executor in
//! this crate speaks (`Confirmed<serde_json::Value>` is the one signature
//! `Executor::execute` has). `base64` is already a crate dependency (used
//! throughout `provider/`), so this adds nothing to `Cargo.toml`.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use base64::Engine;

use crate::ui::confirm::Confirmed;

use super::{Effect, Executor, Undo};

/// Windows raster clipboard format id for a device-independent bitmap: a
/// `BITMAPINFOHEADER` immediately followed by pixel data, no file header.
/// `inputs::selection`'s `win32` module already imports the typed
/// `windows::Win32::System::Ole::CF_DIB` constant for the same value; kept
/// here as a plain `u32` (matching `ImageClipboardAccess`'s Win32-free
/// signature) rather than shared, the same "this task's scope is this file,
/// not a cross-module refactor" call that module's own `com`/`win32`
/// duplication already makes.
const CF_DIB: u32 = 8;

/// Clipboard access for the one format this executor cares about, abstracted
/// so tests never touch the real OS clipboard -- the same reasoning
/// `executors::clipboard::ClipboardAccess` gives for the text case.
pub trait ImageClipboardAccess: Send + Sync {
    /// The raw `CF_DIB` bytes currently on the clipboard, if any -- byte
    /// exact, used only to restore on undo.
    fn get_dib(&self) -> Option<Vec<u8>>;
    /// Replaces the ENTIRE clipboard with exactly this `CF_DIB` payload
    /// (`EmptyClipboard` + `SetClipboardData` semantics -- a full-clipboard
    /// replace, not a merge, the same tradeoff `inputs::selection`'s module
    /// doc documents for its own multi-format snapshot/restore).
    fn set_dib(&self, dib: &[u8]) -> Result<()>;
}

/// The real clipboard, via raw Win32 calls (`arboard`, already a crate
/// dependency, does not expose a byte-exact `CF_DIB` write on Windows --
/// this needs the literal format this executor's own tests golden-check).
pub(crate) struct Win32ImageClipboard;

impl ImageClipboardAccess for Win32ImageClipboard {
    fn get_dib(&self) -> Option<Vec<u8>> {
        win32::get_dib()
    }

    fn set_dib(&self, dib: &[u8]) -> Result<()> {
        win32::set_dib(dib)
    }
}

/// `Arc`-wrapped so `execute`'s undo closure (`'static`, runs after
/// `execute` returns) can hold its own handle to the same clipboard the
/// executor was constructed with -- same pattern as
/// `executors::clipboard::ClipboardExecutor`.
pub struct ImageClipboardExecutor<C: ImageClipboardAccess = Win32ImageClipboard> {
    clipboard: Arc<C>,
}

impl ImageClipboardExecutor<Win32ImageClipboard> {
    pub fn new() -> Self {
        Self {
            clipboard: Arc::new(Win32ImageClipboard),
        }
    }
}

impl Default for ImageClipboardExecutor<Win32ImageClipboard> {
    fn default() -> Self {
        Self::new()
    }
}

impl<C: ImageClipboardAccess> ImageClipboardExecutor<C> {
    /// Only ever called with a fake in this module's own tests in a
    /// non-test build (production always goes through `new()`/`default()`,
    /// resolved by name via `executors::registry`); same status
    /// `executors::clipboard::ClipboardExecutor::with_clipboard` has.
    #[allow(dead_code)]
    pub fn with_clipboard(clipboard: C) -> Self {
        Self {
            clipboard: Arc::new(clipboard),
        }
    }
}

impl<C: ImageClipboardAccess + 'static> Executor for ImageClipboardExecutor<C> {
    fn name(&self) -> &'static str {
        "image_clipboard"
    }

    /// `ReadOnly` is intentional, not an oversight: see `executors::Effect`'s
    /// doc comment ("Decided (issue #402)") for why a clipboard-writing
    /// executor still counts as read-only for the confirm fast path.
    fn effect(&self) -> Effect {
        Effect::ReadOnly
    }

    fn execute(&self, confirmed: Confirmed<serde_json::Value>) -> Result<Undo> {
        let value = confirmed.into_value();
        let width = value.get("width").and_then(|v| v.as_u64());
        let height = value.get("height").and_then(|v| v.as_u64());
        let rgba_b64 = value.get("rgba_base64").and_then(|v| v.as_str());
        let (Some(width), Some(height), Some(rgba_b64)) = (width, height, rgba_b64) else {
            bail!(
                "image_clipboard executor: proposal is missing \"rgba_base64\"/\"width\"/\"height\""
            );
        };
        let (width, height) = (width as u32, height as u32);

        let rgba = base64::engine::general_purpose::STANDARD
            .decode(rgba_b64)
            .context("image_clipboard executor: \"rgba_base64\" was not valid base64")?;
        anyhow::ensure!(
            rgba.len() == (width as usize) * (height as usize) * 4,
            "image_clipboard executor: pixel buffer length does not match width*height*4"
        );

        let dib = encode_dib(&rgba, width, height);

        // Best-effort: a clipboard that was empty, or held no CF_DIB entry,
        // restores to nothing rather than failing the whole action -- same
        // shape as `executors::clipboard`'s text case, including the same
        // residual gap (issue #410): "empty" and "had something this
        // executor cannot capture" are indistinguishable here.
        let previous = self.clipboard.get_dib();

        self.clipboard.set_dib(&dib)?;

        let clipboard = Arc::clone(&self.clipboard);
        Ok(Undo::recording(
            format!("copied a {width}x{height} region image to the clipboard"),
            move || {
                if let Some(previous) = previous {
                    clipboard.set_dib(&previous)?;
                }
                Ok(())
            },
        ))
    }
}

/// Encodes `rgba` (RGBA8, row-major top-down, `width`x`height` -- the same
/// layout `capture::RawShot`/`capture::crop_rgba` use) as a Windows `CF_DIB`
/// clipboard payload: a 40-byte `BITMAPINFOHEADER` immediately followed by
/// pixel data, no file header (`CF_DIB` never has a `BITMAPFILEHEADER` --
/// that only exists in a `.bmp` file). GDI's classic DIB pixel convention is
/// BOTTOM-UP (the image's last row is stored first) and BGRA (blue, green,
/// red, alpha) per pixel for 32bpp `BI_RGB` -- both are the format's own
/// requirement, not a style choice here. The header fields are written as
/// raw little-endian bytes rather than through a `windows`-crate
/// `BITMAPINFOHEADER` value, so this function's exact byte layout does not
/// depend on that struct's Rust-side repr -- see this module's
/// `encode_dib_matches_a_hand_computed_golden` test for the byte-for-byte
/// check this reasoning is standing on.
fn encode_dib(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    const HEADER_LEN: usize = 40;
    let pixel_len = (width as usize) * (height as usize) * 4;
    let mut out = Vec::with_capacity(HEADER_LEN + pixel_len);

    out.extend_from_slice(&(HEADER_LEN as u32).to_le_bytes()); // biSize
    out.extend_from_slice(&(width as i32).to_le_bytes()); // biWidth
    out.extend_from_slice(&(height as i32).to_le_bytes()); // biHeight (positive: bottom-up)
    out.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
    out.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
    out.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
    out.extend_from_slice(&(pixel_len as u32).to_le_bytes()); // biSizeImage
    out.extend_from_slice(&0i32.to_le_bytes()); // biXPelsPerMeter
    out.extend_from_slice(&0i32.to_le_bytes()); // biYPelsPerMeter
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
    out.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant

    for row in (0..height).rev() {
        let start = (row as usize) * (width as usize) * 4;
        for px in rgba[start..start + (width as usize) * 4].chunks_exact(4) {
            out.push(px[2]); // B
            out.push(px[1]); // G
            out.push(px[0]); // R
            out.push(px[3]); // A
        }
    }

    out
}

// ---------------------------------------------------------------------------
// Win32
// ---------------------------------------------------------------------------

mod win32 {
    use anyhow::Result;
    use std::time::Duration;
    use windows::Win32::Foundation::{GlobalFree, HANDLE, HGLOBAL};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardData, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };

    use super::CF_DIB;

    /// RAII pairing of `OpenClipboard`/`CloseClipboard`. Same bounded-retry
    /// shape as `inputs::selection::win32::OpenGuard` (duplicated rather
    /// than shared -- see this module's own reasoning in the parent file's
    /// doc comment): `OpenClipboard` can transiently fail while another
    /// process holds the clipboard open, and this only ever runs during an
    /// actively in-progress, user-triggered copy, never while idle
    /// (AGENTS.md rule 5).
    struct OpenGuard;

    impl OpenGuard {
        fn open() -> Result<Self> {
            const ATTEMPTS: u32 = 10;
            let mut last_err = None;
            for attempt in 0..ATTEMPTS {
                match unsafe { OpenClipboard(None) } {
                    Ok(()) => return Ok(Self),
                    Err(e) => {
                        last_err = Some(e);
                        if attempt + 1 < ATTEMPTS {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                    }
                }
            }
            Err(anyhow::anyhow!(
                "OpenClipboard failed after {ATTEMPTS} attempts: {}",
                last_err.expect("loop always sets last_err before exhausting ATTEMPTS")
            ))
        }
    }

    impl Drop for OpenGuard {
        fn drop(&mut self) {
            let _ = unsafe { CloseClipboard() };
        }
    }

    fn read_hglobal_bytes(handle: HANDLE) -> Option<Vec<u8>> {
        if handle.0.is_null() {
            return None;
        }
        let hglobal = HGLOBAL(handle.0);
        let size = unsafe { GlobalSize(hglobal) };
        if size == 0 {
            return None;
        }
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(ptr as *const u8, size) }.to_vec();
        let _ = unsafe { GlobalUnlock(hglobal) };
        Some(bytes)
    }

    fn alloc_hglobal_bytes(bytes: &[u8]) -> Result<HGLOBAL> {
        let hglobal = unsafe { GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)) }?;
        let ptr = unsafe { GlobalLock(hglobal) };
        if ptr.is_null() {
            let _ = unsafe { GlobalFree(Some(hglobal)) };
            anyhow::bail!("GlobalLock failed while preparing CF_DIB clipboard data");
        }
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len()) };
        let _ = unsafe { GlobalUnlock(hglobal) };
        Ok(hglobal)
    }

    pub(super) fn get_dib() -> Option<Vec<u8>> {
        let _guard = OpenGuard::open().ok()?;
        let handle = unsafe { GetClipboardData(CF_DIB) }.ok()?;
        read_hglobal_bytes(handle)
    }

    pub(super) fn set_dib(bytes: &[u8]) -> Result<()> {
        let _guard = OpenGuard::open()?;
        unsafe { EmptyClipboard() }?;
        let hglobal = alloc_hglobal_bytes(bytes)?;
        // Ownership transfers to the clipboard on success; must not be
        // freed here either way -- same reasoning
        // `inputs::selection::win32::Win32Clipboard::set_formats` documents.
        unsafe { SetClipboardData(CF_DIB, Some(HANDLE(hglobal.0))) }
            .map_err(|e| anyhow::anyhow!("SetClipboardData(CF_DIB) failed: {e}"))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    use crate::ui::confirm::Proposal;

    #[derive(Default)]
    struct FakeImageClipboard {
        dib: RefCell<Option<Vec<u8>>>,
    }

    impl FakeImageClipboard {
        fn seeded(initial: &[u8]) -> Self {
            Self {
                dib: RefCell::new(Some(initial.to_vec())),
            }
        }
    }

    // `RefCell` is not `Sync`; every test here is single-threaded and never
    // shares this fake across a real thread boundary -- same pattern
    // `executors::clipboard`'s own `FakeClipboard` test double uses.
    unsafe impl Sync for FakeImageClipboard {}

    impl ImageClipboardAccess for FakeImageClipboard {
        fn get_dib(&self) -> Option<Vec<u8>> {
            self.dib.borrow().clone()
        }

        fn set_dib(&self, dib: &[u8]) -> Result<()> {
            *self.dib.borrow_mut() = Some(dib.to_vec());
            Ok(())
        }
    }

    fn confirmed_with(
        value: serde_json::Value,
        executor: &ImageClipboardExecutor<FakeImageClipboard>,
    ) -> Confirmed<serde_json::Value> {
        crate::ui::confirm::auto_confirm_read_only(executor, Proposal::new(value)).unwrap()
    }

    fn proposal_for(rgba: &[u8], width: u32, height: u32) -> serde_json::Value {
        serde_json::json!({
            "rgba_base64": base64::engine::general_purpose::STANDARD.encode(rgba),
            "width": width,
            "height": height,
        })
    }

    // -- encode_dib: hand-computed golden ---------------------------------

    #[test]
    fn encode_dib_matches_a_hand_computed_golden() {
        // 1x2 image, top-down input: row0 = (10,20,30,40), row1 =
        // (50,60,70,80). CF_DIB is bottom-up, so row1 must be written
        // first, each pixel reordered from RGBA to BGRA.
        let rgba = [10u8, 20, 30, 40, 50, 60, 70, 80];
        let dib = encode_dib(&rgba, 1, 2);

        #[rustfmt::skip]
        let expected: Vec<u8> = vec![
            // BITMAPINFOHEADER (40 bytes, little-endian)
            40, 0, 0, 0,   // biSize
            1, 0, 0, 0,    // biWidth = 1
            2, 0, 0, 0,    // biHeight = 2 (positive: bottom-up)
            1, 0,          // biPlanes
            32, 0,         // biBitCount
            0, 0, 0, 0,    // biCompression = BI_RGB
            8, 0, 0, 0,    // biSizeImage = 1*2*4
            0, 0, 0, 0,    // biXPelsPerMeter
            0, 0, 0, 0,    // biYPelsPerMeter
            0, 0, 0, 0,    // biClrUsed
            0, 0, 0, 0,    // biClrImportant
            // pixel data, bottom-up: row1 then row0, each BGRA
            70, 60, 50, 80,
            30, 20, 10, 40,
        ];

        assert_eq!(dib, expected);
    }

    #[test]
    fn encode_dib_roundtrips_dimensions_via_header_fields() {
        let rgba = vec![0u8; 3 * 2 * 4];
        let dib = encode_dib(&rgba, 3, 2);
        assert_eq!(dib.len(), 40 + 3 * 2 * 4);
        let width = i32::from_le_bytes(dib[4..8].try_into().unwrap());
        let height = i32::from_le_bytes(dib[8..12].try_into().unwrap());
        assert_eq!(width, 3);
        assert_eq!(height, 2);
    }

    // -- executor behaviour -------------------------------------------------

    #[test]
    fn image_clipboard_executor_is_read_only() {
        assert_eq!(ImageClipboardExecutor::new().effect(), Effect::ReadOnly);
        assert_eq!(ImageClipboardExecutor::new().name(), "image_clipboard");
    }

    /// Issue #402: a clipboard-overwriting executor still auto-confirming
    /// is a deliberate decision (see `executors::Effect`'s doc comment), not
    /// a gap. Regression guard for that decision, same shape as
    /// `executors::clipboard`'s sibling test.
    #[test]
    fn image_clipboard_executor_auto_confirms_by_design_per_issue_402() {
        let executor = ImageClipboardExecutor::with_clipboard(FakeImageClipboard::default());
        let proposal = Proposal::new(proposal_for(&[0, 0, 0, 0], 1, 1));
        crate::ui::confirm::auto_confirm_read_only(&executor, proposal).expect(
            "image_clipboard executor must auto-confirm: issue #402 decided this is intended",
        );
    }

    #[test]
    fn execute_writes_a_cf_dib_payload_matching_encode_dib() {
        let executor = ImageClipboardExecutor::with_clipboard(FakeImageClipboard::default());
        let rgba = vec![1u8, 2, 3, 255, 4, 5, 6, 255]; // 2x1
        let confirmed = confirmed_with(proposal_for(&rgba, 2, 1), &executor);

        executor.execute(confirmed).unwrap();

        let on_clipboard = executor.clipboard.get_dib().unwrap();
        assert_eq!(on_clipboard, encode_dib(&rgba, 2, 1));
    }

    #[test]
    fn execute_errors_on_a_mismatched_pixel_buffer_length() {
        let executor = ImageClipboardExecutor::with_clipboard(FakeImageClipboard::default());
        // Claims 4x4 but only supplies a 2x2 buffer.
        let rgba = vec![0u8; 2 * 2 * 4];
        let confirmed = confirmed_with(proposal_for(&rgba, 4, 4), &executor);

        let err = executor
            .execute(confirmed)
            .err()
            .expect("a mismatched buffer length must be an error");
        assert!(err.to_string().contains("width*height*4"));
        assert!(!err.to_string().contains('\u{2014}'), "no em dashes: {err}");
    }

    #[test]
    fn execute_errors_when_fields_are_missing() {
        let executor = ImageClipboardExecutor::with_clipboard(FakeImageClipboard::default());
        let confirmed = confirmed_with(serde_json::json!({"width": 1, "height": 1}), &executor);

        let err = executor
            .execute(confirmed)
            .err()
            .expect("a missing rgba_base64 field must be an error");
        assert!(err.to_string().contains("rgba_base64"));
    }

    #[test]
    fn undo_restores_the_previous_cf_dib_payload() {
        let previous = encode_dib(&[9u8, 9, 9, 255], 1, 1);
        let executor =
            ImageClipboardExecutor::with_clipboard(FakeImageClipboard::seeded(&previous));
        let rgba = vec![1u8, 2, 3, 255];
        let confirmed = confirmed_with(proposal_for(&rgba, 1, 1), &executor);

        let undo = executor.execute(confirmed).unwrap();
        assert_ne!(executor.clipboard.get_dib().unwrap(), previous);

        undo.undo().unwrap();
        assert_eq!(
            executor.clipboard.get_dib().unwrap(),
            previous,
            "undo must restore exactly what was on the clipboard before execute ran"
        );
    }

    #[test]
    fn undo_is_a_no_op_when_the_clipboard_had_no_prior_cf_dib() {
        let executor = ImageClipboardExecutor::with_clipboard(FakeImageClipboard::default());
        let rgba = vec![1u8, 2, 3, 255];
        let confirmed = confirmed_with(proposal_for(&rgba, 1, 1), &executor);

        let undo = executor.execute(confirmed).unwrap();
        undo.undo()
            .expect("undo must not fail just because there was nothing to restore");
        assert!(executor.clipboard.get_dib().is_some());
    }
}
