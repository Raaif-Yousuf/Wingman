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

/// Capture the target monitor (per `monitor_mode`), downscale so the long
/// edge is `max_edge`, and encode as PNG.
///
/// `monitor_mode` is `"active"` (the monitor under the foreground window) or
/// `"primary"` (always the system's primary monitor). Anything else is
/// treated as `"active"`.
pub fn grab(monitor_mode: &str, max_edge: u32) -> Result<Shot> {
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
    let (nw, nh) = fit_long_edge(w, h, max_edge);

    let resized = if (nw, nh) == (w, h) {
        image
    } else {
        imageops::resize(&image, nw, nh, FilterType::Lanczos3)
    };

    let (final_w, final_h) = (resized.width(), resized.height());

    let mut png = Cursor::new(Vec::new());
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            resized.as_raw(),
            final_w,
            final_h,
            image::ExtendedColorType::Rgba8,
        )
        .context("encoding PNG")?;

    Ok(Shot {
        png: png.into_inner(),
        width: final_w,
        height: final_h,
    })
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
    use super::fit_long_edge;

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
}
