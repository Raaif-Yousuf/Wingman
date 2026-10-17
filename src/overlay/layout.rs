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

/// Pixels two rects share (0 if they only touch or miss).
fn overlap_area(a: &ScreenRect, b: &ScreenRect) -> i64 {
    let w = i64::from(a.right.min(b.right)) - i64::from(a.left.max(b.left));
    let h = i64::from(a.bottom.min(b.bottom)) - i64::from(a.top.max(b.top));
    w.max(0) * h.max(0)
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

/// How far a label may slide along the text, in label heights. There is no
/// leader line, so past this it no longer reads as being about its word.
const SLIDE_REACH_HEIGHTS: i32 = 3;

/// Most steps a slide takes. Each step jumps past every blocker in the way, so
/// this bounds the cost per candidate to a few passes over `blockers`.
const SLIDE_STEPS: usize = 16;

/// Moves `from` sideways (`dir` is 1 for right, -1 for left) until nothing in
/// `blockers` touches it. `stops` (other requests' anchors) are never passed:
/// a slide that would touch or cross one gives up. `None` if it leaves the
/// monitor, goes further than `reach` from where it started, or does not
/// settle in [`SLIDE_STEPS`].
fn slide(
    from: ScreenRect,
    dir: i32,
    blockers: &[ScreenRect],
    stops: &[ScreenRect],
    monitor: &ScreenRect,
    gap: i32,
    reach: i32,
) -> Option<ScreenRect> {
    let size = (
        from.right.saturating_sub(from.left),
        from.bottom.saturating_sub(from.top),
    );
    let mut c = from;
    for _ in 0..SLIDE_STEPS {
        let hit = blockers.iter().filter(|o| intersects(&c, o));
        let left = if dir > 0 {
            match hit.map(|o| o.right).max() {
                None => return Some(c),
                Some(edge) => edge.saturating_add(gap),
            }
        } else {
            match hit.map(|o| o.left).min() {
                None => return Some(c),
                Some(edge) => edge.saturating_sub(gap).saturating_sub(size.0),
            }
        };
        let next = at(left, c.top, size);
        let moved = i64::from(next.left) - i64::from(from.left);
        if moved.abs() > i64::from(reach) || next.left < monitor.left || next.right > monitor.right
        {
            return None;
        }
        // Everything the label sweeps over on this jump, ends included.
        let swept = ScreenRect {
            left: c.left.min(next.left),
            right: c.right.max(next.right),
            ..c
        };
        if stops.iter().any(|o| intersects(&swept, o)) {
            return None;
        }
        c = next;
    }
    None
}

/// Places each label above, then right, then below, then left of its anchor.
/// A mark that runs over a line break is placed at its first line, where the
/// reader starts; every line still keeps labels off it.
/// A candidate is taken if it is inside the monitor (moved inward along the
/// anchor's edge first, so a label near a monitor edge is not lost), does not
/// cover any annotation's anchor, and does not overlap a label already
/// placed. Of the spots that qualify, the one covering the least of
/// `obstacles` (the page's other text) wins, then the one nearest its base
/// spot, then the first listed: a spot touching no text beats any other, and
/// text never costs a label its place. A spot may be slid sideways along the
/// text, at most [`SLIDE_REACH_HEIGHTS`] label heights, never past another mark.
/// Requests are served in order, so reading order wins a contested spot.
pub fn place_labels(
    requests: &[Request],
    obstacles: &[ScreenRect],
    monitor: ScreenRect,
    gap: i32,
) -> Layout {
    let gap = gap.max(0);
    let all_anchors: Vec<ScreenRect> = requests
        .iter()
        .flat_map(|r| r.anchors.iter().copied())
        .filter(|r| !is_empty(r))
        .collect();
    let text: Vec<ScreenRect> = obstacles.iter().copied().filter(|r| !is_empty(r)).collect();
    let mut placed: Vec<Placed> = Vec::new();
    let mut dropped = Vec::new();
    for (index, req) in requests.iter().enumerate() {
        let size = req.size;
        let first = req.anchors.iter().find(|r| !is_empty(r));
        let Some(b) = first.copied().filter(|_| size.0 > 0 && size.1 > 0) else {
            dropped.push(index);
            continue;
        };
        let x = clamp_span(b.left, size.0, monitor.left, monitor.right);
        let y = clamp_span(b.top, size.1, monitor.top, monitor.bottom);
        // Each candidate carries how far it slid from its base, 0 if it did not.
        let mut candidates: Vec<(Side, ScreenRect, i64)> = vec![
            (
                Side::Above,
                at(x, b.top.saturating_sub(gap).saturating_sub(size.1), size),
                0,
            ),
            (Side::Right, at(b.right.saturating_add(gap), y, size), 0),
            (Side::Below, at(x, b.bottom.saturating_add(gap), size), 0),
            (
                Side::Left,
                at(b.left.saturating_sub(gap).saturating_sub(size.0), y, size),
                0,
            ),
        ];
        // On a packed page every spot above, below or beside the word touches
        // a neighbouring line. Slide each spot along the text, past whatever is
        // in the way, to the white space at a line end or start (#1134).
        // Everything stays in the candidate list, so the least-covered one wins.
        // A slide may pass plain text and placed labels, but never another
        // mark: it stops at the first anchor in its way.
        let blockers: Vec<ScreenRect> = text
            .iter()
            .chain(all_anchors.iter())
            .chain(placed.iter().map(|p| &p.rect))
            .copied()
            .collect();
        let reach = size.1.saturating_mul(SLIDE_REACH_HEIGHTS);
        let bottom_aligned = b.bottom.saturating_sub(size.1);
        let bases = [
            (Side::Right, candidates[1].1),
            (
                Side::Right,
                at(b.right.saturating_add(gap), bottom_aligned, size),
            ),
            (Side::Above, candidates[0].1),
            (Side::Below, candidates[2].1),
            (Side::Left, candidates[3].1),
            (
                Side::Left,
                at(
                    b.left.saturating_sub(gap).saturating_sub(size.0),
                    bottom_aligned,
                    size,
                ),
            ),
        ];
        for (side, base) in bases {
            let toward_right = matches!(side, Side::Right | Side::Above | Side::Below);
            let toward_left = matches!(side, Side::Left | Side::Above | Side::Below);
            for (dir, wanted) in [(1, toward_right), (-1, toward_left)] {
                if !wanted {
                    continue;
                }
                let Some(c) = slide(base, dir, &blockers, &all_anchors, &monitor, gap, reach)
                else {
                    continue;
                };
                // No blocker in the way: it is a candidate already.
                if !candidates.iter().any(|(_, r, _)| *r == c) {
                    let moved = (i64::from(c.left) - i64::from(base.left)).abs();
                    candidates.push((side, c, moved));
                }
            }
        }
        let free = |c: &ScreenRect| {
            inside(c, &monitor)
                && !all_anchors.iter().any(|a| intersects(c, a))
                && !placed.iter().any(|p| intersects(c, &p.rect))
        };
        // The free spot that covers the least other text; of those, the one
        // that slid least, then the first listed, so the nearer spots win.
        let pick = candidates
            .into_iter()
            .filter(|(_, c, _)| free(c))
            .min_by_key(|(_, c, moved)| {
                (text.iter().map(|o| overlap_area(c, o)).sum::<i64>(), *moved)
            });
        match pick {
            Some((side, rect, _)) => placed.push(Placed { index, rect, side }),
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

// Ghost text and the "skipped" tag (#329) are not drawn until that slice, so
// they are built for the tests only; the slice drops the `cfg`.

/// Ghost text (#329): a pill of `size` inside `field`, left-aligned and
/// vertically centred, clipped to the field. `None` for an empty field.
#[cfg(test)]
pub fn ghost_rect(field: ScreenRect, size: (i32, i32)) -> Option<ScreenRect> {
    if is_empty(&field) || size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let height = field.bottom.saturating_sub(field.top);
    let top = field.top.saturating_add(height.saturating_sub(size.1) / 2);
    clip_to(at(field.left, top, size), field)
}

/// A "skipped" tag: a pill of `size` at the field's right edge, inside it.
#[cfg(test)]
pub fn skipped_tag(field: ScreenRect, size: (i32, i32)) -> Option<ScreenRect> {
    if is_empty(&field) || size.0 <= 0 || size.1 <= 0 {
        return None;
    }
    let height = field.bottom.saturating_sub(field.top);
    let top = field.top.saturating_add(height.saturating_sub(size.1) / 2);
    clip_to(at(field.right.saturating_sub(size.0), top, size), field)
}

#[cfg(test)]
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
        place_labels(requests, &[], monitor, GAP_DESIGN_PX)
    }

    #[test]
    fn the_gap_is_a_parameter_the_caller_scales() {
        let a = req(r(100, 100, 200, 120), 50, 20);
        // 200% scale: gap 8, so the label sits 8 px above, not 4.
        let l = place_labels(std::slice::from_ref(&a), &[], MON, 2 * GAP_DESIGN_PX);
        assert_eq!(l.placed[0].rect, r(100, 72, 150, 92));
        let l = place_labels(std::slice::from_ref(&a), &[], MON, 0);
        assert_eq!(l.placed[0].rect, r(100, 80, 150, 100));
        // A negative gap is read as 0, never as overlap.
        let l = place_labels(&[a], &[], MON, -50);
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
    fn a_label_moves_off_text_that_is_not_marked() {
        let a = r(100, 100, 200, 120);
        // An unmarked word sits where the label would go above.
        let word_above = r(90, 70, 210, 98);
        let l = place_labels(&[req(a, 50, 20)], &[word_above], MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Right);
        assert!(!intersects(&l.placed[0].rect, &word_above));
        // Without it the label stays above.
        let l = place_labels(&[req(a, 50, 20)], &[], MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Above);
    }

    #[test]
    fn it_skips_every_side_that_touches_text_in_order() {
        let a = r(100, 100, 200, 120);
        let above = r(100, 70, 200, 98);
        let right = r(204, 100, 260, 120);
        let l = place_labels(&[req(a, 50, 20)], &[above, right], MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Below);
    }

    #[test]
    fn when_every_side_touches_text_the_old_order_still_places_it() {
        let a = r(100, 100, 200, 120);
        // Text runs to both monitor edges, so sliding along it finds no gap.
        let around = [
            r(0, 70, 1000, 98),
            r(204, 100, 1000, 120),
            r(0, 124, 1000, 150),
            r(0, 100, 96, 120),
        ];
        let l = place_labels(&[req(a, 50, 20)], &around, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed.len(), 1, "a label over text beats no label");
        assert_eq!(l.placed[0].side, Side::Above);
    }

    #[test]
    fn when_every_side_touches_text_the_least_covered_one_wins() {
        let a = r(100, 100, 200, 120);
        // Above is covered a lot, Right only by a sliver.
        let around = [
            r(100, 70, 200, 98),
            r(204, 100, 210, 120),
            r(100, 124, 200, 150),
            r(40, 100, 96, 120),
        ];
        let l = place_labels(&[req(a, 50, 20)], &around, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Right);
    }

    #[test]
    fn empty_and_wild_obstacles_are_harmless() {
        let a = r(100, 100, 200, 120);
        let wild = [r(5, 5, 5, 9), r(i32::MIN, i32::MIN, i32::MAX, i32::MAX)];
        let l = place_labels(&[req(a, 50, 20)], &wild, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Above, "all-covering falls back");
        let l = place_labels(&[req(a, 50, 20)], &wild[..1], MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].side, Side::Above);
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
    fn a_two_line_mark_gets_its_label_at_the_first_line() {
        // The first line ends far to the right of where the second starts.
        let first = r(500, 100, 700, 112);
        let second = r(100, 116, 160, 128);
        let two = Request {
            anchors: vec![first, second],
            size: (50, 20),
        };
        let l = place(&[two], MON);
        assert_eq!(l.placed[0].side, Side::Above);
        assert_eq!(l.placed[0].rect, r(500, 76, 550, 96));
    }

    #[test]
    fn the_first_line_is_the_first_one_with_any_area() {
        let two = Request {
            anchors: vec![r(5, 5, 5, 9), r(300, 100, 400, 112), r(100, 116, 160, 128)],
            size: (50, 20),
        };
        let l = place(&[two], MON);
        assert_eq!(l.placed[0].rect, r(300, 76, 350, 96));
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

    /// A packed page: lines 24 px apart, words 21 px tall, so a 28 px label
    /// touches two lines wherever it goes beside or above a word.
    fn packed_page(line_ends: [i32; 3]) -> Vec<ScreenRect> {
        let mut words = Vec::new();
        for (row, end) in line_ends.into_iter().enumerate() {
            let top = 100 + 24 * row as i32;
            let mut x = 16;
            while x + 60 <= end {
                words.push(r(x, top, x + 60, top + 21));
                x += 68;
            }
        }
        words
    }

    #[test]
    fn on_a_packed_page_the_label_goes_to_white_space_not_over_text() {
        let words = packed_page([560, 560, 300]);
        // The mark is the 4th word of the middle line.
        let anchor = words[8 + 3];
        let l = place_labels(&[req(anchor, 90, 28)], &words, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed.len(), 1);
        let covered: i64 = words
            .iter()
            .filter(|w| **w != anchor)
            .map(|w| overlap_area(&l.placed[0].rect, w))
            .sum();
        assert_eq!(covered, 0, "covers {covered} px of other text");
    }

    #[test]
    fn a_far_away_gap_is_not_worth_a_label_that_far_from_its_word() {
        // The only white space is 700 px away: stay near, as before.
        let words = packed_page([900, 900, 900]);
        let anchor = words[13 + 1];
        let l = place_labels(&[req(anchor, 90, 28)], &words, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed.len(), 1);
        let dx = l.placed[0].rect.left - anchor.left;
        // Its own width and the gap, then at most three label heights.
        assert!(dx.abs() <= 60 + 4 + 3 * 28, "label moved {dx} px away");
    }

    #[test]
    fn a_slid_label_keeps_off_other_labels_and_anchors() {
        let words = packed_page([560, 560, 300]);
        let a = words[8 + 3];
        let b = words[8 + 5];
        let l = place_labels(
            &[req(a, 90, 28), req(b, 90, 28)],
            &words,
            MON,
            GAP_DESIGN_PX,
        );
        assert_eq!(l.placed.len(), 2);
        // The first label can only keep off the text by sliding along it.
        assert_eq!(l.placed[0].rect.left, a.right + GAP_DESIGN_PX, "not slid");
        for p in &l.placed {
            let covered: i64 = words
                .iter()
                .filter(|w| **w != a && **w != b)
                .map(|w| overlap_area(&p.rect, w))
                .sum();
            assert_eq!(covered, 0, "{p:?} covers text");
        }
        assert!(!intersects(&l.placed[0].rect, &l.placed[1].rect));
        for p in &l.placed {
            assert!(!intersects(&p.rect, &a) && !intersects(&p.rect, &b));
            assert!(inside(&p.rect, &MON));
        }
    }

    #[test]
    fn a_slide_does_not_jump_over_another_mark() {
        let a = r(100, 100, 200, 120);
        // Text then another request's anchor to the right; white space past it.
        let text = r(204, 100, 220, 140);
        let other = r(224, 100, 240, 140);
        let bars = [
            r(0, 50, 1000, 98),
            r(0, 141, 1000, 170),
            r(0, 100, 96, 140),
            text,
        ];
        let l = place_labels(
            &[req(a, 50, 40), req(other, 1, 1)],
            &bars,
            MON,
            GAP_DESIGN_PX,
        );
        let first = l.placed.iter().find(|p| p.index == 0).expect("placed");
        assert!(first.rect.left <= other.left, "jumped to {:?}", first.rect);
    }

    #[test]
    fn of_two_clear_slides_the_nearer_one_wins() {
        let a = r(300, 100, 360, 120);
        let bars = [
            r(0, 70, 1000, 98),
            r(0, 124, 1000, 150),
            r(364, 100, 420, 120), // blocks Right, 60 px of sliding
            r(280, 100, 296, 120), // blocks Left, 20 px of sliding
        ];
        let l = place_labels(&[req(a, 50, 20)], &bars, MON, GAP_DESIGN_PX);
        assert_eq!(l.placed[0].rect, r(226, 100, 276, 120));
        assert_eq!(l.placed[0].side, Side::Left);
    }
}
