//! The alpha pass (spec "Drawing, and the alpha trap"). GDI text drawn into a
//! 32-bit DIB writes alpha 0, so on a per-pixel-alpha layered window the label
//! text would be invisible. The overlay draws every label as an opaque pill
//! with GDI and then runs this pass over the pill rectangles: alpha becomes 255
//! inside the pill, colour is premultiplied, and the corners get a soft edge
//! generated here (not by GDI). It is the one place a wrong byte order would
//! make the overlay vanish without an error, so it is pure and tested.
//!
//! The buffer is what `UpdateLayeredWindow` takes: 32 bits per pixel, bytes
//! B, G, R, A, rows top to bottom, no padding.

use crate::capture::ScreenRect;

/// One filled shape to finish: where it is in the buffer (buffer pixels, so
/// relative to the window, not the screen) and how round its corners are.
/// Radius 0 is a plain rect, which is what marks and halos use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pill {
    pub rect: ScreenRect,
    pub radius: i32,
}

/// Finishes every pill in `buf`, a `width` x `height` BGRA bitmap. Pixels
/// outside every pill are untouched; a pill partly outside the bitmap is
/// clipped. Returns `false`, touching nothing, if the buffer is not exactly
/// `width * height * 4` bytes (a mismatch would otherwise write in the wrong
/// place).
pub fn finish_pills(buf: &mut [u8], width: u32, height: u32, pills: &[Pill]) -> bool {
    let (w, h) = (width as usize, height as usize);
    if w.checked_mul(h).and_then(|n| n.checked_mul(4)) != Some(buf.len()) {
        return false;
    }
    let source = buf.to_vec();
    for pill in pills {
        let r = pill.rect;
        if r.left >= r.right || r.top >= r.bottom {
            continue;
        }
        let x0 = i64::from(r.left).max(0) as usize;
        let y0 = i64::from(r.top).max(0) as usize;
        let x1 = (i64::from(r.right).max(0) as usize).min(w);
        let y1 = (i64::from(r.bottom).max(0) as usize).min(h);
        let (pw, ph) = (
            (i64::from(r.right) - i64::from(r.left)) as f32,
            (i64::from(r.bottom) - i64::from(r.top)) as f32,
        );
        let radius = (pill.radius.max(0) as f32).min(pw.min(ph) / 2.0);
        let (cx, cy) = (
            i64::from(r.left) as f32 + pw / 2.0,
            i64::from(r.top) as f32 + ph / 2.0,
        );
        for y in y0..y1 {
            for x in x0..x1 {
                let cover = coverage(x as f32 + 0.5 - cx, y as f32 + 0.5 - cy, pw, ph, radius);
                let i = (y * w + x) * 4;
                // Overlapping pills: the more opaque one wins, and colour is
                // always premultiplied from the pixel GDI drew, never from a
                // value an earlier pill already scaled.
                let a = ((cover * 255.0).round() as u32).max(u32::from(buf[i + 3]));
                for k in 0..3 {
                    buf[i + k] = ((u32::from(source[i + k]) * a + 127) / 255) as u8;
                }
                buf[i + 3] = a as u8;
            }
        }
    }
    true
}

/// How much of the pixel at offset `(dx, dy)` from the pill's centre is inside
/// a `w` x `h` rounded rect of corner `radius`: 1 inside, 0 outside, a ramp one
/// pixel wide across the edge.
fn coverage(dx: f32, dy: f32, w: f32, h: f32, radius: f32) -> f32 {
    let qx = dx.abs() - (w / 2.0 - radius);
    let qy = dy.abs() - (h / 2.0 - radius);
    let outside = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt();
    let distance = outside + qx.max(qy).min(0.0) - radius;
    (0.5 - distance).clamp(0.0, 1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const W: u32 = 20;
    const H: u32 = 12;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// A bitmap as GDI leaves it after drawing: colour set, alpha 0.
    fn gdi_bitmap() -> Vec<u8> {
        let mut b = vec![0u8; (W * H * 4) as usize];
        for px in b.chunks_exact_mut(4) {
            px.copy_from_slice(&[200, 100, 50, 0]); // B, G, R, A
        }
        b
    }

    fn px(b: &[u8], x: u32, y: u32) -> [u8; 4] {
        let i = ((y * W + x) * 4) as usize;
        [b[i], b[i + 1], b[i + 2], b[i + 3]]
    }

    fn pill(left: i32, top: i32, right: i32, bottom: i32, radius: i32) -> Pill {
        Pill {
            rect: rect(left, top, right, bottom),
            radius,
        }
    }

    #[test]
    fn every_pixel_inside_a_square_pill_becomes_opaque_with_its_colour_kept() {
        let mut b = gdi_bitmap();
        assert!(finish_pills(&mut b, W, H, &[pill(2, 2, 10, 8, 0)]));
        for y in 2..8 {
            for x in 2..10 {
                // Opaque, so premultiplied colour equals the colour.
                assert_eq!(px(&b, x, y), [200, 100, 50, 255], "({x},{y})");
            }
        }
    }

    #[test]
    fn pixels_outside_every_pill_are_untouched() {
        let mut b = gdi_bitmap();
        finish_pills(&mut b, W, H, &[pill(2, 2, 10, 8, 3)]);
        for y in 0..H {
            for x in 0..W {
                let inside = (2..10).contains(&x) && (2..8).contains(&y);
                if !inside {
                    assert_eq!(px(&b, x, y), [200, 100, 50, 0], "({x},{y})");
                }
            }
        }
    }

    #[test]
    fn rounded_corners_fade_and_stay_premultiplied() {
        let mut b = gdi_bitmap();
        finish_pills(&mut b, W, H, &[pill(2, 2, 18, 10, 3)]);
        // The very corner pixel is outside the curve.
        assert_eq!(px(&b, 2, 2)[3], 0);
        assert_eq!(px(&b, 2, 2)[..3], [0, 0, 0]);
        // Somewhere along the curve there is a partial pixel, and for every
        // pixel no colour channel exceeds alpha (the premultiplied rule).
        let mut partial = false;
        for y in 0..H {
            for x in 0..W {
                let p = px(&b, x, y);
                if p[3] > 0 && p[3] < 255 {
                    partial = true;
                }
                if (2..18).contains(&x) && (2..10).contains(&y) {
                    assert!(p[0] <= p[3] && p[1] <= p[3] && p[2] <= p[3], "({x},{y})");
                }
            }
        }
        assert!(partial);
        // The middle of every edge is fully opaque.
        assert_eq!(px(&b, 10, 2)[3], 255);
        assert_eq!(px(&b, 2, 6)[3], 255);
        assert_eq!(px(&b, 17, 6)[3], 255);
        assert_eq!(px(&b, 10, 9)[3], 255);
    }

    #[test]
    fn a_partial_pixel_scales_each_channel_by_its_alpha() {
        let mut b = gdi_bitmap();
        finish_pills(&mut b, W, H, &[pill(2, 2, 18, 10, 4)]);
        let p = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .map(|(x, y)| px(&b, x, y))
            .find(|p| p[3] > 0 && p[3] < 255)
            .unwrap();
        let a = u32::from(p[3]);
        assert_eq!(u32::from(p[0]), (200 * a + 127) / 255);
        assert_eq!(u32::from(p[1]), (100 * a + 127) / 255);
        assert_eq!(u32::from(p[2]), (50 * a + 127) / 255);
    }

    #[test]
    fn overlapping_pills_never_lose_opacity_or_double_premultiply() {
        // B's transparent corner (4,4) lies inside A's interior.
        let mut b = gdi_bitmap();
        finish_pills(
            &mut b,
            W,
            H,
            &[pill(2, 2, 18, 10, 0), pill(4, 4, 16, 12, 4)],
        );
        assert_eq!(
            px(&b, 4, 4),
            [200, 100, 50, 255],
            "corner over an opaque pixel"
        );
        // Order must not matter.
        let mut c = gdi_bitmap();
        finish_pills(
            &mut c,
            W,
            H,
            &[pill(4, 4, 16, 12, 4), pill(2, 2, 18, 10, 0)],
        );
        assert_eq!(b, c);
        // The overlap of two opaque pills is not premultiplied twice.
        assert_eq!(px(&b, 8, 6), [200, 100, 50, 255]);
        // Two soft edges on one pixel: colour still follows the source.
        let mut d = gdi_bitmap();
        finish_pills(
            &mut d,
            W,
            H,
            &[pill(2, 2, 18, 10, 3), pill(2, 2, 18, 10, 3)],
        );
        let mut e = gdi_bitmap();
        finish_pills(&mut e, W, H, &[pill(2, 2, 18, 10, 3)]);
        assert_eq!(d, e);
    }

    #[test]
    fn a_pill_partly_outside_is_clipped_not_wrapped() {
        let mut b = gdi_bitmap();
        assert!(finish_pills(
            &mut b,
            W,
            H,
            &[pill(-5, -5, 4, 3, 0), pill(15, 9, 40, 40, 0)]
        ));
        assert_eq!(px(&b, 0, 0)[3], 255);
        assert_eq!(px(&b, 3, 2)[3], 255);
        assert_eq!(px(&b, 4, 2)[3], 0);
        assert_eq!(px(&b, 19, 11)[3], 255);
        assert_eq!(px(&b, 14, 11)[3], 0);
        // Nothing wrapped to the other side of a row.
        assert_eq!(px(&b, 19, 0)[3], 0);
    }

    #[test]
    fn a_buffer_of_the_wrong_size_is_refused_untouched() {
        let mut short = vec![7u8; (W * H * 4) as usize - 4];
        let before = short.clone();
        assert!(!finish_pills(&mut short, W, H, &[pill(0, 0, 5, 5, 0)]));
        assert_eq!(short, before);
        let mut empty: Vec<u8> = Vec::new();
        assert!(!finish_pills(&mut empty, W, H, &[pill(0, 0, 5, 5, 0)]));
        assert!(finish_pills(&mut empty, 0, 0, &[pill(0, 0, 5, 5, 0)]));
    }

    #[test]
    fn degenerate_pills_and_huge_radii_do_not_panic() {
        let mut b = gdi_bitmap();
        let before = b.clone();
        let bad = [
            pill(5, 5, 5, 9, 2),
            pill(9, 9, 2, 2, 2),
            pill(i32::MIN, i32::MIN, i32::MAX, i32::MAX, i32::MAX),
            pill(0, 0, 4, 4, -3),
        ];
        assert!(finish_pills(&mut b, W, H, &bad[..2]));
        assert_eq!(b, before);
        assert!(finish_pills(&mut b, W, H, &bad[2..]));
    }
}
