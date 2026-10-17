//! What the overlay window shows, as plain rectangles (spec "Layout (pure)",
//! "One window per monitor"): where each label goes, the underline marks, and
//! the one window that bounds them. `ui::overlay` measures the label text,
//! passes the sizes in, and draws these rectangles; nothing here touches
//! Win32, and every sum saturates because the rects came from the screen.

use crate::capture::ScreenRect;

use super::annotation::Annotation;
use super::layout::{place_labels, underline, Request};

/// Pixel sizes for this monitor, already multiplied by its scale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metrics {
    /// Space between a label and its text (`layout::GAP_DESIGN_PX` scaled).
    pub gap: i32,
    /// Thickness of the underline mark.
    pub mark: i32,
    /// Width of the halo around a mark, so it reads on any page colour.
    pub halo: i32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlacedLabel {
    /// Index into the annotations.
    pub index: usize,
    pub rect: ScreenRect,
}

/// An underline and the halo behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mark {
    pub halo: ScreenRect,
    pub line: ScreenRect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scene {
    /// The bounding box of everything drawn, inside the monitor: the
    /// layered window's rectangle, in screen pixels.
    pub window: ScreenRect,
    pub labels: Vec<PlacedLabel>,
    pub marks: Vec<Mark>,
    /// Annotations that found no place. The summary card counts them.
    pub dropped: Vec<usize>,
}

fn is_empty(r: &ScreenRect) -> bool {
    r.left >= r.right || r.top >= r.bottom
}

fn clip(r: ScreenRect, to: ScreenRect) -> Option<ScreenRect> {
    let out = ScreenRect {
        left: r.left.max(to.left),
        top: r.top.max(to.top),
        right: r.right.min(to.right),
        bottom: r.bottom.min(to.bottom),
    };
    (!is_empty(&out)).then_some(out)
}

fn inflate(r: ScreenRect, by: i32) -> ScreenRect {
    ScreenRect {
        left: r.left.saturating_sub(by),
        top: r.top.saturating_sub(by),
        right: r.right.saturating_add(by),
        bottom: r.bottom.saturating_add(by),
    }
}

/// `r` moved so `origin`'s top-left is (0, 0): window-local pixels.
pub fn to_local(r: ScreenRect, origin: ScreenRect) -> ScreenRect {
    ScreenRect {
        left: r.left.saturating_sub(origin.left),
        top: r.top.saturating_sub(origin.top),
        right: r.right.saturating_sub(origin.left),
        bottom: r.bottom.saturating_sub(origin.top),
    }
}

/// Plans the overlay for `annotations`, whose labels measure `sizes` (same
/// order). `obstacles` are the boxes of the other text on screen, which a label
/// keeps off when it can. `None` when no label found a place: there is then
/// nothing to show and the summary card carries the result alone.
pub fn plan(
    annotations: &[Annotation],
    sizes: &[(i32, i32)],
    obstacles: &[ScreenRect],
    monitor: ScreenRect,
    metrics: &Metrics,
) -> Option<Scene> {
    let requests: Vec<Request> = annotations
        .iter()
        .enumerate()
        .map(|(i, a)| Request {
            anchors: a.rects.clone(),
            size: sizes.get(i).copied().unwrap_or((0, 0)),
        })
        .collect();
    let layout = place_labels(&requests, obstacles, monitor, metrics.gap);

    let mut labels = Vec::new();
    let mut marks = Vec::new();
    let mut union: Option<ScreenRect> = None;
    let mut grow = |r: ScreenRect| {
        union = Some(match union {
            None => r,
            Some(u) => ScreenRect {
                left: u.left.min(r.left),
                top: u.top.min(r.top),
                right: u.right.max(r.right),
                bottom: u.bottom.max(r.bottom),
            },
        });
    };
    for placed in &layout.placed {
        labels.push(PlacedLabel {
            index: placed.index,
            rect: placed.rect,
        });
        grow(placed.rect);
        let Some(a) = annotations.get(placed.index) else {
            continue;
        };
        for anchor in &a.rects {
            if is_empty(anchor) {
                continue;
            }
            let line = underline(*anchor, metrics.mark);
            let Some(line) = clip(line, monitor) else {
                continue;
            };
            let Some(halo) = clip(inflate(line, metrics.halo.max(0)), monitor) else {
                continue;
            };
            grow(halo);
            marks.push(Mark { halo, line });
        }
    }
    let window = clip(union?, monitor)?;
    Some(Scene {
        window,
        labels,
        marks,
        dropped: layout.dropped,
    })
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

    const MONITOR: ScreenRect = ScreenRect {
        left: 0,
        top: 0,
        right: 1000,
        bottom: 800,
    };
    const M: Metrics = Metrics {
        gap: 4,
        mark: 2,
        halo: 1,
    };

    fn note(rects: Vec<ScreenRect>) -> Annotation {
        Annotation::note(rects, "label").unwrap()
    }

    /// `plan` with no other text on the page.
    fn plan(
        annotations: &[Annotation],
        sizes: &[(i32, i32)],
        monitor: ScreenRect,
        metrics: &Metrics,
    ) -> Option<Scene> {
        super::plan(annotations, sizes, &[], monitor, metrics)
    }

    #[test]
    fn a_label_keeps_off_the_other_text_on_the_page() {
        let a = note(vec![r(100, 200, 180, 216)]);
        let above = r(90, 170, 200, 196);
        let s = super::plan(&[a], &[(60, 20)], &[above], MONITOR, &M).unwrap();
        let label = s.labels[0].rect;
        assert_eq!(label, r(184, 200, 244, 220), "moved to the right");
    }

    #[test]
    fn a_label_goes_above_its_text_and_the_window_bounds_everything() {
        let a = note(vec![r(100, 200, 180, 216)]);
        let s = plan(&[a], &[(60, 20)], MONITOR, &M).unwrap();
        assert_eq!(s.labels.len(), 1);
        let label = s.labels[0].rect;
        assert_eq!(label, r(100, 176, 160, 196));
        assert_eq!(s.marks.len(), 1);
        assert_eq!(s.marks[0].line, r(100, 216, 180, 218));
        assert_eq!(s.marks[0].halo, r(99, 215, 181, 219));
        // The window is exactly the union of the label and the halo.
        assert_eq!(s.window, r(99, 176, 181, 219));
        assert!(s.dropped.is_empty());
    }

    #[test]
    fn a_two_line_anchor_gets_an_underline_per_line_and_one_label() {
        let a = note(vec![r(100, 200, 400, 216), r(100, 220, 250, 236)]);
        let s = plan(&[a], &[(60, 20)], MONITOR, &M).unwrap();
        assert_eq!(s.labels.len(), 1);
        assert_eq!(s.marks.len(), 2);
    }

    #[test]
    fn an_annotation_with_no_place_is_dropped_with_its_marks() {
        // A label wider than the monitor cannot be placed anywhere.
        let a = note(vec![r(100, 200, 180, 216)]);
        let b = note(vec![r(100, 400, 180, 416)]);
        let s = plan(&[a, b], &[(5000, 20), (60, 20)], MONITOR, &M).unwrap();
        assert_eq!(s.dropped, vec![0]);
        assert_eq!(s.labels.len(), 1);
        assert_eq!(s.labels[0].index, 1);
        assert_eq!(s.marks.len(), 1, "the dropped annotation draws no mark");
    }

    #[test]
    fn nothing_placed_means_no_scene() {
        let a = note(vec![r(100, 200, 180, 216)]);
        assert!(plan(std::slice::from_ref(&a), &[(5000, 20)], MONITOR, &M).is_none());
        assert!(plan(&[a], &[], MONITOR, &M).is_none(), "no size, no label");
        assert!(plan(&[], &[], MONITOR, &M).is_none());
    }

    #[test]
    fn the_window_never_leaves_the_monitor() {
        // Text on the bottom edge: the underline and its halo are clipped.
        let a = note(vec![r(10, 790, 90, 800)]);
        let s = plan(&[a], &[(60, 20)], MONITOR, &M).unwrap();
        assert!(s.window.left >= 0 && s.window.top >= 0);
        assert!(s.window.right <= 1000 && s.window.bottom <= 800);
        for m in &s.marks {
            assert!(m.halo.bottom <= 800);
        }
    }

    #[test]
    fn rects_far_outside_the_i32_range_do_not_overflow() {
        let a = note(vec![r(i32::MAX - 10, i32::MAX - 10, i32::MAX, i32::MAX)]);
        let wide = r(i32::MIN / 2, i32::MIN / 2, i32::MAX, i32::MAX);
        let _ = plan(&[a], &[(60, 20)], wide, &M);
    }

    #[test]
    fn local_coordinates_start_at_the_window_corner() {
        let window = r(99, 176, 181, 219);
        assert_eq!(to_local(r(100, 176, 160, 196), window), r(1, 0, 61, 20));
        assert_eq!(
            to_local(r(0, 0, 10, 10), r(i32::MIN, i32::MIN, 0, 0)),
            r(i32::MAX, i32::MAX, i32::MAX, i32::MAX)
        );
    }
}
