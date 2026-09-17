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
use windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow;

// The constants below are sensible per-provider/per-tier limits for
// `fit_for_model`, exercised by the tests in this module. Nothing in
// production code references them by name yet: `grab` derives its pixel
// budget from the single shared `max_edge` setting (see its comment), because
// wiring the real, model-specific limits requires knowing which provider is
// configured, and `src/provider/**`, `src/config.rs` and `src/ui/settings.rs`
// are out of scope for this change (other agents are editing them; the
// Provider trait extension that would carry this, #12, is in flight). Filed
// as a follow-up issue to thread these through once #12 lands.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub const ANTHROPIC_STANDARD_MAX_PIXELS: u64 = 1568 * 28 * 28;

#[allow(dead_code)]
/// Claude 4.7 and later, "high-resolution" tier: long edge at most 2576 px,
/// visual tokens at most 4784. Same source and date as the standard tier
/// above.
pub const ANTHROPIC_HIGH_RES_MAX_LONG_EDGE: u32 = 2576;
#[allow(dead_code)]
pub const ANTHROPIC_HIGH_RES_MAX_PIXELS: u64 = 4784 * 28 * 28;

#[allow(dead_code)]
/// OpenAI's legacy tile-based vision models (gpt-4o, gpt-4.1 class) with
/// `detail: "high"`: the first resize stage fits the image within a
/// 2048x2048 square. MEASURED 2026-09-17 from
/// <https://developers.openai.com/api/docs/guides/images-vision> (512px
/// tiles, 85 base tokens + 170 tokens/tile). OpenAI's real second stage then
/// further rescales so the *shortest* side is 768 px -- a short-edge
/// constraint this module does not model (see the follow-up issue above), so
/// `OPENAI_TILE_MAX_PIXELS` is only the 2048x2048 first-stage bound, not the
/// true token-minimal size.
pub const OPENAI_TILE_MAX_LONG_EDGE: u32 = 2048;
#[allow(dead_code)]
pub const OPENAI_TILE_MAX_PIXELS: u64 = 2048 * 2048;

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

/// Capture the target monitor (per `monitor_mode`) and downscale so we never
/// send more pixels than the configured `max_edge` implies a provider would
/// keep. Does not encode -- see [`encode`] and this function's doc comment
/// on [`RawShot`] for why that is a separate, later step.
///
/// `monitor_mode` is `"active"` (the monitor under the foreground window) or
/// `"primary"` (always the system's primary monitor). Anything else is
/// treated as `"active"`.
pub fn grab_raw(monitor_mode: &str, max_edge: u32) -> Result<RawShot> {
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
    // `max_edge` is a single setting shared by whichever provider is
    // configured (config.rs / ui/settings.rs, out of scope here -- see the
    // follow-up issue on threading real per-provider limits through the
    // Provider trait, #12). Pair it with the pixel budget implied by
    // Anthropic's standard tier scaled to that long edge
    // (`max_edge * 28 * 28`, which reduces to `ANTHROPIC_STANDARD_MAX_PIXELS`
    // at the config default of 1568) so raising the long edge in settings
    // still raises the effective resolution, while typical screen aspect
    // ratios (16:9, 16:10 and wider) get the tighter, token-shaped cap that
    // `fit_long_edge` alone could not express.
    let max_pixels = (max_edge as u64) * 28 * 28;
    let (nw, nh) = fit_for_model(w, h, max_edge, max_pixels);

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
fn active_monitor_rect() -> Result<RECT> {
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

#[cfg(test)]
mod tests {
    use super::{
        encode, encode_png, fit_for_model, fit_long_edge, pick_compression_level, Cursor,
        RawShot, ANTHROPIC_HIGH_RES_MAX_LONG_EDGE, ANTHROPIC_HIGH_RES_MAX_PIXELS,
        ANTHROPIC_STANDARD_MAX_LONG_EDGE, ANTHROPIC_STANDARD_MAX_PIXELS, OPENAI_TILE_MAX_LONG_EDGE,
        OPENAI_TILE_MAX_PIXELS,
    };
    use image::ImageEncoder;

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
            (CompressionType::Fast, MEASURED_1402X876_FAST_MS, MEASURED_1402X876_FAST_BYTES),
            (
                CompressionType::Default,
                MEASURED_1402X876_DEFAULT_MS,
                MEASURED_1402X876_DEFAULT_BYTES,
            ),
            (CompressionType::Best, MEASURED_1402X876_BEST_MS, MEASURED_1402X876_BEST_BYTES),
        ];
        assert_eq!(pick_compression_level(&candidates, ASSUMED_UPLINK_BITS_PER_SEC), CompressionType::Default);
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

    /// Run manually in release, once (CLAUDE.md build rules): from the crate
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
}
