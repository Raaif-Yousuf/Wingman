//! Screen capture: find the target monitor, grab it, downscale, and encode PNG.
//!
//! This module knows about Win32 (to find the target monitor rect) and about
//! xcap/image (to actually grab and encode pixels). It must not know about
//! providers.

use crate::provider::Shot;
use anyhow::{anyhow, Context, Result};
use image::imageops::{self, FilterType};
use image::ImageEncoder;
use std::io::Cursor;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

// The constants below are sensible per-provider/per-tier limits for
// `fit_for_model`. Issue #169: now referenced from `src/provider/**` (each
// provider's `capabilities()` fills `Caps::image_limits` from these), and
// from this module's own `resolve_limits`/`grab_raw` via `App::ask`'s
// capture call site -- no longer dead code outside tests.
/// Anthropic's "standard" resolution tier (all current models except Claude
/// 4.7 and later): long edge at most 1568 px, and total visual tokens (Claude
/// tiles images into 28x28-pixel patches; `tokens = ceil(w/28) * ceil(h/28)`)
/// at most 1568. MEASURED 2026-09-17 from
/// <https://platform.claude.com/docs/en/build-with-claude/vision> ("Resolution
/// and token cost"). `1568 * 28 * 28 = 1_229_312` is the largest pixel area
/// that can stay at or under the 1568-token budget for *any* aspect ratio;
/// the real per-image cap is tighter (patch-quantized), so this is a safe
/// upper bound, not an exact reproduction of Claude's own rounding.
pub const ANTHROPIC_STANDARD_MAX_LONG_EDGE: u32 = 1568;
pub const ANTHROPIC_STANDARD_MAX_PIXELS: u64 = 1568 * 28 * 28;

/// Claude 4.7 and later, "high-resolution" tier: long edge at most 2576 px,
/// visual tokens at most 4784. Same source and date as the standard tier
/// above.
pub const ANTHROPIC_HIGH_RES_MAX_LONG_EDGE: u32 = 2576;
pub const ANTHROPIC_HIGH_RES_MAX_PIXELS: u64 = 4784 * 28 * 28;

/// OpenAI's legacy tile-based vision models (gpt-4o, gpt-4.1 class) with
/// `detail: "high"`: the first resize stage fits the image within a
/// 2048x2048 square. MEASURED 2026-09-17 from
/// <https://developers.openai.com/api/docs/guides/images-vision> (512px
/// tiles, 85 base tokens + 170 tokens/tile). OpenAI's real second stage then
/// further rescales so the *shortest* side is 768 px -- a short-edge
/// constraint this module does not model (see `openai.rs`'s `capabilities()`
/// doc comment), so `OPENAI_TILE_MAX_PIXELS` is only the 2048x2048
/// first-stage bound, not the true token-minimal size.
pub const OPENAI_TILE_MAX_LONG_EDGE: u32 = 2048;
pub const OPENAI_TILE_MAX_PIXELS: u64 = 2048 * 2048;

/// Gemini's conservative default (issue #169): Gemini's own docs (fetched
/// 2026-09-17, <https://ai.google.dev/gemini-api/docs/image-understanding>)
/// describe tiling into 768x768-pixel tiles at 258 tokens/tile but document
/// no hard maximum image dimension at all. THEORY (unverified): rather than
/// send an unbounded image, this caps at 2x2 tiles per edge (~4 tiles,
/// roughly 1032 tokens for a typical screen aspect ratio) to keep upload
/// size and token cost in the same order of magnitude as Anthropic's
/// standard tier above -- a deliberately conservative choice, not a
/// documented Gemini limit.
pub const GEMINI_CONSERVATIVE_MAX_LONG_EDGE: u32 = 1536;
pub const GEMINI_CONSERVATIVE_MAX_PIXELS: u64 = 1536 * 1536;

/// Ollama's conservative default (issue #169): Ollama runs whatever
/// user-pulled model is configured, so there is no single vendor-documented
/// image limit the way there is for a hosted API. THEORY (unverified): reuse
/// Anthropic's standard-tier numbers as a reasonable, already-justified
/// budget (most local vision encoders in the 2026-09-16 expansion plan's
/// hardware table -- gemma3/gemma4/qwen3.5 -- are ViT-based at a similar
/// patch scale), which also keeps local CPU/GPU decode time bounded on this
/// machine's iGPU (AGENTS.md's battery-drain concern).
pub const OLLAMA_CONSERVATIVE_MAX_LONG_EDGE: u32 = ANTHROPIC_STANDARD_MAX_LONG_EDGE;
pub const OLLAMA_CONSERVATIVE_MAX_PIXELS: u64 = ANTHROPIC_STANDARD_MAX_PIXELS;

/// Raw RGBA8 pixels captured and downscaled on the main thread, not yet
/// encoded to PNG.
///
/// Split out from the old `grab` (issue #177): `App::ask` captures via
/// [`grab_raw`] and shows the pending card *before* calling [`encode`], which
/// is the expensive step -- up to ~1s in a release build at the old `Best`
/// compression setting on a 1402x876 image (see `encode_png`'s doc comment
/// for the measured numbers) -- so `encode` now runs on the worker thread,
/// after the card is already on screen, instead of blocking the main
/// thread's message loop before it.
pub struct RawShot {
    /// Row-major RGBA8, `width * height * 4` bytes.
    pub rgba: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// Resolves the `(max_long_edge, max_pixels)` [`fit_for_model`] should use
/// for one capture (issue #169), from the actually-configured provider's
/// real per-model image limits plus the user's own `config.capture.max_edge`
/// setting.
///
/// `provider_limits` is `Caps::image_limits` for the model the FIRST
/// provider the chain will try is configured with (see
/// `Chain::first_ready_caps`) -- `None` when that is not known (e.g. no
/// provider is ready yet), in which case this falls back entirely to the
/// old max_edge-derived heuristic (`max_edge * 28 * 28`, which reduces to
/// Anthropic's own standard-tier pixel budget at the config default of
/// 1568).
///
/// When the provider's real limits ARE known, `user_max_edge` only ever
/// tightens the long edge further -- it is a cap on top of the provider's
/// own limit, never a request for more resolution than the provider would
/// keep. The provider's own pixel budget is used as-is: the user has no
/// separate pixel-budget setting to combine it with.
pub fn resolve_limits(
    provider_limits: Option<crate::provider::ImageLimits>,
    user_max_edge: u32,
) -> (u32, u64) {
    match provider_limits {
        Some(limits) => (limits.max_long_edge.min(user_max_edge), limits.max_pixels),
        None => (user_max_edge, (user_max_edge as u64) * 28 * 28),
    }
}

/// Capture the target monitor (per `monitor_mode`) and downscale to fit
/// within `max_long_edge`/`max_pixels` (see [`resolve_limits`] for how a
/// caller derives those from the configured provider's real limits and the
/// user's own cap). Does not encode -- see [`encode`] and this function's
/// doc comment on [`RawShot`] for why that is a separate, later step.
///
/// `monitor_mode` is `"active"` (the monitor under the foreground window) or
/// `"primary"` (always the system's primary monitor). Anything else is
/// treated as `"active"`.
pub fn grab_raw(monitor_mode: &str, max_long_edge: u32, max_pixels: u64) -> Result<RawShot> {
    let rect = match monitor_mode {
        "primary" => primary_monitor_rect()?,
        _ => active_monitor_rect()?,
    };

    let monitors = xcap::Monitor::all().map_err(|e| anyhow!("enumerating monitors: {e}"))?;
    if monitors.is_empty() {
        return Err(anyhow!("no monitors found"));
    }

    // Match by origin coordinates -- xcap's monitor naming is unstable across
    // platforms/drivers, but the Win32 rect origin and xcap's x()/y() both
    // describe the same virtual-screen coordinate space.
    let target = monitors
        .iter()
        .find(|m| matches!((m.x(), m.y()), (Ok(x), Ok(y)) if x == rect.left && y == rect.top))
        .or_else(|| monitors.iter().find(|m| matches!(m.is_primary(), Ok(true))))
        .unwrap_or(&monitors[0]);

    let image = target
        .capture_image()
        .map_err(|e| anyhow!("capturing monitor: {e}"))?;

    let (w, h) = (image.width(), image.height());
    let (nw, nh) = fit_for_model(w, h, max_long_edge, max_pixels);

    let resized = if (nw, nh) == (w, h) {
        image
    } else {
        imageops::resize(&image, nw, nh, FilterType::Lanczos3)
    };

    let (final_w, final_h) = (resized.width(), resized.height());
    Ok(RawShot {
        rgba: resized.into_raw(),
        width: final_w,
        height: final_h,
    })
}

/// Encode a [`RawShot`] to PNG. The expensive half of the old `grab` --
/// see [`RawShot`]'s doc comment for why the caller runs this on a worker
/// thread, after the pending card is already shown, rather than inline with
/// [`grab_raw`].
pub fn encode(raw: &RawShot) -> Result<Shot> {
    let png = encode_png(&raw.rgba, raw.width, raw.height)?;
    Ok(Shot {
        png,
        width: raw.width,
        height: raw.height,
    })
}

/// Encode raw RGBA8 pixels as a PNG.
///
/// Uses `CompressionType::Default` (`FilterType::Adaptive`) -- neither the
/// `image` crate's own `PngEncoder::new` default (`CompressionType::Fast`)
/// nor the smallest-output `CompressionType::Best` this module used before
/// issue #177.
///
/// MEASURED 2026-09-17 (release build, this crate's `opt-level = "z"` + LTO
/// profile, `bench_png_compression_levels`, median of 5 runs, synthetic
/// UI-like images at the two sizes `fit_for_model` actually produces for a
/// typical screen at the default `max_edge` 1568):
///
/// | size | level | encode | bytes |
/// |---|---|---|---|
/// | 1402x876 | Fast | 16.1 ms | 76.6 KiB |
/// | 1402x876 | Default | 23.5 ms | 8.2 KiB |
/// | 1402x876 | Best | 25.2 ms | 8.2 KiB |
/// | 1568x980 | Fast | 19.3 ms | 96.7 KiB |
/// | 1568x980 | Default | 28.9 ms | 10.1 KiB |
/// | 1568x980 | Best | 32.5 ms | 10.2 KiB |
///
/// Best barely beats Default on bytes here (this synthetic image's mostly
/// flat background compresses much further than Fast's fixed window size
/// allows, so the marginal gain from Best's slower search is tiny) while
/// costing noticeably more encode time, so it is never the right choice on
/// this shape of image. Assuming a 20 Mbit/s uplink (2500 bytes/ms) and
/// summing encode time plus upload time, at 1402x876: Fast ~16.1 + 76.6*1024
/// /2500 = ~47 ms, Default ~23.5 + 8.2*1024/2500 = ~27 ms, Best ~25.2 +
/// 8.2*1024/2500 = ~29 ms -- Default wins outright, not just on encode time
/// alone. `pick_compression_level` and its tests
/// (`pick_compression_level_matches_this_module_s_actual_choice_at_20_mbit`)
/// encode this same tradeoff as checked logic against these exact numbers,
/// not just a one-off eyeball of the table above.
fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let mut png = Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new_with_quality(
        &mut png,
        image::codecs::png::CompressionType::Default,
        image::codecs::png::FilterType::Adaptive,
    )
    .write_image(rgba, width, height, image::ExtendedColorType::Rgba8)
    .context("encoding PNG")?;
    Ok(png.into_inner())
}

/// Picks the compression level with the lowest total encode-plus-upload
/// time, given each candidate's measured `(encode_ms, output_bytes)` and an
/// assumed uplink in bits/second. Pure decision logic pulled out of
/// `bench_png_compression_levels` so the tradeoff itself -- not just the
/// numbers that went into it -- is unit-tested (issue #177's "pure decision
/// tests where possible"). Only exercised by that test module today (the
/// level `encode_png` actually uses is a `const`, chosen once from a live
/// run of the numbers this proves the logic against), not called from
/// production code.
#[allow(dead_code)]
fn pick_compression_level(
    candidates: &[(image::codecs::png::CompressionType, f64, usize)],
    uplink_bits_per_sec: f64,
) -> image::codecs::png::CompressionType {
    let uplink_bytes_per_ms = uplink_bits_per_sec / 8.0 / 1000.0;
    candidates
        .iter()
        .min_by(|a, b| {
            let total_a = a.1 + (a.2 as f64 / uplink_bytes_per_ms);
            let total_b = b.1 + (b.2 as f64 / uplink_bytes_per_ms);
            total_a
                .partial_cmp(&total_b)
                .expect("encode ms / byte counts are always finite")
        })
        .map(|(level, _, _)| *level)
        .expect("candidates is never empty in practice")
}

/// Fit `(w, h)` within both a maximum long edge and a maximum total pixel
/// count, preserving aspect ratio and never upscaling.
///
/// This generalizes [`fit_long_edge`] with a second constraint modeled on
/// how vision models actually limit images: a long edge cap alone is not
/// enough to describe a provider's real behaviour when it also caps a
/// derived quantity such as total tokens or total tiles (see the
/// `ANTHROPIC_*`/`OPENAI_*` constants above). For most screen aspect ratios
/// (16:9, 16:10 and wider) the pixel/token cap binds before the long-edge
/// cap does, so a long-edge-only fit sends more pixels than the provider
/// will keep once it downscales server-side (or, for a local model with no
/// such server-side safety net, more pixels than it needs to process at
/// all) -- extra upload bytes and latency, and for local models extra
/// compute time, for no benefit.
///
/// `max_pixels` is a `u64` so `width as u64 * height as u64` never overflows.
pub fn fit_for_model(w: u32, h: u32, max_long_edge: u32, max_pixels: u64) -> (u32, u32) {
    let (w1, h1) = fit_long_edge(w, h, max_long_edge);
    let area = (w1 as u64) * (h1 as u64);
    let budget = max_pixels.max(1);
    if area <= budget {
        return (w1, h1);
    }

    let scale = (budget as f64 / area as f64).sqrt();
    let new_w = ((w1 as f64) * scale).floor().max(1.0) as u32;
    let new_h = ((h1 as f64) * scale).floor().max(1.0) as u32;
    (new_w, new_h)
}

/// Compute the size an image should be resized to so its long edge equals
/// `max_edge`, preserving aspect ratio.
///
/// Never upscales (an already-smaller image is returned unchanged) and never
/// returns a zero dimension, even for degenerate inputs like `max_edge == 0`
/// or extreme aspect ratios that would otherwise round a short edge to 0.
pub fn fit_long_edge(w: u32, h: u32, max_edge: u32) -> (u32, u32) {
    if w == 0 || h == 0 {
        return (w.max(1), h.max(1));
    }

    let long_edge = w.max(h);
    if long_edge <= max_edge {
        return (w, h);
    }

    // long_edge > max_edge here, and long_edge >= 1, so max_edge could still
    // be 0 -- clamp the divisor to at least 1 so we scale toward a 1x1 image
    // instead of dividing by zero.
    let target = max_edge.max(1) as f64;
    let scale = target / long_edge as f64;

    let new_w = ((w as f64) * scale).round().max(1.0) as u32;
    let new_h = ((h as f64) * scale).round().max(1.0) as u32;

    (new_w, new_h)
}

/// The Win32 rect (in virtual-screen coordinates) of the monitor containing
/// the foreground window, falling back to the primary monitor when there is
/// no foreground window or the lookup fails.
///
/// `pub(crate)` (added for #25): `ui::palette` reuses this exact function to
/// center the Quick Ask palette on the active monitor, rather than
/// duplicating the `MonitorFromWindow`/`GetMonitorInfoW` sequence -- see the
/// `wired-to-nothing` skill's "a hard-coded list" row on why a sibling copy
/// of monitor-lookup logic is worth avoiding.
pub(crate) fn active_monitor_rect() -> Result<RECT> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return primary_monitor_rect();
    }

    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    rect_from_hmonitor(hmon).or_else(|_| primary_monitor_rect())
}

/// The Win32 rect (in virtual-screen coordinates) of the system's primary
/// monitor.
fn primary_monitor_rect() -> Result<RECT> {
    let hmon = unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) };
    rect_from_hmonitor(hmon)
}

fn rect_from_hmonitor(hmon: HMONITOR) -> Result<RECT> {
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    let ok = unsafe { GetMonitorInfoW(hmon, &mut mi) };
    if !ok.as_bool() {
        return Err(anyhow!("GetMonitorInfoW failed"));
    }

    Ok(mi.rcMonitor)
}

// ---------------------------------------------------------------------------
// Virtual-desktop (all-monitors) capture -- issue #29's region/window
// overlay (`ui::region`) freezes one composited frame of the whole desktop
// before it shows its overlay window, so the crop it eventually returns
// matches exactly what was on screen at the moment the user pressed the
// hotkey, not a re-capture taken after the overlay (which would show up in
// its own screenshot) is already on top.
// ---------------------------------------------------------------------------

/// The bounding rectangle of the entire virtual desktop (every monitor
/// combined), in physical pixels. This is Win32's virtual-screen coordinate
/// space, which a `DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2` process (set
/// once, process-wide, in `App::run`) receives un-scaled: `left`/`top` can
/// be negative when a monitor sits above or to the left of the primary
/// monitor's own origin.
pub fn virtual_desktop_rect() -> Result<RECT> {
    let left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let width = unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let height = unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if width <= 0 || height <= 0 {
        return Err(anyhow!(
            "GetSystemMetrics reported an empty virtual desktop"
        ));
    }
    Ok(RECT {
        left,
        top,
        right: left + width,
        bottom: top + height,
    })
}

/// Captures every monitor at native resolution and composites the results
/// into one RGBA8 buffer sized to [`virtual_desktop_rect`], each monitor's
/// pixels placed at its own offset within that buffer via [`blit_into`]. No
/// downscaling: the region overlay crops out of this buffer at native
/// resolution, so shrinking it here would shrink the eventual crop too.
///
/// Pixels not covered by any monitor (possible when monitors of different
/// physical sizes/DPI scale factors do not tile edge-to-edge) are left as
/// opaque black -- harmless, since every rectangle `ui::region` can produce
/// is clamped to the real desktop bounds first, so a gap pixel is never
/// actually selectable.
pub fn grab_virtual_desktop_raw() -> Result<RawShot> {
    let desktop = virtual_desktop_rect()?;
    let width = (desktop.right - desktop.left).max(1) as u32;
    let height = (desktop.bottom - desktop.top).max(1) as u32;
    let mut rgba = vec![0u8; (width as usize) * (height as usize) * 4];

    let monitors = xcap::Monitor::all().map_err(|e| anyhow!("enumerating monitors: {e}"))?;
    if monitors.is_empty() {
        return Err(anyhow!("no monitors found"));
    }

    for m in &monitors {
        let (Ok(mx), Ok(my)) = (m.x(), m.y()) else {
            continue;
        };
        let Ok(image) = m.capture_image() else {
            continue;
        };
        let (mw, mh) = (image.width(), image.height());
        blit_into(
            &mut rgba,
            width,
            height,
            image.as_raw(),
            mw,
            mh,
            mx - desktop.left,
            my - desktop.top,
        );
    }

    Ok(RawShot {
        rgba,
        width,
        height,
    })
}

/// Copies `src` (an RGBA8 `src_w`x`src_h` buffer) into `dst` (an RGBA8
/// `dst_w`x`dst_h` buffer) at offset `(ox, oy)`, clipping whatever part of
/// `src` would fall outside `dst`'s bounds -- including a partially or
/// fully negative offset, which is the normal case for every monitor except
/// the one at the virtual desktop's own origin. Row-copies the clipped
/// overlap rather than checking bounds per pixel, so this stays fast enough
/// to run once per monitor on every region-overlay open.
///
/// Pure pixel arithmetic over plain slices -- no xcap or Win32 type
/// anywhere in the signature -- so this is exercised directly against
/// synthetic buffers, without a real multi-monitor capture.
#[allow(clippy::too_many_arguments)] // a `Rect`-like bundle per buffer would
                                     // just move these same 8 primitives
                                     // into two structs only this function
                                     // and its tests ever construct
fn blit_into(
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    src: &[u8],
    src_w: u32,
    src_h: u32,
    ox: i32,
    oy: i32,
) {
    if src_w == 0 || src_h == 0 || dst_w == 0 || dst_h == 0 {
        return;
    }

    let src_x0 = (-ox).max(0) as u32;
    let src_y0 = (-oy).max(0) as u32;
    let src_x1 = (((dst_w as i32) - ox).max(0) as u32).min(src_w);
    let src_y1 = (((dst_h as i32) - oy).max(0) as u32).min(src_h);
    if src_x0 >= src_x1 || src_y0 >= src_y1 {
        return; // src and dst do not overlap at all
    }

    let row_bytes = ((src_x1 - src_x0) as usize) * 4;
    for sy in src_y0..src_y1 {
        let dy = (oy + sy as i32) as u32;
        let s_start = ((sy * src_w + src_x0) * 4) as usize;
        let dx0 = (ox + src_x0 as i32) as u32;
        let d_start = ((dy * dst_w + dx0) * 4) as usize;
        if s_start + row_bytes <= src.len() && d_start + row_bytes <= dst.len() {
            dst[d_start..d_start + row_bytes].copy_from_slice(&src[s_start..s_start + row_bytes]);
        }
    }
}

/// A pixel rectangle in the same buffer-local (non-negative) coordinate
/// space as an already-captured [`RawShot`] -- what [`crop_rgba`] takes,
/// unlike `ui::region::Rect`, which is virtual-desktop space and can be
/// negative. `ui::region::to_buffer_rect` converts between the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RectPx {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

/// Crops `rect` out of an RGBA8 `width`x`height` buffer, clamping `rect` to
/// the buffer's own bounds first rather than trusting the caller (AGENTS.md
/// rule 8: a parser/guard should not trust its input even when every
/// current caller already clamps). Never returns a zero-size image: a
/// completely out-of-bounds or zero-area `rect` clamps to a single pixel
/// rather than an empty `RawShot` every downstream consumer would otherwise
/// have to special-case.
pub fn crop_rgba(rgba: &[u8], width: u32, height: u32, rect: RectPx) -> RawShot {
    if width == 0 || height == 0 || rgba.len() < (width as usize) * (height as usize) * 4 {
        return RawShot {
            rgba: vec![0; 4],
            width: 1,
            height: 1,
        };
    }

    let x0 = rect.x.min(width - 1);
    let y0 = rect.y.min(height - 1);
    let x1 = rect.x.saturating_add(rect.w).min(width).max(x0 + 1);
    let y1 = rect.y.saturating_add(rect.h).min(height).max(y0 + 1);
    let cw = x1 - x0;
    let ch = y1 - y0;

    let row_bytes = (cw as usize) * 4;
    let mut out = vec![0u8; row_bytes * (ch as usize)];
    for row in 0..ch {
        let src_start = (((y0 + row) * width + x0) * 4) as usize;
        let dst_start = (row as usize) * row_bytes;
        out[dst_start..dst_start + row_bytes]
            .copy_from_slice(&rgba[src_start..src_start + row_bytes]);
    }

    RawShot {
        rgba: out,
        width: cw,
        height: ch,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        blit_into, crop_rgba, encode, encode_png, fit_for_model, fit_long_edge,
        pick_compression_level, resolve_limits, Cursor, RawShot, RectPx,
        ANTHROPIC_HIGH_RES_MAX_LONG_EDGE, ANTHROPIC_HIGH_RES_MAX_PIXELS,
        ANTHROPIC_STANDARD_MAX_LONG_EDGE, ANTHROPIC_STANDARD_MAX_PIXELS, OPENAI_TILE_MAX_LONG_EDGE,
        OPENAI_TILE_MAX_PIXELS,
    };
    use crate::provider::ImageLimits;
    use image::ImageEncoder;

    // -- resolve_limits (issue #169) -------------------------------------

    #[test]
    fn resolve_limits_falls_back_to_user_max_edge_heuristic_when_provider_unknown() {
        let (edge, pixels) = resolve_limits(None, 1568);
        assert_eq!(edge, 1568);
        assert_eq!(pixels, 1568 * 28 * 28);
    }

    #[test]
    fn resolve_limits_uses_providers_pixel_budget_as_is() {
        let limits = ImageLimits {
            max_long_edge: ANTHROPIC_HIGH_RES_MAX_LONG_EDGE,
            max_pixels: ANTHROPIC_HIGH_RES_MAX_PIXELS,
        };
        // User cap looser than the provider's own long edge: the provider's
        // limit wins, unchanged.
        let (edge, pixels) = resolve_limits(Some(limits), 4000);
        assert_eq!(edge, ANTHROPIC_HIGH_RES_MAX_LONG_EDGE);
        assert_eq!(pixels, ANTHROPIC_HIGH_RES_MAX_PIXELS);
    }

    #[test]
    fn resolve_limits_user_max_edge_tightens_a_looser_provider_limit() {
        let limits = ImageLimits {
            max_long_edge: ANTHROPIC_HIGH_RES_MAX_LONG_EDGE, // 2576
            max_pixels: ANTHROPIC_HIGH_RES_MAX_PIXELS,
        };
        // User has capped max_edge below the provider's own limit: the
        // user's cap wins for the long edge (min of both), but the
        // provider's pixel budget is unaffected -- the user has no
        // separate pixel-budget setting.
        let (edge, pixels) = resolve_limits(Some(limits), 1000);
        assert_eq!(edge, 1000);
        assert_eq!(pixels, ANTHROPIC_HIGH_RES_MAX_PIXELS);
    }

    #[test]
    fn resolve_limits_switching_provider_changes_the_downscale_target() {
        // Issue #169's own "Done when": switching the configured provider
        // changes the downscale target for a fixed input image. Anthropic
        // standard vs. OpenAI's tile budget give different fit_for_model
        // results for the same 1920x1080 input.
        let anthropic = ImageLimits {
            max_long_edge: ANTHROPIC_STANDARD_MAX_LONG_EDGE,
            max_pixels: ANTHROPIC_STANDARD_MAX_PIXELS,
        };
        let openai = ImageLimits {
            max_long_edge: OPENAI_TILE_MAX_LONG_EDGE,
            max_pixels: OPENAI_TILE_MAX_PIXELS,
        };
        let user_max_edge = 4000; // loose enough that both providers bind first
        let (a_edge, a_pixels) = resolve_limits(Some(anthropic), user_max_edge);
        let (o_edge, o_pixels) = resolve_limits(Some(openai), user_max_edge);
        let a_fit = fit_for_model(1920, 1080, a_edge, a_pixels);
        let o_fit = fit_for_model(1920, 1080, o_edge, o_pixels);
        assert_ne!(
            a_fit, o_fit,
            "switching provider must change the downscale target"
        );
    }

    #[test]
    fn landscape_downscales_to_long_edge() {
        assert_eq!(fit_long_edge(1920, 1080, 1568), (1568, 882));
    }

    #[test]
    fn portrait_downscales_to_long_edge() {
        assert_eq!(fit_long_edge(1080, 1920, 1568), (882, 1568));
    }

    #[test]
    fn square_downscales_evenly() {
        assert_eq!(fit_long_edge(2000, 2000, 1000), (1000, 1000));
    }

    #[test]
    fn already_smaller_is_unchanged() {
        assert_eq!(fit_long_edge(800, 600, 1568), (800, 600));
    }

    #[test]
    fn exactly_max_edge_is_unchanged() {
        assert_eq!(fit_long_edge(1568, 800, 1568), (1568, 800));
    }

    #[test]
    fn never_upscales() {
        // Long edge already well under max_edge: dimensions pass through.
        assert_eq!(fit_long_edge(100, 50, 4000), (100, 50));
    }

    #[test]
    fn extreme_wide_aspect_avoids_zero_short_edge() {
        // Naive rounding of the short edge would floor to 0; must clamp to 1.
        let (w, h) = fit_long_edge(100_000, 1, 50);
        assert_eq!(w, 50);
        assert_eq!(h, 1);
    }

    #[test]
    fn extreme_tall_aspect_avoids_zero_short_edge() {
        let (w, h) = fit_long_edge(1, 100_000, 50);
        assert_eq!(w, 1);
        assert_eq!(h, 50);
    }

    #[test]
    fn max_edge_zero_never_returns_zero_dimension() {
        let (w, h) = fit_long_edge(1920, 1080, 0);
        assert!(w >= 1);
        assert!(h >= 1);
    }

    #[test]
    fn max_edge_one_never_returns_zero_dimension() {
        let (w, h) = fit_long_edge(1920, 1080, 1);
        assert!(w >= 1);
        assert!(h >= 1);

        let (w, h) = fit_long_edge(3, 1, 1);
        assert!(w >= 1);
        assert!(h >= 1);
    }

    #[test]
    fn zero_size_input_never_returns_zero_dimension() {
        let (w, h) = fit_long_edge(0, 0, 1568);
        assert!(w >= 1);
        assert!(h >= 1);
    }

    #[test]
    fn aspect_ratio_preserved_within_rounding() {
        let (w, h) = fit_long_edge(4000, 3000, 1000);
        // 4:3 in, 4:3 out (within integer rounding).
        assert_eq!(w, 1000);
        assert!((h as i64 - 750).abs() <= 1);
    }

    // -- fit_for_model ------------------------------------------------

    #[test]
    fn fit_for_model_aspect_ratio_preserved_within_rounding() {
        // 4:3 in, both limits loose enough that only the long edge binds.
        let (w, h) = fit_for_model(4000, 3000, 1000, 10_000_000);
        assert_eq!(w, 1000);
        assert!((h as i64 - 750).abs() <= 1);
    }

    #[test]
    fn fit_for_model_never_upscales() {
        let (w, h) = fit_for_model(100, 50, 4000, 10_000_000);
        assert_eq!((w, h), (100, 50));
    }

    #[test]
    fn fit_for_model_long_edge_binds_for_elongated_aspect() {
        // A very wide image has few total pixels once fit to the long edge,
        // so a generous pixel budget never engages: behaves like
        // fit_long_edge alone.
        let (w, h) = fit_for_model(4000, 100, 1000, 10_000_000);
        assert_eq!((w, h), fit_long_edge(4000, 100, 1000));
    }

    #[test]
    fn fit_for_model_pixel_cap_binds_for_near_square_aspect() {
        // A near-square image hits the pixel/token budget well before its
        // long edge reaches the long-edge limit -- this is the case
        // fit_long_edge alone cannot express. 2000x2000 fit to long edge
        // 1568 is (1568, 1568) = ~2.46 megapixels, over a 1.23 megapixel
        // budget, so the pixel cap must shrink it further.
        let (w, h) = fit_for_model(2000, 2000, 1568, 1_229_312);
        let long_edge_only = fit_long_edge(2000, 2000, 1568);
        assert!(
            (w as u64) * (h as u64) <= 1_229_312,
            "pixel budget exceeded: {w}x{h}"
        );
        assert!(
            w < long_edge_only.0,
            "pixel cap should shrink below the long-edge-only fit"
        );
    }

    #[test]
    fn fit_for_model_matches_anthropic_standard_tier_shape() {
        // MEASURED 2026-09-17 from
        // https://platform.claude.com/docs/en/build-with-claude/vision:
        // a 1920x1080 screenshot on the standard tier (long edge 1568,
        // visual-token budget 1568) is downsized to 1456x819 (1560 tokens)
        // -- the pixel/token budget binds before the 1568 long-edge limit
        // does, since 1456 < 1568. A long-edge-only fit (the previous
        // behaviour) would send 1568x882, which is larger than the
        // provider will ever bill for.
        let (w, h) = fit_for_model(
            1920,
            1080,
            ANTHROPIC_STANDARD_MAX_LONG_EDGE,
            ANTHROPIC_STANDARD_MAX_PIXELS,
        );
        let long_edge_only = fit_long_edge(1920, 1080, ANTHROPIC_STANDARD_MAX_LONG_EDGE);
        assert!(
            (w as u64) * (h as u64) <= ANTHROPIC_STANDARD_MAX_PIXELS,
            "exceeded Anthropic's standard-tier pixel budget: {w}x{h}"
        );
        assert!(
            w < long_edge_only.0 && h < long_edge_only.1,
            "expected the pixel cap to bind tighter than the long-edge-only fit \
             ({w}x{h} vs {:?})",
            long_edge_only
        );
    }

    #[test]
    fn fit_for_model_odd_sizes() {
        let (w, h) = fit_for_model(4001, 2999, 1567, 1_000_003);
        assert!(w >= 1 && h >= 1);
        assert!((w as u64) * (h as u64) <= 1_000_003);
        assert!(w <= 1567);
    }

    #[test]
    fn fit_for_model_tiny_image_unchanged() {
        let (w, h) = fit_for_model(4, 3, 1568, ANTHROPIC_STANDARD_MAX_PIXELS);
        assert_eq!((w, h), (4, 3));
    }

    #[test]
    fn fit_for_model_zero_max_pixels_never_zero_dimension() {
        let (w, h) = fit_for_model(1920, 1080, 1568, 0);
        assert!(w >= 1);
        assert!(h >= 1);
    }

    #[test]
    fn fit_for_model_zero_max_long_edge_never_zero_dimension() {
        let (w, h) = fit_for_model(1920, 1080, 0, 0);
        assert!(w >= 1);
        assert!(h >= 1);
    }

    #[test]
    fn fit_for_model_anthropic_high_res_tier_allows_larger_images_than_standard() {
        // Claude 4.7+'s high-resolution tier should never produce a smaller
        // image than the standard tier for the same input -- otherwise the
        // constants would be backwards.
        let input = (3840, 2160);
        let standard = fit_for_model(
            input.0,
            input.1,
            ANTHROPIC_STANDARD_MAX_LONG_EDGE,
            ANTHROPIC_STANDARD_MAX_PIXELS,
        );
        let high_res = fit_for_model(
            input.0,
            input.1,
            ANTHROPIC_HIGH_RES_MAX_LONG_EDGE,
            ANTHROPIC_HIGH_RES_MAX_PIXELS,
        );
        assert!(
            (high_res.0 as u64) * (high_res.1 as u64) > (standard.0 as u64) * (standard.1 as u64),
            "high-res tier {high_res:?} should exceed standard tier {standard:?}"
        );
        assert!((high_res.0 as u64) * (high_res.1 as u64) <= ANTHROPIC_HIGH_RES_MAX_PIXELS);
    }

    #[test]
    fn fit_for_model_openai_tile_first_stage_fits_2048_square() {
        let (w, h) = fit_for_model(
            3840,
            2160,
            OPENAI_TILE_MAX_LONG_EDGE,
            OPENAI_TILE_MAX_PIXELS,
        );
        assert!(w <= OPENAI_TILE_MAX_LONG_EDGE && h <= OPENAI_TILE_MAX_LONG_EDGE);
        assert!((w as u64) * (h as u64) <= OPENAI_TILE_MAX_PIXELS);
    }

    // -- encode_png -----------------------------------------------------

    /// A non-uniform pattern: real screenshots are not flat, so a codec
    /// setting that only helps on solid colour would not show up here.
    fn checkerboard_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let on = ((x / 8) + (y / 8)) % 2 == 0;
                let v: u8 = if on { 255 } else { 0 };
                buf.extend_from_slice(&[v, v.wrapping_add(40), v.wrapping_add(80), 255]);
            }
        }
        buf
    }

    #[test]
    fn encode_png_roundtrips_dimensions() {
        let (w, h) = (64, 48);
        let rgba = checkerboard_rgba(w, h);
        let png_bytes = encode_png(&rgba, w, h).expect("encode");

        let decoded = image::load_from_memory(&png_bytes).expect("decode");
        assert_eq!(decoded.width(), w);
        assert_eq!(decoded.height(), h);
    }

    #[test]
    fn encode_png_default_compression_not_larger_than_codec_fast_default() {
        let (w, h) = (256, 256);
        let rgba = checkerboard_rgba(w, h);

        let ours = encode_png(&rgba, w, h).expect("encode");

        // Reproduce the codec's own out-of-the-box default (Fast) directly
        // to compare against -- this is the setting encode_png replaced
        // before issue #177 moved it again, Best -> Default.
        let mut fast = Cursor::new(Vec::new());
        image::codecs::png::PngEncoder::new(&mut fast)
            .write_image(&rgba, w, h, image::ExtendedColorType::Rgba8)
            .expect("encode fast");
        let fast = fast.into_inner();

        assert!(
            ours.len() <= fast.len(),
            "encode_png's output ({} bytes) should not exceed codec-default Fast ({} bytes)",
            ours.len(),
            fast.len()
        );
        // On a patterned (non-flat) image, Default should meaningfully beat
        // Fast, not just tie -- otherwise the setting isn't doing anything.
        assert!(
            ours.len() < fast.len(),
            "expected encode_png's output ({} bytes) to beat Fast ({} bytes) on a patterned image",
            ours.len(),
            fast.len()
        );
    }

    // -- RawShot / grab_raw / encode split (issue #177) ------------------

    #[test]
    fn encode_turns_a_raw_shot_into_a_valid_png_with_matching_dimensions() {
        let (w, h) = (64, 48);
        let raw = RawShot {
            rgba: checkerboard_rgba(w, h),
            width: w,
            height: h,
        };
        let shot = encode(&raw).expect("encode");
        assert_eq!(shot.width, w);
        assert_eq!(shot.height, h);

        let decoded = image::load_from_memory(&shot.png).expect("decode");
        assert_eq!(decoded.width(), w);
        assert_eq!(decoded.height(), h);
    }

    // -- pick_compression_level (issue #177) ------------------------------

    use image::codecs::png::CompressionType;

    /// A 20 Mbit/s uplink, the bandwidth `encode_png`'s doc comment and
    /// `bench_png_compression_levels` assume for the encode-plus-upload
    /// tradeoff (issue #177 names this explicitly: "assume a typical 20
    /// Mbit uplink and state it").
    const ASSUMED_UPLINK_BITS_PER_SEC: f64 = 20_000_000.0;

    // MEASURED 2026-09-17: `cargo test --release
    // capture::tests::bench_png_compression_levels -- --ignored --nocapture`,
    // this crate's `opt-level = "z"` + LTO release profile, median of 5 runs,
    // synthetic UI-like image at 1402x876 (the size `fit_for_model` gives a
    // 16:9 screen at the default `max_edge` 1568). Bytes are the printed KiB
    // (rounded to 1 decimal by that test) converted back to bytes, which is
    // precise enough for this decision -- see `bench_png_compression_levels`
    // for the exact reproduction command and full table (both sizes).
    const MEASURED_1402X876_FAST_MS: f64 = 16.1;
    const MEASURED_1402X876_FAST_BYTES: usize = 78_438; // 76.6 KiB
    const MEASURED_1402X876_DEFAULT_MS: f64 = 23.5;
    const MEASURED_1402X876_DEFAULT_BYTES: usize = 8_397; // 8.2 KiB
    const MEASURED_1402X876_BEST_MS: f64 = 25.2;
    const MEASURED_1402X876_BEST_BYTES: usize = 8_397; // 8.2 KiB (same rounded value as Default)

    #[test]
    fn pick_compression_level_prefers_lower_total_time_at_a_slow_uplink() {
        // A slow uplink (1 Mbit/s = 125 bytes/ms) makes the byte-count
        // difference dominate: the smallest candidate should win even
        // though it costs more to encode.
        let candidates = [
            (CompressionType::Fast, 10.0, 200_000usize),
            (CompressionType::Default, 30.0, 120_000usize),
            (CompressionType::Best, 100.0, 118_000usize),
        ];
        assert_eq!(
            pick_compression_level(&candidates, 1_000_000.0),
            CompressionType::Default
        );
    }

    #[test]
    fn pick_compression_level_prefers_faster_encode_at_a_very_fast_uplink() {
        // An extremely fast uplink makes bytes nearly free, so the
        // candidate with the least encode time should win even though it
        // produces the most bytes.
        let candidates = [
            (CompressionType::Fast, 10.0, 200_000usize),
            (CompressionType::Default, 30.0, 120_000usize),
            (CompressionType::Best, 100.0, 118_000usize),
        ];
        assert_eq!(
            pick_compression_level(&candidates, 100_000_000_000.0),
            CompressionType::Fast
        );
    }

    #[test]
    fn pick_compression_level_matches_this_module_s_actual_choice_at_20_mbit() {
        // The tradeoff this module's own `encode_png` doc comment cites,
        // using the real MEASURED bench_png_compression_levels numbers for
        // the 1402x876 case (see that test) -- proves the shipped
        // CompressionType::Default in encode_png is the one the decision
        // function actually picks, not a value chosen by eyeballing the
        // table and then hard-coded independently of it.
        let candidates = [
            (
                CompressionType::Fast,
                MEASURED_1402X876_FAST_MS,
                MEASURED_1402X876_FAST_BYTES,
            ),
            (
                CompressionType::Default,
                MEASURED_1402X876_DEFAULT_MS,
                MEASURED_1402X876_DEFAULT_BYTES,
            ),
            (
                CompressionType::Best,
                MEASURED_1402X876_BEST_MS,
                MEASURED_1402X876_BEST_BYTES,
            ),
        ];
        assert_eq!(
            pick_compression_level(&candidates, ASSUMED_UPLINK_BITS_PER_SEC),
            CompressionType::Default
        );
    }

    // -- bench_png_compression_levels (issue #177) ------------------------
    //
    // A synthetic UI-like image: alternating flat background bands (window
    // chrome / panels) with dense rows of high-contrast strokes (glyph-like
    // text), at the two sizes `fit_for_model` actually produces for a
    // typical screen at the default `max_edge` (1402x876 for 16:9 at 1568,
    // 1568x980 for 16:10). A flat fill or the `checkerboard_rgba` used by
    // the roundtrip tests above is too easy for the codec and would not
    // show a realistic gap between compression levels.
    fn synthetic_ui_image(w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
        let text_row_period = 18;
        let text_row_height = 10;
        // "Text" runs across about 70% of the row width, in short dashes
        // like glyph strokes rather than one solid bar.
        let text_extent = (w as u64 * 7 / 10).max(1) as u32;
        for y in 0..h {
            let band = (y / 40) % 2;
            let bg: u8 = if band == 0 { 245 } else { 235 };
            let in_text_row = (y % text_row_period) < text_row_height;
            for x in 0..w {
                let idx = ((y * w + x) * 4) as usize;
                let glyph_on = in_text_row && x < text_extent && ((x / 3) % 4) < 2;
                let v: u8 = if glyph_on { 20 } else { bg };
                buf[idx] = v;
                buf[idx + 1] = v;
                buf[idx + 2] = v;
                buf[idx + 3] = 255;
            }
        }
        buf
    }

    /// Median encode time (ms) and output size (bytes) for `compression`
    /// over several runs, to smooth scheduler noise -- same idea as issue
    /// #177's own scratch-crate measurement (median of 7).
    fn median_encode(
        rgba: &[u8],
        w: u32,
        h: u32,
        compression: CompressionType,
        runs: usize,
    ) -> (f64, usize) {
        let mut ms_samples = Vec::with_capacity(runs);
        let mut bytes = 0usize;
        for _ in 0..runs {
            let mut out = Cursor::new(Vec::new());
            let start = std::time::Instant::now();
            image::codecs::png::PngEncoder::new_with_quality(
                &mut out,
                compression,
                image::codecs::png::FilterType::Adaptive,
            )
            .write_image(rgba, w, h, image::ExtendedColorType::Rgba8)
            .expect("encode");
            ms_samples.push(start.elapsed().as_secs_f64() * 1000.0);
            bytes = out.into_inner().len();
        }
        ms_samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
        (ms_samples[ms_samples.len() / 2], bytes)
    }

    /// Run manually in release, once (AGENTS.md build rules): from the crate
    /// root,
    /// ```text
    /// $env:CARGO_TARGET_DIR = "...\target\wt\<worktree>"; $env:RUSTC_WRAPPER = "sccache"; $env:CARGO_BUILD_JOBS = "2"
    /// cargo test --release capture::tests::bench_png_compression_levels -- --ignored --nocapture
    /// ```
    /// Prints a `MEASURED 2026-09-17:` line per size/level; the numbers this
    /// module's `encode_png` doc comment and the `MEASURED_1402X876_*`
    /// constants above cite came from one such run.
    #[test]
    #[ignore = "release-only timing benchmark; run manually, see this test's doc comment"]
    fn bench_png_compression_levels() {
        for (w, h) in [(1402u32, 876u32), (1568u32, 980u32)] {
            let rgba = synthetic_ui_image(w, h);
            for (name, compression) in [
                ("Fast", CompressionType::Fast),
                ("Default", CompressionType::Default),
                ("Best", CompressionType::Best),
            ] {
                let (ms, bytes) = median_encode(&rgba, w, h, compression, 5);
                println!(
                    "MEASURED 2026-09-17: {w}x{h} {name}: {:.1} ms, {:.1} KiB",
                    ms,
                    bytes as f64 / 1024.0
                );
            }
        }
    }

    // -- blit_into (issue #29's virtual-desktop compositing) -------------

    fn solid_rgba(w: u32, h: u32, px: [u8; 4]) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for _ in 0..(w * h) {
            buf.extend_from_slice(&px);
        }
        buf
    }

    /// Byte offset of pixel `(col, row)` in a `width`-wide RGBA8 buffer.
    fn px_idx(row: u32, col: u32, width: u32) -> usize {
        ((row * width + col) * 4) as usize
    }

    #[test]
    fn blit_into_places_src_at_a_positive_offset() {
        let mut dst = vec![0u8; 4 * 4 * 4]; // 4x4, transparent black
        let src = solid_rgba(2, 2, [10, 20, 30, 255]);
        blit_into(&mut dst, 4, 4, &src, 2, 2, 1, 1);

        // Pixel (1,1) in dst should now be the src colour.
        let idx = px_idx(1, 1, 4);
        assert_eq!(&dst[idx..idx + 4], &[10, 20, 30, 255]);
        // Pixel (0,0) untouched.
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
        // Pixel (3,3) (outside the 2x2 src placed at (1,1)-(3,3)) untouched.
        let idx33 = px_idx(3, 3, 4);
        assert_eq!(&dst[idx33..idx33 + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_into_clips_a_negative_offset() {
        // src is 4x4 placed at (-2,-2): only its bottom-right 2x2 quadrant
        // overlaps a 4x4 dst.
        let mut dst = vec![0u8; 4 * 4 * 4];
        let mut src = vec![0u8; 4 * 4 * 4];
        // Mark the bottom-right 2x2 of src (rows/cols 2..4) distinctly.
        for y in 2..4u32 {
            for x in 2..4u32 {
                let idx = px_idx(y, x, 4);
                src[idx..idx + 4].copy_from_slice(&[7, 8, 9, 255]);
            }
        }
        blit_into(&mut dst, 4, 4, &src, 4, 4, -2, -2);

        // dst (0,0) should now hold what was src (2,2).
        assert_eq!(&dst[0..4], &[7, 8, 9, 255]);
        // dst (1,1) should hold src (3,3), still inside the marked quadrant.
        let idx11 = px_idx(1, 1, 4);
        assert_eq!(&dst[idx11..idx11 + 4], &[7, 8, 9, 255]);
        // dst (2,2) is past the clipped 2x2 overlap; must stay untouched.
        let idx22 = px_idx(2, 2, 4);
        assert_eq!(&dst[idx22..idx22 + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_into_clips_an_offset_past_the_far_edge() {
        let mut dst = vec![0u8; 4 * 4 * 4];
        let src = solid_rgba(4, 4, [1, 2, 3, 255]);
        // Placed almost entirely off the right/bottom edge: only dst's
        // (3,3) corner should be touched.
        blit_into(&mut dst, 4, 4, &src, 4, 4, 3, 3);

        let idx33 = ((3 * 4 + 3) * 4) as usize;
        assert_eq!(&dst[idx33..idx33 + 4], &[1, 2, 3, 255]);
        assert_eq!(&dst[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn blit_into_wholly_outside_dst_is_a_no_op() {
        let mut dst = vec![9u8; 4 * 4 * 4];
        let before = dst.clone();
        let src = solid_rgba(2, 2, [1, 2, 3, 255]);
        blit_into(&mut dst, 4, 4, &src, 2, 2, 100, 100);
        assert_eq!(dst, before);
    }

    #[test]
    fn blit_into_zero_size_src_is_a_no_op() {
        let mut dst = vec![9u8; 4 * 4 * 4];
        let before = dst.clone();
        blit_into(&mut dst, 4, 4, &[], 0, 0, 0, 0);
        assert_eq!(dst, before);
    }

    // -- crop_rgba (issue #29: region capture) ----------------------------

    /// A distinct-per-pixel buffer (value = row*width + col) so a crop's
    /// exact placement and size are checkable, not just "some pixels
    /// copied".
    fn indexed_rgba(w: u32, h: u32) -> Vec<u8> {
        let mut buf = Vec::with_capacity((w as usize) * (h as usize) * 4);
        for y in 0..h {
            for x in 0..w {
                let v = (y * w + x) as u8;
                buf.extend_from_slice(&[v, v, v, 255]);
            }
        }
        buf
    }

    #[test]
    fn crop_rgba_extracts_exactly_the_requested_rectangle() {
        let (w, h) = (10u32, 10u32);
        let src = indexed_rgba(w, h);
        let rect = RectPx {
            x: 3,
            y: 2,
            w: 4,
            h: 3,
        };
        let cropped = crop_rgba(&src, w, h, rect);

        assert_eq!(cropped.width, 4);
        assert_eq!(cropped.height, 3);
        for row in 0..3u32 {
            for col in 0..4u32 {
                let src_v = ((2 + row) * w + (3 + col)) as u8;
                let idx = ((row * 4 + col) * 4) as usize;
                assert_eq!(
                    cropped.rgba[idx], src_v,
                    "mismatch at cropped ({col},{row})"
                );
            }
        }
    }

    #[test]
    fn crop_rgba_clamps_a_rect_that_overhangs_the_buffer() {
        let (w, h) = (5u32, 5u32);
        let src = indexed_rgba(w, h);
        let rect = RectPx {
            x: 3,
            y: 3,
            w: 10,
            h: 10,
        };
        let cropped = crop_rgba(&src, w, h, rect);
        assert_eq!(cropped.width, 2); // 5 - 3
        assert_eq!(cropped.height, 2);
    }

    #[test]
    fn crop_rgba_rect_entirely_past_the_buffer_still_returns_a_pixel() {
        let (w, h) = (5u32, 5u32);
        let src = indexed_rgba(w, h);
        let rect = RectPx {
            x: 50,
            y: 50,
            w: 10,
            h: 10,
        };
        let cropped = crop_rgba(&src, w, h, rect);
        assert_eq!(cropped.width, 1);
        assert_eq!(cropped.height, 1);
        assert_eq!(cropped.rgba.len(), 4);
    }

    #[test]
    fn crop_rgba_zero_size_source_never_panics() {
        let cropped = crop_rgba(
            &[],
            0,
            0,
            RectPx {
                x: 0,
                y: 0,
                w: 0,
                h: 0,
            },
        );
        assert_eq!(cropped.width, 1);
        assert_eq!(cropped.height, 1);
    }

    #[test]
    fn crop_rgba_full_buffer_roundtrips_unchanged() {
        let (w, h) = (6u32, 4u32);
        let src = indexed_rgba(w, h);
        let cropped = crop_rgba(&src, w, h, RectPx { x: 0, y: 0, w, h });
        assert_eq!(cropped.width, w);
        assert_eq!(cropped.height, h);
        assert_eq!(cropped.rgba, src);
    }
}
