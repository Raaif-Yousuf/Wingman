//! Windows OCR (`Windows.Media.Ocr`) over a captured frame.
//!
//! Issue #30. This module is deliberately self-contained: it takes raw
//! RGBA8 pixels (the shape `capture::RawShot` already holds) rather than
//! depending on `capture` or `provider`, so the orchestrator can move it
//! under `inputs/` at merge without untangling a dependency on either.
//! Must not call the network (see AGENTS.md's `inputs/ocr.rs` row).
//!
//! **MEASURED 2026-09-17** (this module's `ocr_live_recognizes_gdi_rendered_text`
//! test, run manually per its own doc comment): a plain `cargo test` binary
//! has no package identity (`has_package_identity()` false,
//! `GetCurrentPackageFullName` returns `APPMODEL_ERROR_NO_PACKAGE`), and
//! `OcrEngine::RecognizeAsync` over a synthetic GDI-rendered image still
//! succeeds from that binary: cold call 36 ms, warm call 22 ms, recognizer
//! language `en-US`, `OcrEngine::MaxImageDimension()` 10000. See AGENTS.md's
//! "Windows OCR under a sparse package" pitfall, updated in place with this
//! result, for what remains THEORY (unverified) -- whether an exe launched
//! via the `Run` key while the sparse package is installed elsewhere on the
//! machine differs from this measurement is not the same question and is
//! not answered by it, though no mechanism is known by which it would.

// Issue #30's scope is this module and the package-identity measurement,
// not wiring OCR into `App::ask` -- that lands in a follow-up issue (see
// this module's own tests for what filed it). Until a caller in `app.rs`
// exists, this binary crate's dead-code analysis would otherwise flag the
// whole public surface below as unused, the way `capture.rs`'s
// `pick_compression_level` is allowed for the same reason.
#![allow(dead_code)]

use anyhow::{anyhow, Context, Result};
use std::time::{Duration, Instant};
use windows::core::HSTRING;
use windows::Foundation::Rect;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::{OcrEngine, OcrLine as WinOcrLine};
use windows::Storage::Streams::DataWriter;
use windows::Win32::Storage::Packaging::Appx::GetCurrentPackageFullName;
use windows::Win32::System::WinRT::{RoInitialize, RoUninitialize, RO_INIT_MULTITHREADED};
use windows_future::AsyncStatus;

/// How long [`recognize`] waits for `RecognizeAsync` before cancelling it
/// and returning an error. Chosen generously relative to the MEASURED cold
/// latency in this module's doc comment -- see that note for the actual
/// numbers this was checked against.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// One recognized line of text, with its bounding rect (image pixel
/// coordinates, the union of its words' rects) so a caller can highlight or
/// hit-test it later. Lines are in the reading order `OcrResult::Lines`
/// returns them in.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrLine {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
    /// The line's words with their own rects, so a caller can paint over
    /// part of a line (screenshot redaction, #103) rather than all of it.
    pub words: Vec<OcrWord>,
}

/// One recognized word and its bounding rect, in the same pixel space as
/// [`OcrLine`].
#[derive(Debug, Clone, PartialEq)]
pub struct OcrWord {
    pub text: String,
    pub x: f32,
    pub y: f32,
    pub width: f32,
    pub height: f32,
}

/// The full result of one [`recognize`] call.
#[derive(Debug, Clone, PartialEq)]
pub struct OcrOutput {
    pub lines: Vec<OcrLine>,
    /// BCP-47 tag of the recognizer language actually used (e.g. `en-US`),
    /// for the card and for filing findings if recognition looks wrong.
    pub language_tag: String,
}

/// Whether this process currently has package identity:
/// `GetCurrentPackageFullName` returns something other than
/// `APPMODEL_ERROR_NO_PACKAGE`. Windows OCR is documented as requiring
/// package identity for some WinRT surfaces; see this module's doc comment
/// and AGENTS.md's pitfall for what was actually measured. Cheap and
/// side-effect-free -- safe to call from any thread, no apartment needed.
pub fn has_package_identity() -> bool {
    let mut len: u32 = 0;
    // A null buffer just asks for the required length. APPMODEL_ERROR_NO_PACKAGE
    // here (rather than ERROR_INSUFFICIENT_BUFFER, the ordinary "buffer too
    // small" answer) IS the "no identity" answer -- the length is never
    // actually needed.
    let err = unsafe { GetCurrentPackageFullName(&mut len, None) };
    err != windows::Win32::Foundation::APPMODEL_ERROR_NO_PACKAGE
}

/// Convert RGBA8 pixels (as `capture::RawShot::rgba` holds them) to BGRA8
/// with premultiplied alpha -- the pixel format
/// `SoftwareBitmap::CreateCopyWithAlphaFromBuffer` needs for OCR
/// (`BitmapPixelFormat::Bgra8` / `BitmapAlphaMode::Premultiplied`).
/// Screenshots captured by `xcap` are always fully opaque (alpha == 255),
/// so premultiplication is a no-op on the colour channels for every input
/// this module actually receives in production, but it is applied
/// unconditionally so the function is correct if that ever changes, and so
/// its own tests can exercise the non-opaque case.
pub fn rgba_to_bgra_premultiplied(rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(rgba.len());
    for px in rgba.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        let mul = |c: u8| ((c as u16 * a as u16 + 127) / 255) as u8;
        out.extend_from_slice(&[mul(b), mul(g), mul(r), a]);
    }
    out
}

/// Downscale sizing for `OcrEngine::MaxImageDimension` (the engine rejects
/// an image whose long edge exceeds it): fits `(w, h)` so neither dimension
/// exceeds `max_dim`, preserving aspect ratio, never upscaling, never
/// returning a zero dimension.
///
/// Same shape as `capture::fit_long_edge`, deliberately re-implemented
/// rather than imported: this module must not depend on `capture` (see the
/// module doc comment), and re-proving the "never zero" edge cases here
/// means this module's own tests are the evidence for its own behaviour.
pub fn fit_within_max_dimension(w: u32, h: u32, max_dim: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }
    let long_edge = w.max(h);
    if long_edge <= max_dim {
        return (w, h);
    }
    let target = max_dim.max(1) as f64;
    let scale = target / long_edge as f64;
    let new_w = ((w as f64) * scale).round().max(1.0) as u32;
    let new_h = ((h as f64) * scale).round().max(1.0) as u32;
    (new_w, new_h)
}

/// One line of text per OCR line, in reading order, joined with `\n`. Pure
/// and independent of the live WinRT call so it is unit-testable without a
/// real engine.
pub fn serialize_lines(lines: &[OcrLine]) -> String {
    lines
        .iter()
        .map(|l| l.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

/// The smallest rect containing both `a` and `b`.
fn union_rect(a: Rect, b: Rect) -> Rect {
    let x0 = a.X.min(b.X);
    let y0 = a.Y.min(b.Y);
    let x1 = (a.X + a.Width).max(b.X + b.Width);
    let y1 = (a.Y + a.Height).max(b.Y + b.Height);
    Rect {
        X: x0,
        Y: y0,
        Width: x1 - x0,
        Height: y1 - y0,
    }
}

/// A line's words with their rects, and the line's own bounding rect (the
/// union of its words' rects: `OcrLine`, the WinRT type, exposes no rect of
/// its own). Every rect is multiplied by `(sx, sy)` to map from the space
/// the engine saw (possibly downscaled to `MaxImageDimension`) back to the
/// caller's pixels. The line rect is a zero rect for a line with no words --
/// the OCR engine should never produce one, but this avoids a panic rather
/// than assuming that.
fn line_words_and_rect(line: &WinOcrLine, sx: f32, sy: f32) -> Result<(Vec<OcrWord>, Rect)> {
    let words = line.Words().context("OcrLine::Words")?;
    let mut acc: Option<Rect> = None;
    let mut out = Vec::new();
    for word in words.into_iter() {
        let r = word.BoundingRect().context("OcrWord::BoundingRect")?;
        let r = Rect {
            X: r.X * sx,
            Y: r.Y * sy,
            Width: r.Width * sx,
            Height: r.Height * sy,
        };
        out.push(OcrWord {
            text: word.Text().context("OcrWord::Text")?.to_string_lossy(),
            x: r.X,
            y: r.Y,
            width: r.Width,
            height: r.Height,
        });
        acc = Some(match acc {
            None => r,
            Some(a) => union_rect(a, r),
        });
    }
    let rect = acc.unwrap_or(Rect {
        X: 0.0,
        Y: 0.0,
        Width: 0.0,
        Height: 0.0,
    });
    Ok((out, rect))
}

/// Runs Windows OCR over one captured frame. **Must be called from a
/// worker thread**, never the UI thread (rule 5/7: nothing may block the
/// message loop) -- this blocks the calling thread up to `timeout` waiting
/// for the recognizer, bounded rather than the unbounded wait
/// `windows_future`'s `Async::join()` would give.
///
/// Initializes the calling thread's WinRT apartment
/// (`RoInitialize(RO_INIT_MULTITHREADED)`) for the duration of the call and
/// uninitializes it before returning, so this is safe to call from a fresh
/// worker thread each request (the shape `App::ask` already uses -- see
/// `src/app.rs`'s `std::thread::spawn` call site) without leaking apartment
/// state onto a thread the pool might reuse for something else.
///
/// Every failure path returns a plain `anyhow` error with no WinRT jargon a
/// card couldn't show (rule 7): the caller decides how to word the card.
pub fn recognize(rgba: &[u8], width: u32, height: u32, timeout: Duration) -> Result<OcrOutput> {
    let expected_len = (width as u64) * (height as u64) * 4;
    if rgba.len() as u64 != expected_len {
        return Err(anyhow!(
            "OCR input buffer is {} bytes, expected {expected_len} for {width}x{height} RGBA8",
            rgba.len()
        ));
    }
    if width == 0 || height == 0 {
        return Err(anyhow!("OCR input has a zero dimension ({width}x{height})"));
    }

    unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
        .context("RoInitialize failed: could not create a WinRT apartment on this thread")?;
    let result = recognize_inner(rgba, width, height, timeout);
    unsafe { RoUninitialize() };
    result
}

/// What the `[capture] ocr_language` setting resolved to (issue #111).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanguageChoice {
    /// Let Windows pick from the user's profile languages.
    Profile,
    /// Use this installed recognizer language (a tag from the installed list).
    Tag(String),
    /// The requested language has no installed OCR pack.
    Missing,
}

/// Process-wide mirror of `Config::capture.ocr_language`. `recognize` runs
/// on worker threads with no `Config` in reach, the same reason
/// `config::set_egress_preview_enabled` exists. `App::run` and
/// `App::apply_config` both call [`set_preferred_language`].
static PREFERRED_LANGUAGE: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

#[cfg(test)]
static PREF_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn set_preferred_language(tag: &str) {
    *PREFERRED_LANGUAGE.lock().unwrap_or_else(|e| e.into_inner()) = tag.trim().to_string();
}

/// The current setting; `"auto"` until something sets it.
pub fn preferred_language() -> String {
    let s = PREFERRED_LANGUAGE.lock().unwrap_or_else(|e| e.into_inner());
    if s.is_empty() {
        "auto".to_string()
    } else {
        s.clone()
    }
}

/// Primary language and optional script subtag of a BCP-47 tag, lowercased
/// (`zh-Hans-CN` is `("zh", Some("hans"))`, `de-DE` is `("de", None)`).
fn language_and_script(tag: &str) -> (String, Option<String>) {
    let mut parts = tag.split(['-', '_']);
    let lang = parts.next().unwrap_or("").to_ascii_lowercase();
    let script = parts
        .next()
        .filter(|p| p.len() == 4 && p.chars().all(|c| c.is_ascii_alphabetic()))
        .map(|p| p.to_ascii_lowercase());
    (lang, script)
}

/// Region the OCR pack for a bare language is expected to have, so `de`
/// resolves to `de-DE` however Windows orders its list.
fn default_region_tag(lang: &str) -> String {
    match lang {
        "en" => "en-us".to_string(),
        "pt" => "pt-br".to_string(),
        "ja" => "ja-jp".to_string(),
        "ko" => "ko-kr".to_string(),
        "zh" => "zh-hans-cn".to_string(),
        "sv" => "sv-se".to_string(),
        "da" => "da-dk".to_string(),
        "cs" => "cs-cz".to_string(),
        "el" => "el-gr".to_string(),
        "uk" => "uk-ua".to_string(),
        other => format!("{other}-{other}"),
    }
}

/// Pure language choice. `preferred` is `"auto"` (or empty) or a BCP-47 tag
/// such as `de`, `de-DE` or `zh-Hans`; `available` is the installed
/// recognizer tags.
///
/// Order: exact tag (case-insensitive); otherwise among packs of the same
/// primary language (and, when the request names a script, the same script
/// only, so `zh-Hans` never gets `zh-Hant-TW`): the language's default
/// region (`de-DE`, `en-US`...), then alphabetical, so the answer does not
/// depend on the order Windows lists packs in. A request with no pack is
/// `Missing`, never silently swapped for a different language or script.
pub fn select_language(preferred: &str, available: &[String]) -> LanguageChoice {
    let want = preferred.trim();
    if want.is_empty() || want.eq_ignore_ascii_case("auto") {
        return LanguageChoice::Profile;
    }
    if let Some(t) = available.iter().find(|t| t.eq_ignore_ascii_case(want)) {
        return LanguageChoice::Tag(t.clone());
    }
    let (want_lang, want_script) = language_and_script(want);
    let default_tag = default_region_tag(&want_lang);
    let mut candidates: Vec<&String> = available
        .iter()
        .filter(|t| {
            let (lang, script) = language_and_script(t);
            lang == want_lang && (want_script.is_none() || script == want_script)
        })
        .collect();
    candidates.sort_by_key(|t| {
        let lower = t.to_ascii_lowercase().replace('_', "-");
        (lower != default_tag, lower)
    });
    match candidates.first() {
        Some(t) => LanguageChoice::Tag((*t).clone()),
        None => LanguageChoice::Missing,
    }
}

/// The tag actually used by the last successful engine creation, for the
/// diagnostics report.
static LAST_USED_LANGUAGE: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

/// One diagnostics line: the setting, and which pack it resolved to when
/// that differs (`de, used de-DE`; `en-GB, used en-US`).
pub fn language_report(requested: &str, used: Option<&str>) -> String {
    match used {
        None => format!("{requested} (not used yet)"),
        Some(u) if u.eq_ignore_ascii_case(requested) => u.to_string(),
        Some(u) => format!("{requested}, used {u}"),
    }
}

/// Current `language_report` for the Copy diagnostics text.
pub fn language_status() -> String {
    let used = LAST_USED_LANGUAGE
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    language_report(&preferred_language(), used.as_deref())
}

/// Card text for a requested language with no installed OCR pack (rule 7).
pub fn missing_pack_message(requested: &str, available: &[String]) -> String {
    let installed = if available.is_empty() {
        "none".to_string()
    } else {
        available.join(", ")
    };
    format!(
        "No OCR language pack is installed for \"{requested}\" (installed: {installed}). \
         Add the language under Settings > Time & language > Language & region \
         and make sure its optical character recognition feature is installed, \
         or set ocr_language to \"auto\" in config.toml."
    )
}

/// Tags of every OCR recognizer language installed on this machine.
fn installed_language_tags() -> Result<Vec<String>> {
    let langs = OcrEngine::AvailableRecognizerLanguages()
        .context("OcrEngine::AvailableRecognizerLanguages")?;
    let mut out = Vec::new();
    for l in langs.into_iter() {
        out.push(
            l.LanguageTag()
                .context("Language::LanguageTag")?
                .to_string_lossy(),
        );
    }
    Ok(out)
}

fn create_engine() -> Result<OcrEngine> {
    let pref = preferred_language();
    let available = if select_language(&pref, &[]) == LanguageChoice::Profile {
        Vec::new() // auto never needs the list
    } else {
        installed_language_tags()?
    };
    match select_language(&pref, &available) {
        LanguageChoice::Missing => Err(anyhow!(missing_pack_message(&pref, &available))),
        LanguageChoice::Tag(tag) => {
            let lang = Language::CreateLanguage(&HSTRING::from(tag.as_str()))
                .context("Language::CreateLanguage")?;
            let engine = OcrEngine::TryCreateFromLanguage(&lang)
                .context("OcrEngine::TryCreateFromLanguage failed")?;
            if windows::core::Interface::as_raw(&engine).is_null() {
                return Err(anyhow!(missing_pack_message(&pref, &available)));
            }
            Ok(engine)
        }
        LanguageChoice::Profile => {
            let engine = OcrEngine::TryCreateFromUserProfileLanguages()
                .context("OcrEngine::TryCreateFromUserProfileLanguages failed")?;
            // THEORY (unverified): TryCreateFromUserProfileLanguages is
            // documented as a WinRT "TryCreate" factory, which signals
            // "nothing available" by returning a null interface inside an Ok
            // result, not an Err -- not exercised on this machine, which has
            // at least one OCR-capable language installed. Guarded
            // defensively rather than assumed away.
            if windows::core::Interface::as_raw(&engine).is_null() {
                return Err(anyhow!(
                    "no OCR-capable language is installed for this user profile; \
                     install one under Settings > Time & language > Language & region"
                ));
            }
            Ok(engine)
        }
    }
}

fn recognize_inner(rgba: &[u8], width: u32, height: u32, timeout: Duration) -> Result<OcrOutput> {
    let engine = create_engine()?;

    let language_tag = engine
        .RecognizerLanguage()
        .and_then(|l| l.LanguageTag())
        .map(|t| t.to_string_lossy())
        .unwrap_or_else(|_| "unknown".to_string());
    *LAST_USED_LANGUAGE.lock().unwrap_or_else(|e| e.into_inner()) = Some(language_tag.clone());

    // A failure to query the limit is treated as "assume the conservative
    // default this crate already uses elsewhere" rather than aborting OCR
    // over it -- the resize below is best-effort hardening against a
    // documented rejection, not the primary reason OCR would fail.
    let max_dim = OcrEngine::MaxImageDimension().unwrap_or(4096);
    let (fit_w, fit_h) = fit_within_max_dimension(width, height, max_dim);

    let bgra = if (fit_w, fit_h) == (width, height) {
        rgba_to_bgra_premultiplied(rgba)
    } else {
        let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
            .ok_or_else(|| anyhow!("RGBA buffer does not form a {width}x{height} image"))?;
        let resized =
            image::imageops::resize(&img, fit_w, fit_h, image::imageops::FilterType::Lanczos3);
        rgba_to_bgra_premultiplied(resized.as_raw())
    };

    let writer = DataWriter::new().context("DataWriter::new")?;
    writer.WriteBytes(&bgra).context("DataWriter::WriteBytes")?;
    let buffer = writer.DetachBuffer().context("DataWriter::DetachBuffer")?;

    let bitmap = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        fit_w as i32,
        fit_h as i32,
        BitmapAlphaMode::Premultiplied,
    )
    .context("SoftwareBitmap::CreateCopyWithAlphaFromBuffer")?;

    let op = engine
        .RecognizeAsync(&bitmap)
        .context("OcrEngine::RecognizeAsync")?;
    let ocr_result = wait_bounded(&op, timeout)?;

    // Rects come back in the (possibly downscaled) space the engine saw;
    // map them to the caller's pixels so a painted region lands on the text.
    let sx = width as f32 / fit_w as f32;
    let sy = height as f32 / fit_h as f32;
    let mut lines = Vec::new();
    for line in ocr_result.Lines().context("OcrResult::Lines")?.into_iter() {
        let text = line.Text().context("OcrLine::Text")?.to_string_lossy();
        let (words, rect) = line_words_and_rect(&line, sx, sy)?;
        lines.push(OcrLine {
            text,
            x: rect.X,
            y: rect.Y,
            width: rect.Width,
            height: rect.Height,
            words,
        });
    }

    Ok(OcrOutput {
        lines,
        language_tag,
    })
}

/// Polls `op` until it completes, errors, is canceled, or `timeout`
/// elapses, whichever comes first -- a bounded alternative to
/// `windows_future`'s `Async::join()`, which waits forever. On timeout,
/// `Cancel()` is requested (best-effort; its result is not itself checked,
/// since the timeout error is what matters to the caller either way).
fn wait_bounded(
    op: &windows_future::IAsyncOperation<windows::Media::Ocr::OcrResult>,
    timeout: Duration,
) -> Result<windows::Media::Ocr::OcrResult> {
    let deadline = Instant::now() + timeout;
    loop {
        let status = op.Status().context("IAsyncOperation::Status")?;
        match status {
            AsyncStatus::Completed => {
                return op.GetResults().context("IAsyncOperation::GetResults")
            }
            AsyncStatus::Error => {
                let code = op.ErrorCode().unwrap_or_default();
                return Err(anyhow!("OCR recognition failed: {code:?}"));
            }
            AsyncStatus::Canceled => return Err(anyhow!("OCR recognition was canceled")),
            _ => {}
        }
        if Instant::now() >= deadline {
            let _ = op.Cancel();
            return Err(anyhow!("OCR recognition timed out after {timeout:?}"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    // -- rgba_to_bgra_premultiplied --------------------------------------

    #[test]
    fn opaque_pixel_is_a_bgr_swap_only() {
        let rgba = [10u8, 20, 30, 255];
        assert_eq!(rgba_to_bgra_premultiplied(&rgba), vec![30, 20, 10, 255]);
    }

    #[test]
    fn empty_input_is_empty_output() {
        assert_eq!(rgba_to_bgra_premultiplied(&[]), Vec::<u8>::new());
    }

    #[test]
    fn semi_transparent_pixel_is_premultiplied() {
        let rgba = [200u8, 100, 50, 128];
        let expect = |c: u8| ((c as u16 * 128 + 127) / 255) as u8;
        assert_eq!(
            rgba_to_bgra_premultiplied(&rgba),
            vec![expect(50), expect(100), expect(200), 128]
        );
    }

    #[test]
    fn zero_alpha_premultiplies_colour_to_black() {
        let rgba = [255u8, 255, 255, 0];
        assert_eq!(rgba_to_bgra_premultiplied(&rgba), vec![0, 0, 0, 0]);
    }

    #[test]
    fn multiple_pixels_preserve_order() {
        let rgba = [1u8, 2, 3, 255, 4, 5, 6, 255];
        assert_eq!(
            rgba_to_bgra_premultiplied(&rgba),
            vec![3, 2, 1, 255, 6, 5, 4, 255]
        );
    }

    // -- fit_within_max_dimension -----------------------------------------

    #[test]
    fn already_within_bounds_is_unchanged() {
        assert_eq!(fit_within_max_dimension(800, 600, 4096), (800, 600));
    }

    #[test]
    fn landscape_downscales_to_max_dim() {
        assert_eq!(fit_within_max_dimension(8000, 4000, 4096), (4096, 2048));
    }

    #[test]
    fn portrait_downscales_to_max_dim() {
        assert_eq!(fit_within_max_dimension(4000, 8000, 4096), (2048, 4096));
    }

    #[test]
    fn exactly_max_dim_is_unchanged() {
        assert_eq!(fit_within_max_dimension(4096, 2000, 4096), (4096, 2000));
    }

    #[test]
    fn never_upscales() {
        assert_eq!(fit_within_max_dimension(100, 50, 4096), (100, 50));
    }

    #[test]
    fn zero_size_input_never_returns_zero_dimension() {
        let (w, h) = fit_within_max_dimension(0, 0, 4096);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn zero_max_dim_never_returns_zero_dimension() {
        let (w, h) = fit_within_max_dimension(1920, 1080, 0);
        assert!(w >= 1 && h >= 1);
    }

    #[test]
    fn extreme_aspect_avoids_zero_short_edge() {
        let (w, h) = fit_within_max_dimension(100_000, 1, 50);
        assert_eq!(w, 50);
        assert_eq!(h, 1);
    }

    // -- serialize_lines -----------------------------------------------

    #[test]
    fn serialize_lines_of_empty_input_is_empty_string() {
        assert_eq!(serialize_lines(&[]), "");
    }

    fn line(text: &str, y: f32) -> OcrLine {
        OcrLine {
            text: text.to_string(),
            x: 0.0,
            y,
            width: 10.0,
            height: 10.0,
            words: Vec::new(),
        }
    }

    #[test]
    fn serialize_lines_single_line_has_no_trailing_newline() {
        assert_eq!(serialize_lines(&[line("hello", 0.0)]), "hello");
    }

    #[test]
    fn serialize_lines_preserves_reading_order() {
        let lines = [line("first", 0.0), line("second", 10.0)];
        assert_eq!(serialize_lines(&lines), "first\nsecond");
    }

    #[test]
    fn serialize_lines_does_not_reorder_by_position() {
        // Reading order is whatever OcrResult::Lines already returned --
        // this function must not re-sort by y, even if a caller happened
        // to pass lines in reverse.
        let lines = [line("second", 10.0), line("first", 0.0)];
        assert_eq!(serialize_lines(&lines), "second\nfirst");
    }

    // -- union_rect ---------------------------------------------------

    #[test]
    fn union_rect_of_identical_rects_is_itself() {
        let r = Rect {
            X: 1.0,
            Y: 2.0,
            Width: 3.0,
            Height: 4.0,
        };
        assert_eq!(union_rect(r, r), r);
    }

    #[test]
    fn union_rect_of_disjoint_rects_spans_both() {
        let a = Rect {
            X: 0.0,
            Y: 0.0,
            Width: 10.0,
            Height: 10.0,
        };
        let b = Rect {
            X: 50.0,
            Y: 20.0,
            Width: 5.0,
            Height: 5.0,
        };
        assert_eq!(
            union_rect(a, b),
            Rect {
                X: 0.0,
                Y: 0.0,
                Width: 55.0,
                Height: 25.0,
            }
        );
    }

    #[test]
    fn union_rect_is_commutative() {
        let a = Rect {
            X: 3.0,
            Y: 3.0,
            Width: 2.0,
            Height: 2.0,
        };
        let b = Rect {
            X: -1.0,
            Y: 0.0,
            Width: 1.0,
            Height: 1.0,
        };
        assert_eq!(union_rect(a, b), union_rect(b, a));
    }

    // -- select_language (issue #111) -------------------------------------

    fn tags(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn auto_selects_the_user_profile_languages() {
        assert_eq!(
            select_language("auto", &tags(&["en-US", "de-DE"])),
            LanguageChoice::Profile
        );
        assert_eq!(
            select_language("", &tags(&["en-US"])),
            LanguageChoice::Profile
        );
        assert_eq!(
            select_language("  AUTO ", &tags(&[])),
            LanguageChoice::Profile
        );
    }

    #[test]
    fn exact_tag_match_ignores_case() {
        assert_eq!(
            select_language("de-de", &tags(&["en-US", "de-DE"])),
            LanguageChoice::Tag("de-DE".to_string())
        );
    }

    #[test]
    fn bare_language_matches_a_regional_pack() {
        assert_eq!(
            select_language("de", &tags(&["en-US", "de-DE"])),
            LanguageChoice::Tag("de-DE".to_string())
        );
    }

    #[test]
    fn exact_match_wins_over_a_primary_subtag_match() {
        assert_eq!(
            select_language("de-CH", &tags(&["de-DE", "de-CH"])),
            LanguageChoice::Tag("de-CH".to_string())
        );
    }

    #[test]
    fn a_regional_request_falls_back_to_the_same_language_pack() {
        assert_eq!(
            select_language("en-GB", &tags(&["en-US"])),
            LanguageChoice::Tag("en-US".to_string())
        );
    }

    #[test]
    fn script_subtag_must_match_the_pack_script() {
        let packs = tags(&["zh-Hant-TW", "zh-Hans-CN"]);
        assert_eq!(
            select_language("zh-Hans", &packs),
            LanguageChoice::Tag("zh-Hans-CN".to_string())
        );
        assert_eq!(
            select_language("zh-Hans", &tags(&["zh-Hant-TW"])),
            LanguageChoice::Missing
        );
        assert_eq!(
            select_language("zh-Hant-HK", &packs),
            LanguageChoice::Tag("zh-Hant-TW".to_string())
        );
    }

    #[test]
    fn bare_language_prefers_its_default_region_whatever_the_list_order() {
        assert_eq!(
            select_language("de", &tags(&["de-CH", "de-DE"])),
            LanguageChoice::Tag("de-DE".to_string())
        );
        assert_eq!(
            select_language("de", &tags(&["de-DE", "de-CH"])),
            LanguageChoice::Tag("de-DE".to_string())
        );
        assert_eq!(
            select_language("en", &tags(&["en-GB", "en-US"])),
            LanguageChoice::Tag("en-US".to_string())
        );
    }

    #[test]
    fn without_a_default_region_pack_the_choice_is_alphabetical() {
        assert_eq!(
            select_language("de", &tags(&["de-CH", "de-AT"])),
            LanguageChoice::Tag("de-AT".to_string())
        );
        assert_eq!(
            select_language("de", &tags(&["de-AT", "de-CH"])),
            LanguageChoice::Tag("de-AT".to_string())
        );
    }

    #[test]
    fn language_report_says_requested_and_used() {
        assert_eq!(language_report("auto", None), "auto (not used yet)");
        assert_eq!(language_report("de", None), "de (not used yet)");
        assert_eq!(language_report("de", Some("de-DE")), "de, used de-DE");
        assert_eq!(language_report("en-US", Some("en-US")), "en-US");
        assert_eq!(language_report("auto", Some("en-US")), "auto, used en-US");
    }

    #[test]
    fn missing_pack_is_reported_not_silently_replaced() {
        assert_eq!(
            select_language("de", &tags(&["en-US"])),
            LanguageChoice::Missing
        );
        assert_eq!(select_language("de", &[]), LanguageChoice::Missing);
    }

    #[test]
    fn missing_pack_message_names_the_language_and_the_windows_setting() {
        let msg = missing_pack_message("de", &tags(&["en-US", "fr-FR"]));
        assert!(msg.contains("\"de\""), "{msg}");
        assert!(msg.contains("en-US, fr-FR"), "{msg}");
        assert!(msg.contains("Settings > Time & language"), "{msg}");
        assert!(!msg.contains('\u{2014}'), "no em dash in a card string");
    }

    #[test]
    fn missing_pack_message_copes_with_no_installed_packs() {
        let msg = missing_pack_message("de", &[]);
        assert!(msg.contains("none"), "{msg}");
    }

    #[test]
    fn preferred_language_defaults_to_auto_and_round_trips() {
        let _g = PREF_TEST_LOCK.lock().unwrap();
        set_preferred_language("de");
        assert_eq!(preferred_language(), "de");
        set_preferred_language("auto");
        assert_eq!(preferred_language(), "auto");
    }

    // -- has_package_identity -------------------------------------------

    #[test]
    fn has_package_identity_is_false_under_cargo_test() {
        // A plain `cargo test` binary carries no package identity. This is
        // the same fact issue #30's measurement depends on -- see the
        // ignored live test below and this module's MEASURED doc comment.
        assert!(!has_package_identity());
    }

    // -- recognize: input validation (no WinRT call reached) -------------

    #[test]
    fn recognize_rejects_a_buffer_of_the_wrong_length() {
        let rgba = vec![0u8; 10]; // not width*height*4
        let err = recognize(&rgba, 4, 4, Duration::from_millis(1)).unwrap_err();
        assert!(err.to_string().contains("bytes, expected"));
    }

    #[test]
    fn recognize_rejects_a_zero_dimension() {
        let err = recognize(&[], 0, 4, Duration::from_millis(1)).unwrap_err();
        assert!(err.to_string().contains("zero dimension"));
    }

    // -- live OCR (issue #30's measurement) -------------------------------

    /// Renders `text` as black-on-white using GDI into an in-memory DIB and
    /// returns it as an opaque RGBA8 buffer (alpha forced to 255, matching
    /// what a real screenshot's alpha channel always is -- see
    /// `rgba_to_bgra_premultiplied`'s doc comment) -- a synthetic
    /// screenshot this test can run the real OCR pipeline over without a
    /// live screen capture. Test-only; not reachable from production code.
    pub(crate) unsafe fn render_gdi_text_rgba(text: &str, width: u32, height: u32) -> Vec<u8> {
        use windows::Win32::Foundation::{COLORREF, RECT};
        use windows::Win32::Graphics::Gdi::{
            CreateCompatibleDC, CreateDIBSection, CreateFontW, CreateSolidBrush, DeleteDC,
            DeleteObject, FillRect, SelectObject, SetBkColor, SetTextColor, ANSI_CHARSET,
            BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DEFAULT_QUALITY,
            DIB_RGB_COLORS, DT_CENTER, DT_SINGLELINE, DT_VCENTER, FW_NORMAL, OUT_DEFAULT_PRECIS,
        };

        let hdc = CreateCompatibleDC(None);
        assert!(!hdc.is_invalid(), "CreateCompatibleDC failed");

        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width as i32,
                biHeight: -(height as i32), // negative => top-down, row 0 first
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
        assert!(!bits.is_null(), "CreateDIBSection returned a null buffer");

        let old_bitmap = SelectObject(hdc, hbitmap.into());

        let white = CreateSolidBrush(COLORREF(0x00FF_FFFF));
        let rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        FillRect(hdc, &rect, white);
        let _ = DeleteObject(white.into());

        let hfont = CreateFontW(
            -40,
            0,
            0,
            0,
            FW_NORMAL.0 as i32,
            0,
            0,
            0,
            ANSI_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            DEFAULT_QUALITY,
            DEFAULT_PITCH.0 as u32,
            windows::core::w!("Segoe UI"),
        );
        let old_font = SelectObject(hdc, hfont.into());
        SetTextColor(hdc, COLORREF(0x0000_0000));
        SetBkColor(hdc, COLORREF(0x00FF_FFFF));

        // Routed through the shared guard (issue #221) rather than a raw
        // `DrawTextW` call: this helper's `text` is always a literal in this
        // module's own tests today, but nothing enforces that at the call
        // site, and an empty-string `DrawTextW` call crashes with
        // `STATUS_ACCESS_VIOLATION` (MEASURED 2026-09-17, palette branch
        // commit `453fe0b`).
        crate::ui::text::draw_text_line(hdc, text, rect, DT_SINGLELINE | DT_CENTER | DT_VCENTER);

        let pixel_count = (width as usize) * (height as usize) * 4;
        let bgra = std::slice::from_raw_parts(bits as *const u8, pixel_count).to_vec();

        SelectObject(hdc, old_font);
        let _ = DeleteObject(hfont.into());
        SelectObject(hdc, old_bitmap);
        let _ = DeleteObject(hbitmap.into());
        let _ = DeleteDC(hdc);

        let mut rgba = Vec::with_capacity(pixel_count);
        for px in bgra.chunks_exact(4) {
            rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
        }
        rgba
    }

    /// Issue #111 hand check: lists the installed packs, then asks for a
    /// language with no pack ("xx") and expects the card text, then for the
    /// first installed pack and expects recognition under that tag. For the
    /// German "done when", install the German pack and run with
    /// `WINGMAN_OCR_LIVE_LANG=de` to also try that tag.
    #[test]
    #[ignore = "live WinRT OCR; run manually: cargo test ocr_live_language -- --ignored --nocapture"]
    fn ocr_live_language_selection() {
        let _g = PREF_TEST_LOCK.lock().unwrap();
        let installed = installed_language_tags().expect("list packs");
        println!("installed OCR packs: {installed:?}");
        let rgba = unsafe { render_gdi_text_rgba("Wingman OCR 12345", 900, 160) };

        set_preferred_language("xx");
        let err = recognize(&rgba, 900, 160, Duration::from_secs(15)).unwrap_err();
        println!("missing-pack card text: {err}");
        assert!(err.to_string().contains("Settings > Time & language"));

        assert!(
            !installed.is_empty(),
            "no OCR language pack is installed on this machine; install one under \
             Settings > Time & language > Language & region and rerun"
        );
        let mut tries = vec![installed[0].clone()];
        if let Ok(l) = std::env::var("WINGMAN_OCR_LIVE_LANG") {
            tries.push(l);
        }
        for t in tries {
            set_preferred_language(&t);
            let out = recognize(&rgba, 900, 160, Duration::from_secs(15)).expect("recognize");
            println!(
                "requested {t}: used {} -> {:?}",
                out.language_tag,
                serialize_lines(&out.lines)
            );
        }
        set_preferred_language("auto");
    }

    /// The measurement issue #30 exists to make: does `Windows.Media.Ocr`
    /// work from a process with no package identity, and how fast.
    ///
    /// Run manually (AGENTS.md build rules -- never bare `cargo test`):
    /// ```text
    /// export CARGO_TARGET_DIR=<repo>/target/wt/<worktree> RUSTC_WRAPPER=sccache CARGO_BUILD_JOBS=2
    /// cargo test ocr_live -- --ignored --nocapture
    /// ```
    /// Prints `MEASURED 2026-09-17:` lines with the cold/warm latency and
    /// recognizer language actually observed; the module doc comment above
    /// and AGENTS.md's "Windows OCR under a sparse package" pitfall were
    /// updated from one such run's output.
    #[test]
    #[ignore = "live WinRT OCR call against a rendered image; run manually, see this test's doc comment"]
    fn ocr_live_recognizes_gdi_rendered_text() {
        assert!(
            !has_package_identity(),
            "expected a plain `cargo test` binary to have no package identity -- \
             if this fails, the measurement below no longer tests what it claims to"
        );

        let (width, height) = (900u32, 160u32);
        let rgba = unsafe { render_gdi_text_rgba("Wingman OCR 12345", width, height) };

        let cold_start = Instant::now();
        let first = recognize(&rgba, width, height, Duration::from_secs(15))
            .expect("OCR failed on the first (cold) call");
        let cold_ms = cold_start.elapsed().as_millis();

        let warm_start = Instant::now();
        let second = recognize(&rgba, width, height, Duration::from_secs(15))
            .expect("OCR failed on the second (warm) call");
        let warm_ms = warm_start.elapsed().as_millis();

        let text = serialize_lines(&first.lines);
        println!(
            "MEASURED 2026-09-17: OCR without package identity succeeded. \
             cold={cold_ms}ms warm={warm_ms}ms language={}",
            first.language_tag
        );
        println!("MEASURED 2026-09-17: recognized text: {text:?}");
        println!(
            "MEASURED 2026-09-17: OcrEngine::MaxImageDimension = {:?}",
            OcrEngine::MaxImageDimension()
        );

        assert!(
            text.contains("Wingman") && text.contains("OCR") && text.contains("12345"),
            "expected the rendered text to be recognized, got {text:?}"
        );
        assert!(!second.lines.is_empty());
    }

    /// The measurement the screenshot-redaction design (#103) rests on: how
    /// long does `recognize` take on a REAL capture of the screen this test
    /// runs on, at the size a cloud request would be sent? Prints only
    /// timings and counts, never any recognized text.
    ///
    /// ```text
    /// cargo test ocr_live_times_a_real_screen_capture -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "live screen capture plus WinRT OCR; run manually, see this test's doc comment"]
    fn ocr_live_times_a_real_screen_capture() {
        let raw = crate::capture::grab_raw("primary", 1568, 1568 * 28 * 28)
            .expect("screen capture failed");
        let mut runs = Vec::new();
        let mut line_count = 0;
        for _ in 0..5 {
            let start = Instant::now();
            let out = recognize(&raw.rgba, raw.width, raw.height, Duration::from_secs(15))
                .expect("OCR failed on a real capture");
            runs.push(start.elapsed().as_millis());
            line_count = out.lines.len();
        }
        println!(
            "MEASURED: OCR of a real {}x{} capture: runs_ms={runs:?} lines={line_count}",
            raw.width, raw.height
        );
    }
}
