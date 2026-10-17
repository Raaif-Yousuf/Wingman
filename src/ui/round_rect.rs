//! Smooth rounded rectangles for the GDI paint paths (#989).
//!
//! GDI's `RoundRect` has no anti-aliasing, so a corner shows steps at 200
//! percent and above. [`paint`] draws the same shape with coverage blended into
//! the pixels already on the device context: the shape's outline is a signed
//! distance, and a pixel's coverage is how far the edge sits inside it. Pure
//! maths ([`coverage`], [`blend`]) is tested; [`paint`] reads the rectangle
//! under the shape into a 32-bit bitmap, blends there and copies it back, so it
//! works on any device context and honours its clip region.

use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Gdi::{
    BitBlt, CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS, HDC, HGDIOBJ, SRCCOPY,
};

/// The largest side [`paint`] will blend, in pixels. A bigger shape (never a
/// control or a row) is skipped rather than allocating a huge bitmap.
const MAX_SIDE: i32 = 8192;

/// How much of the pixel centred at (`x`, `y`) lies inside a `w` by `h`
/// rounded rectangle with corner radius `r`, whose top-left corner is the
/// origin: 0.0 outside, 1.0 inside, in between on the edge.
pub(crate) fn coverage(x: f32, y: f32, w: f32, h: f32, r: f32) -> f32 {
    let r = r.clamp(0.0, w.min(h) / 2.0);
    // Signed distance to the outline, negative inside.
    let (px, py) = ((x - w / 2.0).abs(), (y - h / 2.0).abs());
    let (qx, qy) = (px - (w / 2.0 - r), py - (h / 2.0 - r));
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let d = outside + qx.max(qy).min(0.0) - r;
    (0.5 - d).clamp(0.0, 1.0)
}

/// `src` laid over `dst` at opacity `a` (0.0 to 1.0). Both are `0x00BBGGRR`.
pub(crate) fn blend(dst: u32, src: u32, a: f32) -> u32 {
    let a = a.clamp(0.0, 1.0);
    let mix = |shift: u32| {
        let d = ((dst >> shift) & 0xFF) as f32;
        let s = ((src >> shift) & 0xFF) as f32;
        (d + (s - d) * a).round() as u32 & 0xFF
    };
    mix(0) | (mix(8) << 8) | (mix(16) << 16)
}

/// Draws a rounded rectangle exactly covering `rect` (right and bottom
/// excluded) on `hdc`: filled with `fill`, and outlined inside its edge with
/// `stroke` (colour and width in pixels), either of which may be absent.
/// Edges are smoothed. Nothing is drawn if the shape is empty or unreasonably
/// large; every GDI object is freed on every path.
///
/// # Safety
/// `hdc` must be a live device context.
pub(crate) unsafe fn paint(
    hdc: HDC,
    rect: RECT,
    radius: i32,
    fill: Option<u32>,
    stroke: Option<(u32, i32)>,
) {
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w <= 0 || h <= 0 || w > MAX_SIDE || h > MAX_SIDE || (fill.is_none() && stroke.is_none()) {
        return;
    }
    let mem = CreateCompatibleDC(Some(hdc));
    if mem.is_invalid() {
        return;
    }
    let info = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h, // top-down
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        },
        ..Default::default()
    };
    let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
    let Ok(dib) = CreateDIBSection(Some(mem), &info, DIB_RGB_COLORS, &mut bits, None, 0) else {
        let _ = DeleteDC(mem);
        return;
    };
    if bits.is_null() {
        let _ = DeleteObject(HGDIOBJ(dib.0));
        let _ = DeleteDC(mem);
        return;
    }
    let old = SelectObject(mem, HGDIOBJ(dib.0));
    // The pixels already there, so the edge blends into them.
    if BitBlt(mem, 0, 0, w, h, Some(hdc), rect.left, rect.top, SRCCOPY).is_ok() {
        // SAFETY: the section is `w * h` 32-bit pixels, top-down.
        let px = std::slice::from_raw_parts_mut(bits as *mut u32, (w as usize) * (h as usize));
        let (wf, hf, r) = (w as f32, h as f32, radius as f32);
        let inner = stroke.map(|(_, width)| {
            let width = width.max(0) as f32;
            (width, (r - width).max(0.0))
        });
        for y in 0..h {
            for x in 0..w {
                let (cx, cy) = (x as f32 + 0.5, y as f32 + 0.5);
                let outer = coverage(cx, cy, wf, hf, r);
                if outer <= 0.0 {
                    continue;
                }
                let i = (y as usize) * (w as usize) + x as usize;
                // The section's pixels are 0x00RRGGBB; COLORREF is 0x00BBGGRR.
                let mut c = swap_rb(px[i]);
                if let Some(fill) = fill {
                    c = blend(c, fill, outer);
                }
                if let (Some((color, _)), Some((width, ir))) = (stroke, inner) {
                    let inside = coverage(
                        cx - width,
                        cy - width,
                        wf - 2.0 * width,
                        hf - 2.0 * width,
                        ir,
                    );
                    c = blend(c, color, outer * (1.0 - inside));
                }
                px[i] = swap_rb(c);
            }
        }
        let _ = BitBlt(hdc, rect.left, rect.top, w, h, Some(mem), 0, 0, SRCCOPY);
    }
    SelectObject(mem, old);
    let _ = DeleteObject(HGDIOBJ(dib.0));
    let _ = DeleteDC(mem);
}

/// Swaps the red and blue bytes of a `0x00BBGGRR` / `0x00RRGGBB` colour.
fn swap_rb(c: u32) -> u32 {
    (c & 0x0000_FF00) | ((c & 0xFF) << 16) | ((c >> 16) & 0xFF)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Graphics::Gdi::{
        CreateCompatibleBitmap, CreateSolidBrush, FillRect, GetDC, GetPixel, ReleaseDC,
    };

    #[test]
    fn coverage_is_full_inside_empty_outside_and_partial_on_the_curve() {
        // A 20 by 20 shape with radius 8: the middle is in, the very corner out.
        assert_eq!(coverage(10.0, 10.0, 20.0, 20.0, 8.0), 1.0);
        assert_eq!(coverage(0.5, 0.5, 20.0, 20.0, 8.0), 0.0);
        // A straight edge pixel is whole; one on the curve is part covered.
        assert_eq!(coverage(10.0, 0.5, 20.0, 20.0, 8.0), 1.0);
        let on_curve = coverage(2.0, 2.5, 20.0, 20.0, 8.0);
        assert!(on_curve > 0.0 && on_curve < 1.0, "{on_curve}");
        // A radius of 0 is a plain rectangle, a huge one is a circle.
        assert_eq!(coverage(0.5, 0.5, 20.0, 20.0, 0.0), 1.0);
        assert_eq!(coverage(0.5, 0.5, 20.0, 20.0, 99.0), 0.0);
    }

    #[test]
    fn blend_mixes_each_channel_and_keeps_the_ends() {
        let (dst, src) = (0x0010_2030, 0x00F0_E0D0);
        assert_eq!(blend(dst, src, 0.0), dst);
        assert_eq!(blend(dst, src, 1.0), src);
        assert_eq!(blend(0, 0x00FF_FFFF, 0.5), 0x0080_8080);
    }

    #[test]
    fn red_and_blue_swap() {
        assert_eq!(swap_rb(0x0011_2233), 0x0033_2211);
    }

    /// A device context with a bitmap the size of the test shape, cleared to
    /// `bg`, to paint on and read back.
    fn with_surface(bg: u32, f: impl FnOnce(HDC)) {
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(Some(screen));
            let bmp = CreateCompatibleBitmap(screen, 40, 40);
            let old = SelectObject(mem, HGDIOBJ(bmp.0));
            let brush = CreateSolidBrush(windows::Win32::Foundation::COLORREF(bg));
            FillRect(
                mem,
                &RECT {
                    left: 0,
                    top: 0,
                    right: 40,
                    bottom: 40,
                },
                brush,
            );
            let _ = DeleteObject(HGDIOBJ(brush.0));
            f(mem);
            SelectObject(mem, old);
            let _ = DeleteObject(HGDIOBJ(bmp.0));
            let _ = DeleteDC(mem);
            ReleaseDC(None, screen);
        }
    }

    fn pixel(hdc: HDC, x: i32, y: i32) -> u32 {
        unsafe { GetPixel(hdc, x, y).0 }
    }

    #[test]
    fn a_painted_corner_is_smoothed_and_the_rest_is_exact() {
        let (bg, fill) = (0x0000_0000, 0x00FF_FFFF);
        with_surface(bg, |hdc| unsafe {
            let rect = RECT {
                left: 4,
                top: 4,
                right: 36,
                bottom: 36,
            };
            paint(hdc, rect, 8, Some(fill), None);
            assert_eq!(pixel(hdc, 20, 20), fill, "inside");
            assert_eq!(pixel(hdc, 20, 4), fill, "straight edge");
            assert_eq!(pixel(hdc, 2, 20), bg, "outside");
            assert_eq!(pixel(hdc, 4, 4), bg, "the very corner is empty");
            // Some pixel along the curve is neither colour: the steps are gone.
            let mixed = (0..8).any(|d| {
                let p = pixel(hdc, 4 + d, 4 + d);
                p != bg && p != fill
            });
            assert!(mixed, "no blended pixel on the corner curve");
        });
    }

    #[test]
    fn a_stroke_sits_inside_the_edge_and_leaves_the_fill_alone() {
        let (bg, fill, line) = (0x0000_0000, 0x0000_FF00, 0x00FF_0000);
        with_surface(bg, |hdc| unsafe {
            let rect = RECT {
                left: 4,
                top: 4,
                right: 36,
                bottom: 36,
            };
            paint(hdc, rect, 6, Some(fill), Some((line, 2)));
            assert_eq!(pixel(hdc, 20, 4), line, "outer row of the stroke");
            assert_eq!(pixel(hdc, 20, 5), line, "second row of the stroke");
            assert_eq!(pixel(hdc, 20, 6), fill, "fill starts inside the stroke");
            assert_eq!(pixel(hdc, 3, 20), bg, "nothing outside the rect");
        });
    }

    #[test]
    fn an_empty_or_huge_shape_draws_nothing_and_does_not_crash() {
        let bg = 0x0012_3456;
        with_surface(bg, |hdc| unsafe {
            let empty = RECT {
                left: 5,
                top: 5,
                right: 5,
                bottom: 9,
            };
            paint(hdc, empty, 4, Some(0), None);
            let huge = RECT {
                left: 0,
                top: 0,
                right: MAX_SIDE + 1,
                bottom: 10,
            };
            paint(hdc, huge, 4, Some(0), None);
            paint(
                hdc,
                RECT {
                    left: 0,
                    top: 0,
                    right: 10,
                    bottom: 10,
                },
                4,
                None,
                None,
            );
            assert_eq!(pixel(hdc, 6, 6), bg);
        });
    }
}
