//! Where each label goes (spec "Layout (pure)"). Everything is in physical
//! virtual-screen pixels, as `anchor` returns them. The caller measures each
//! label (GDI text extent plus padding, scaled for DPI) and passes the size in;
//! nothing here touches Win32.
//!
//! Rects come from other processes (UIA) or the screen (OCR), so every sum
//! saturates: no overflow, no panic.

use crate::capture::ScreenRect;

/// Space between a label and the text it is about, in design pixels (96 DPI).
/// Everything this module takes and returns is physical pixels, so the caller
/// multiplies this by the monitor's scale and passes the result as `gap` to
/// [`place_labels`], the same way it scales label sizes and mark thickness.
pub const GAP_DESIGN_PX: i32 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Above,
    Right,
    Below,
    Left,
}

/// One label to place: the rects of the text it is about and its own size.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub anchors: Vec<ScreenRect>,
    /// `(width, height)` of the label. Zero or negative drops it.
    pub size: (i32, i32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Placed {
    /// Index into the requests.
    pub index: usize,
    pub rect: ScreenRect,
    pub side: Side,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub placed: Vec<Placed>,
    /// Requests that found no place. The summary card counts them.
    pub dropped: Vec<usize>,
}

fn intersects(a: &ScreenRect, b: &ScreenRect) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

fn inside(inner: &ScreenRect, outer: &ScreenRect) -> bool {
    inner.left >= outer.left
        && inner.top >= outer.top
        && inner.right <= outer.right
        && inner.bottom <= outer.bottom
}

fn is_empty(r: &ScreenRect) -> bool {
    r.left >= r.right || r.top >= r.bottom
}

/// The box around all of `rects`, ignoring empty ones. `None` if none count.
fn bounds(rects: &[ScreenRect]) -> Option<ScreenRect> {
    rects
        .iter()
        .filter(|r| !is_empty(r))
        .fold(None, |acc: Option<ScreenRect>, r| {
            Some(match acc {
                None => *r,
                Some(a) => ScreenRect {
                    left: a.left.min(r.left),
                    top: a.top.min(r.top),
                    right: a.right.max(r.right),
                    bottom: a.bottom.max(r.bottom),
                },
            })
        })
}

/// A rect of `size` with its left edge at `left` and top edge at `top`.
fn at(left: i32, top: i32, size: (i32, i32)) -> ScreenRect {
    ScreenRect {
        left,
        top,
        right: left.saturating_add(size.0),
        bottom: top.saturating_add(size.1),
    }
}

/// `v` moved into `lo..=hi - len` so a span of `len` fits, if it can.
fn clamp_span(v: i32, len: i32, lo: i32, hi: i32) -> i32 {
    let max = hi.saturating_sub(len);
    if max < lo {
        v
    } else {
        v.clamp(lo, max)
    }
}

/// Places each label above, then right, then below, then left of its anchor.
/// A candidate is taken if it is inside the monitor (moved inward along the
/// anchor's edge first, so a label near a monitor edge is not lost), does not
/// cover any annotation's anchor, and does not overlap a label already
/// placed. Requests are served in order, so reading order wins a contested
/// spot.
pub fn place_labels(requests: &[Request], monitor: ScreenRect, gap: i32) -> Layout {
    let gap = gap.max(0);
    let all_anchors: Vec<ScreenRect> = requests
        .iter()
        .flat_map(|r| r.anchors.iter().copied())
        .filter(|r| !is_empty(r))
        .collect();
    let mut placed: Vec<Placed> = Vec::new();
    let mut dropped = Vec::new();
    for (index, req) in requests.iter().enumerate() {
        let size = req.size;
        let Some(b) = bounds(&req.anchors).filter(|_| size.0 > 0 && size.1 > 0) else {
            dropped.push(index);
            continue;
        };
        let x = clamp_span(b.left, size.0, monitor.left, monitor.right);
        let y = clamp_span(b.top, size.1, monitor.top, monitor.bottom);
        let candidates = [
            (
                Side::Above,
                at(x, b.top.saturating_sub(gap).saturating_sub(size.1), size),
            ),
            (Side::Right, at(b.right.saturating_add(gap), y, size)),
            (Side::Below, at(x, b.bottom.saturating_add(gap), size)),
            (
                Side::Left,
                at(b.left.saturating_sub(gap).saturating_sub(size.0), y, size),
            ),
        ];
        let pick = candidates.into_iter().find(|(_, c)| {
            inside(c, &monitor)
                && !all_anchors.iter().any(|a| intersects(c, a))
                && !placed.iter().any(|p| intersects(c, &p.rect))
        });
        match pick {
            Some((side, rect)) => placed.push(Placed { index, rect, side }),
            None => dropped.push(index),
        }
    }
    Layout { placed, dropped }
}

/// The underline for an anchor rect: `thickness` pixels directly under it.
pub fn underline(anchor: ScreenRect, thickness: i32) -> ScreenRect {
    ScreenRect {
        left: anchor.left,
        top: anchor.bottom,
        right: anchor.right,
        bottom: anchor.bottom.saturating_add(thickness.max(0)),
    }
}

/// Ghost text (#329): a pill of `size` inside `field`, left-aligned and
/// vertically centred, clipped to the field. `None` for an empty field.
pub fn ghost_rect(field: ScreenRect, size: (i32, i32)) -> Option<ScreenRect> {
    if is_empty(&field) || size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let height = field.bottom.saturating_sub(field.top);
    let top = field.top.saturating_add(height.saturating_sub(size.1) / 2);
    clip_to(at(field.left, top, size), field)
}

/// A "skipped" tag: a pill of `size` at the field's right edge, inside it.
pub fn skipped_tag(field: ScreenRect, size: (i32, i32)) -> Option<ScreenRect> {
    if is_empty(&field) || size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let height = field.bottom.saturating_sub(field.top);
    let top = field.top.saturating_add(height.saturating_sub(size.1) / 2);
    clip_to(at(field.right.saturating_sub(size.0), top, size), field)
}

fn clip_to(r: ScreenRect, bounds: ScreenRect) -> Option<ScreenRect> {
    let out = ScreenRect {
        left: r.left.max(bounds.left),
        top: r.top.max(bounds.top),
        right: r.right.min(bounds.right),
        bottom: r.bottom.min(bounds.bottom),
    };
    (!is_empty(&out)).then_some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn req(anchor: ScreenRect, w: i32, h: i32) -> Request {
        Request {
            anchors: vec![anchor],
            size: (w, h),
        }
    }

    fn place(requests: &[Request], monitor: ScreenRect) -> Layout {
        place_labels(requests, monitor, GAP_DESIGN_PX)
    }

    #[test]
    fn the_gap_is_a_parameter_the_caller_scales() {
        let a = req(r(100, 100, 200, 120), 50, 20);
        // 200% scale: gap 8, so the label sits 8 px above, not 4.
        let l = place_labels(std::slice::from_ref(&a), MON, 2 * GAP_DESIGN_PX);
        assert_eq!(l.placed[0].rect, r(100, 72, 150, 92));
        let l = place_labels(std::slice::from_ref(&a), MON, 0);
        assert_eq!(l.placed[0].rect, r(100, 80, 150, 100));
        // A negative gap is read as 0, never as overlap.
        let l = place_labels(&[a], MON, -50);
        assert_eq!(l.placed[0].rect, r(100, 80, 150, 100));
    }

    const MON: ScreenRect = ScreenRect {
        left: 0,
        top: 0,
        right: 1000,
        bottom: 1000,
    };

    #[test]
    fn a_label_goes_above_its_anchor() {
        let l = place(&[req(r(100, 100, 200, 120), 50, 20)], MON);
        assert_eq!(
            l.placed,
            vec![Placed {
                index: 0,
                rect: r(100, 76, 150, 96),
                side: Side::Above
            }]
        );
        assert!(l.dropped.is_empty());
    }

    #[test]
    fn it_falls_right_when_above_is_off_screen() {
        let l = place(&[req(r(100, 5, 200, 25), 50, 20)], MON);
        assert_eq!(l.placed[0].side, Side::Right);
        assert_eq!(l.placed[0].rect, r(204, 5, 254, 25));
    }

    #[test]
    fn it_tries_below_then_left() {
        // Top-right corner: above is off-screen and right is off-screen.
        let l = place(&[req(r(940, 5, 1000, 25), 50, 20)], MON);
        assert_eq!(l.placed[0].side, Side::Below);
        assert_eq!(l.placed[0].rect, r(940, 29, 990, 49));
        // Boxed in above (off-screen), right (off), below (a second anchor).
        let blocker = r(900, 29, 1000, 60);
        let l = place(
            &[
                req(r(940, 5, 1000, 25), 50, 20),
                req(blocker, 1, 1), // its own label goes elsewhere
            ],
            MON,
        );
        assert_eq!(l.placed[0].side, Side::Left);
        assert_eq!(l.placed[0].rect, r(886, 5, 936, 25));
    }

    #[test]
    fn a_label_near_the_edge_is_moved_inward() {
        let l = place(&[req(r(960, 100, 1000, 120), 50, 20)], MON);
        assert_eq!(l.placed[0].side, Side::Above);
        assert_eq!(l.placed[0].rect, r(950, 76, 1000, 96));
    }

    #[test]
    fn a_second_label_avoids_the_first() {
        let a = r(100, 100, 200, 120);
        let l = place(&[req(a, 50, 20), req(a, 50, 20)], MON);
        assert_eq!(l.placed.len(), 2);
        assert_eq!(l.placed[0].side, Side::Above);
        assert_eq!(l.placed[1].side, Side::Right);
        assert!(!intersects(&l.placed[0].rect, &l.placed[1].rect));
    }

    #[test]
    fn a_label_never_covers_any_anchor() {
        // B's anchor sits where A's label would go above.
        let a = r(100, 100, 200, 120);
        let b = r(90, 70, 210, 98);
        let l = place(&[req(a, 50, 20), req(b, 50, 20)], MON);
        for p in &l.placed {
            for anchor in [a, b] {
                assert!(!intersects(&p.rect, &anchor), "{p:?} covers {anchor:?}");
            }
        }
        assert_eq!(l.placed[0].side, Side::Right);
    }

    #[test]
    fn a_multi_line_anchor_is_one_box() {
        let two = Request {
            anchors: vec![r(100, 100, 300, 112), r(100, 116, 160, 128)],
            size: (50, 20),
        };
        let l = place(&[two], MON);
        assert_eq!(l.placed[0].rect, r(100, 76, 150, 96));
    }

    #[test]
    fn a_label_that_fits_nowhere_is_dropped_and_counted() {
        let tiny = r(0, 0, 60, 30);
        let l = place(&[req(r(10, 5, 50, 25), 50, 20)], tiny);
        assert!(l.placed.is_empty());
        assert_eq!(l.dropped, vec![0]);
    }

    #[test]
    fn nothing_to_anchor_or_no_size_is_dropped() {
        let none = Request {
            anchors: vec![],
            size: (50, 20),
        };
        let empty_rect = req(r(5, 5, 5, 9), 50, 20);
        let no_size = req(r(100, 100, 200, 120), 0, 20);
        let l = place(&[none, empty_rect, no_size], MON);
        assert_eq!(l.dropped, vec![0, 1, 2]);
    }

    #[test]
    fn huge_coordinates_do_not_overflow() {
        let wild = r(i32::MIN, i32::MIN, i32::MAX, i32::MAX);
        let l = place(&[req(wild, i32::MAX, i32::MAX)], MON);
        assert_eq!(l.dropped, vec![0]);
        let edge = r(i32::MAX - 10, i32::MAX - 10, i32::MAX, i32::MAX);
        let l = place(&[req(edge, 50, 20)], MON);
        assert_eq!(l.dropped, vec![0]);
        assert_eq!(underline(edge, i32::MAX).bottom, i32::MAX);
    }

    #[test]
    fn a_monitor_with_negative_coordinates_works() {
        let mon = r(-1920, 0, 0, 1080);
        let l = place(&[req(r(-1000, 500, -900, 520), 50, 20)], mon);
        assert_eq!(l.placed[0].rect, r(-1000, 476, -950, 496));
    }

    #[test]
    fn an_underline_sits_directly_under_the_anchor() {
        assert_eq!(underline(r(10, 20, 110, 40), 2), r(10, 40, 110, 42));
        assert_eq!(underline(r(10, 20, 110, 40), -5), r(10, 40, 110, 40));
    }

    #[test]
    fn ghost_text_is_centred_in_and_clipped_to_the_field() {
        let field = r(10, 10, 60, 40);
        assert_eq!(ghost_rect(field, (200, 20)), Some(r(10, 15, 60, 35)));
        assert_eq!(ghost_rect(field, (30, 20)), Some(r(10, 15, 40, 35)));
        assert_eq!(ghost_rect(r(10, 10, 10, 40), (30, 20)), None);
        assert_eq!(ghost_rect(field, (0, 20)), None);
    }

    #[test]
    fn a_skipped_tag_sits_at_the_fields_right_edge() {
        let field = r(10, 10, 300, 40);
        assert_eq!(skipped_tag(field, (50, 20)), Some(r(250, 15, 300, 35)));
        assert_eq!(skipped_tag(r(0, 0, 0, 0), (50, 20)), None);
    }
}
