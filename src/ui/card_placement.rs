//! Issue #109: where the card goes. Pure placement math, no Win32 calls, so
//! it is unit-tested across multi-monitor layouts, negative coordinates and
//! mixed DPI. `ui::card` gathers the monitor rectangles and the cursor, asks
//! this module for an origin, and calls `SetWindowPos`.
//!
//! All rectangles and points are in virtual-screen pixels. A monitor left of
//! or above the primary one has negative coordinates; nothing here assumes
//! the origin is (0, 0). Sizes and margins arrive already scaled for the
//! card's DPI, so a 100 percent and a 200 percent monitor differ only in the
//! numbers passed in.

use crate::config::CardCorner;
use windows::Win32::Foundation::{POINT, RECT};

/// Which monitor the card appears on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MonitorChoice {
    /// The monitor under the mouse when the card appears (the default).
    Cursor,
    /// The primary monitor.
    Primary,
    /// The n-th monitor in the system's enumeration order, 1-based.
    Index(usize),
}

impl MonitorChoice {
    /// Reads `ui.card_monitor`. Case and surrounding space are ignored.
    /// Anything unrecognised (including `0`) means [`MonitorChoice::Cursor`]:
    /// a typo in the config must not stop the card appearing.
    pub(crate) fn parse(s: &str) -> Self {
        let s = s.trim().to_ascii_lowercase();
        match s.as_str() {
            "primary" => MonitorChoice::Primary,
            _ => match s.parse::<usize>() {
                Ok(n) if n >= 1 => MonitorChoice::Index(n),
                _ => MonitorChoice::Cursor,
            },
        }
    }
}

/// Picks the work area the card is placed in. A pinned index with no such
/// monitor (it was unplugged) falls back to the cursor's monitor, so the
/// card never lands off screen.
pub(crate) fn resolve_work_area(
    choice: MonitorChoice,
    cursor_work: RECT,
    primary_work: RECT,
    all_work: &[RECT],
) -> RECT {
    match choice {
        MonitorChoice::Cursor => cursor_work,
        MonitorChoice::Primary => primary_work,
        MonitorChoice::Index(n) => all_work.get(n - 1).copied().unwrap_or(cursor_work),
    }
}

/// Top-left of a `w` x `h` card inside `work`. The card always ends up fully
/// inside `work` when it fits (and flush with the top-left edge when it does
/// not). `margin` is the gap to the screen edge, or to the mouse pointer for
/// [`CardCorner::FollowMouse`], which puts the card below and right of
/// `anchor`, flipping to the other side when that would overflow.
pub(crate) fn card_origin(
    corner: CardCorner,
    work: RECT,
    margin: i32,
    w: i32,
    h: i32,
    anchor: POINT,
) -> (i32, i32) {
    let left = work.left + margin;
    let right = work.right - margin - w;
    let top = work.top + margin;
    let bottom = work.bottom - margin - h;
    let (x, y) = match corner {
        CardCorner::BottomRight => (right, bottom),
        CardCorner::BottomLeft => (left, bottom),
        CardCorner::TopRight => (right, top),
        CardCorner::TopLeft => (left, top),
        CardCorner::FollowMouse => {
            let x = if anchor.x + margin + w > work.right {
                anchor.x - margin - w
            } else {
                anchor.x + margin
            };
            let y = if anchor.y + margin + h > work.bottom {
                anchor.y - margin - h
            } else {
                anchor.y + margin
            };
            (x, y)
        }
    };
    // Fully inside the work area; flush with its top-left when it cannot fit.
    (
        x.min(work.right - w).max(work.left),
        y.min(work.bottom - h).max(work.top),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }
    fn p(x: i32, y: i32) -> POINT {
        POINT { x, y }
    }

    // Primary 1920x1040 work area; a second monitor to its left (negative x)
    // and a third above it (negative y), the layout that breaks code assuming
    // the origin is the top-left of everything.
    const PRIMARY: RECT = RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1040,
    };
    const LEFT: RECT = RECT {
        left: -2560,
        top: -200,
        right: 0,
        bottom: 1240,
    };
    const ABOVE: RECT = RECT {
        left: 0,
        top: -1080,
        right: 1920,
        bottom: 0,
    };

    #[test]
    fn parse_monitor_choice() {
        assert_eq!(MonitorChoice::parse("cursor"), MonitorChoice::Cursor);
        assert_eq!(MonitorChoice::parse(" Primary "), MonitorChoice::Primary);
        assert_eq!(MonitorChoice::parse("2"), MonitorChoice::Index(2));
        assert_eq!(MonitorChoice::parse(" 3 "), MonitorChoice::Index(3));
    }

    #[test]
    fn parse_junk_means_cursor() {
        assert_eq!(MonitorChoice::parse(""), MonitorChoice::Cursor);
        assert_eq!(MonitorChoice::parse("0"), MonitorChoice::Cursor);
        assert_eq!(MonitorChoice::parse("-1"), MonitorChoice::Cursor);
        assert_eq!(MonitorChoice::parse("left"), MonitorChoice::Cursor);
    }

    #[test]
    fn resolve_picks_each_monitor() {
        let all = [PRIMARY, LEFT, ABOVE];
        assert_eq!(
            resolve_work_area(MonitorChoice::Cursor, LEFT, PRIMARY, &all),
            LEFT
        );
        assert_eq!(
            resolve_work_area(MonitorChoice::Primary, LEFT, PRIMARY, &all),
            PRIMARY
        );
        assert_eq!(
            resolve_work_area(MonitorChoice::Index(1), PRIMARY, PRIMARY, &all),
            PRIMARY
        );
        assert_eq!(
            resolve_work_area(MonitorChoice::Index(2), PRIMARY, PRIMARY, &all),
            LEFT
        );
        assert_eq!(
            resolve_work_area(MonitorChoice::Index(3), PRIMARY, PRIMARY, &all),
            ABOVE
        );
    }

    #[test]
    fn resolve_unplugged_index_falls_back_to_the_cursor_monitor() {
        let all = [PRIMARY, LEFT];
        assert_eq!(
            resolve_work_area(MonitorChoice::Index(3), LEFT, PRIMARY, &all),
            LEFT
        );
        assert_eq!(
            resolve_work_area(MonitorChoice::Index(1), LEFT, PRIMARY, &[]),
            LEFT
        );
    }

    #[test]
    fn corners_on_the_primary_monitor() {
        let (w, h, m) = (280, 100, 14);
        assert_eq!(
            card_origin(CardCorner::BottomRight, PRIMARY, m, w, h, p(0, 0)),
            (1626, 926)
        );
        assert_eq!(
            card_origin(CardCorner::BottomLeft, PRIMARY, m, w, h, p(0, 0)),
            (14, 926)
        );
        assert_eq!(
            card_origin(CardCorner::TopRight, PRIMARY, m, w, h, p(0, 0)),
            (1626, 14)
        );
        assert_eq!(
            card_origin(CardCorner::TopLeft, PRIMARY, m, w, h, p(0, 0)),
            (14, 14)
        );
    }

    #[test]
    fn corners_on_a_monitor_with_negative_coordinates() {
        let (w, h, m) = (280, 100, 14);
        assert_eq!(
            card_origin(CardCorner::TopLeft, LEFT, m, w, h, p(0, 0)),
            (-2546, -186)
        );
        assert_eq!(
            card_origin(CardCorner::BottomRight, LEFT, m, w, h, p(0, 0)),
            (-294, 1126)
        );
        assert_eq!(
            card_origin(CardCorner::BottomLeft, ABOVE, m, w, h, p(0, 0)),
            (14, -114)
        );
    }

    #[test]
    fn corners_with_a_scaled_up_card_on_a_high_dpi_monitor() {
        // 200 percent: the card and margin arrive doubled.
        let hi = r(3840, 0, 7680, 2120);
        assert_eq!(
            card_origin(CardCorner::BottomRight, hi, 28, 560, 200, p(0, 0)),
            (7092, 1892)
        );
    }

    #[test]
    fn a_card_larger_than_the_work_area_sticks_to_the_top_left() {
        let small = r(100, 50, 300, 150);
        assert_eq!(
            card_origin(CardCorner::BottomRight, small, 14, 280, 200, p(0, 0)),
            (100, 50)
        );
    }

    #[test]
    fn follow_mouse_sits_below_right_of_the_pointer() {
        assert_eq!(
            card_origin(CardCorner::FollowMouse, PRIMARY, 14, 280, 100, p(500, 400)),
            (514, 414)
        );
    }

    #[test]
    fn follow_mouse_flips_left_and_up_near_the_far_edges() {
        assert_eq!(
            card_origin(
                CardCorner::FollowMouse,
                PRIMARY,
                14,
                280,
                100,
                p(1900, 1030)
            ),
            (1606, 916)
        );
    }

    #[test]
    fn follow_mouse_on_a_negative_monitor_stays_inside_it() {
        // Pointer at the left monitor's top-left corner: cannot flip left or
        // up without leaving the monitor, so it clamps.
        assert_eq!(
            card_origin(CardCorner::FollowMouse, LEFT, 14, 280, 100, p(-2560, -200)),
            (-2546, -186)
        );
        assert_eq!(
            card_origin(CardCorner::FollowMouse, LEFT, 14, 280, 100, p(-10, 1235)),
            (-304, 1121)
        );
    }
}
