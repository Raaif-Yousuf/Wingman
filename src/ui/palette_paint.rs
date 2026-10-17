//! What the Quick Ask palette looks like, with no Win32 in it: the colours it
//! draws with, where everything goes at a given DPI, and the list of shapes
//! and text to paint (#970).
//!
//! The Direct2D path and the GDI path (the `WM_PRINTCLIENT` picture and the
//! fallback when Direct2D is missing) both walk the same [`Op`] list, so they
//! cannot drift apart: a row that is inset and rounded in one is inset and
//! rounded in the other, and one set of tests covers both. The two executors
//! in `palette.rs` only know how to draw a rounded fill, a rounded outline and
//! a line of text.
//!
//! Every size is in dp (pixels at 96 DPI) and is scaled by [`px`] for the
//! window's DPI. The Direct2D path asks for dp-sized geometry (`dpi` 96) and
//! lets the render target scale it. Spacing, radius and type sizes come from
//! `theme.rs`; what is left here are the sizes of the palette's own parts.

use windows::Win32::Foundation::RECT;

use crate::ui::palette_model::{Row, MAX_VISIBLE_ROWS};
use crate::ui::theme::{
    ensure_contrast, SysColors, Theme, Tokens, CONTROL_RADIUS_DP, GLYPH_SEARCH, ICON_BOX_DP,
    SPACE_MD_DP, SPACE_SM_DP, SPACE_XS_DP, TYPE_CAPTION_LINE_PX,
};

pub(super) const WINDOW_WIDTH: i32 = 560;
/// Space between the window edge and the query box, the rows and the footer.
const EDGE_DP: i32 = SPACE_MD_DP;
/// The query box: a text line, 8 dp above and below.
const BOX_HEIGHT_DP: i32 = 32;
/// Between the query box and the first row, and the same under the last row.
const BOX_GAP_DP: i32 = SPACE_SM_DP;
/// One row's slot. The fill is the slot less [`ROW_FILL_INSET_Y_DP`] above and
/// below, so two filled rows are 4 dp apart, as in a Windows 11 list.
const ROW_HEIGHT_DP: i32 = 28;
const ROW_FILL_INSET_Y_DP: i32 = 2;
/// From a row's left edge to its text and to a glyph. The same as the glyph in
/// the query box, so the glyph, the row text and the footer share one edge.
const TEXT_INSET_DP: i32 = SPACE_MD_DP;
/// From a row's left edge to its selection bar.
const BAR_INSET_DP: i32 = SPACE_XS_DP;
const BAR_WIDTH_DP: i32 = 3;
const BAR_HEIGHT_DP: i32 = ICON_BOX_DP;
/// Between the glyph in the query box and the text typed after it.
const GLYPH_GAP_DP: i32 = SPACE_SM_DP;
/// The line under a focused query box.
const FOCUS_LINE_DP: i32 = 2;
/// Between the last row and the footer line, and under the footer line.
const FOOTER_GAP_DP: i32 = SPACE_SM_DP;
const FOOTER_MARGIN_DP: i32 = SPACE_MD_DP;
/// The scroll thumb is this wide, this far from the window's right edge and
/// never shorter than this.
const THUMB_WIDTH_DP: i32 = 4;
const THUMB_EDGE_DP: i32 = SPACE_XS_DP;
const THUMB_MIN_DP: i32 = 24;

/// `v` dp at `dpi`.
pub(super) fn px(v: i32, dpi: u32) -> i32 {
    v * dpi as i32 / 96
}

// ---------------------------------------------------------------------------
// Colours
// ---------------------------------------------------------------------------

/// Every colour the palette draws, for one theme (#579). Built from the shared
/// tokens in `theme.rs`, so there are no literal colours in this file and the
/// palette follows light, dark and high contrast like the card does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct PaletteColors {
    pub(super) background: u32,
    /// The query box, which is a step off the background (the window colour in
    /// high contrast).
    pub(super) box_fill: u32,
    /// The hairline around the query box.
    pub(super) border: u32,
    /// The line under the query box while it has focus: the accent, and the
    /// system highlight in high contrast.
    pub(super) focus_line: u32,
    /// The search glyph in the query box.
    pub(super) glyph: u32,
    /// Action row text.
    pub(super) text: u32,
    /// Group headers.
    pub(super) header: u32,
    /// The "no matching actions" hint.
    pub(super) hint: u32,
    pub(super) footer: u32,
    /// The scroll thumb shown when there are more rows than fit (#883).
    pub(super) thumb: u32,
    /// The placeholder in an empty query box.
    pub(super) cue: u32,
    /// The fill of a row the mouse is over: quieter than the selection's.
    /// High contrast has no tints, so there it is the background and the row
    /// gets `hover_outline` instead.
    pub(super) hover_fill: u32,
    pub(super) hover_outline: Option<u32>,
    /// The fill of the selected row.
    pub(super) selected_fill: u32,
    pub(super) selected_text: u32,
    /// The bar on the selected row. `None` in high contrast, where the whole
    /// row is the system highlight and a bar would be a decoration.
    pub(super) bar: Option<u32>,
}

/// The palette's colours in `theme`. High contrast uses the system colours
/// only: the selected row is the system highlight, as in a Windows list.
pub(super) fn palette_colors(theme: Theme, sys: &SysColors, accent: u32) -> PaletteColors {
    use crate::ui::card_buttons::blend;
    let t = Tokens::for_theme(theme, sys);
    if theme == Theme::HighContrast {
        return PaletteColors {
            background: t.surface,
            box_fill: t.control_fill,
            border: t.border,
            focus_line: sys.highlight,
            glyph: t.text,
            text: t.text,
            header: t.text,
            hint: t.text,
            footer: t.text,
            cue: sys.gray_text,
            thumb: t.text,
            hover_fill: t.surface,
            hover_outline: Some(t.text),
            selected_fill: sys.highlight,
            selected_text: sys.highlight_text,
            bar: None,
        };
    }
    // An accent from live_accent already clears 3:1; this keeps any other
    // caller honest too.
    let bar = ensure_contrast(accent, t.surface, 3.0);
    PaletteColors {
        background: t.surface,
        box_fill: t.control_fill,
        border: t.border,
        focus_line: bar,
        glyph: t.text_dim,
        text: t.text,
        header: t.text_dim,
        hint: t.text_faint,
        footer: t.text_faint,
        cue: t.text_dim,
        thumb: t.text_faint,
        // Neutral tints of the text colour, as in a Windows 11 list: only the
        // bar carries the accent.
        hover_fill: blend(t.surface, t.text, 7),
        hover_outline: None,
        selected_fill: blend(t.surface, t.text, 12),
        selected_text: t.text,
        bar: Some(bar),
    }
}

// ---------------------------------------------------------------------------
// Geometry (client pixels at `dpi`)
// ---------------------------------------------------------------------------

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right,
        bottom,
    }
}

pub(super) fn window_width(dpi: u32) -> i32 {
    px(WINDOW_WIDTH, dpi)
}

/// The query box, border included.
pub(super) fn box_rect(dpi: u32) -> RECT {
    let top = px(EDGE_DP, dpi);
    rect(
        px(EDGE_DP, dpi),
        top,
        window_width(dpi) - px(EDGE_DP, dpi),
        top + px(BOX_HEIGHT_DP, dpi),
    )
}

/// Where the search glyph goes: a square in the box, centred top to bottom.
pub(super) fn glyph_rect(dpi: u32) -> RECT {
    let b = box_rect(dpi);
    let side = px(ICON_BOX_DP, dpi);
    let left = b.left + px(TEXT_INSET_DP, dpi);
    let top = b.top + (b.bottom - b.top - side) / 2;
    rect(left, top, left + side, top + side)
}

/// Where the query `EDIT` goes, in client pixels (#881). A single-line EDIT
/// draws its text from the top of its client area and cannot be told to centre
/// it, so the control is exactly one text line high (`text_h`, from the font
/// it uses) and sits in the middle of the box, after the glyph.
pub(super) fn query_edit_rect(dpi: u32, text_h: i32) -> RECT {
    let b = box_rect(dpi);
    let box_h = b.bottom - b.top;
    let h = text_h.clamp(1, box_h.max(1));
    let top = b.top + (box_h - h) / 2;
    rect(
        glyph_rect(dpi).right + px(GLYPH_GAP_DP, dpi),
        top,
        b.right - px(TEXT_INSET_DP, dpi),
        top + h,
    )
}

pub(super) fn list_top(dpi: u32) -> i32 {
    box_rect(dpi).bottom + px(BOX_GAP_DP, dpi)
}

pub(super) fn row_height(dpi: u32) -> i32 {
    px(ROW_HEIGHT_DP, dpi)
}

fn list_bottom(dpi: u32) -> i32 {
    list_top(dpi) + row_height(dpi) * MAX_VISIBLE_ROWS as i32
}

/// The footer line: caption height, level with the row text.
pub(super) fn footer_rect(dpi: u32) -> RECT {
    let top = list_bottom(dpi) + px(FOOTER_GAP_DP, dpi);
    rect(
        px(EDGE_DP + TEXT_INSET_DP, dpi),
        top,
        window_width(dpi) - px(EDGE_DP, dpi),
        top + px(TYPE_CAPTION_LINE_PX, dpi),
    )
}

pub(super) fn window_height(dpi: u32) -> i32 {
    footer_rect(dpi).bottom + px(FOOTER_MARGIN_DP, dpi)
}

/// The slot of the row painted `pos` rows down the viewport: the full list
/// width, one row high.
pub(super) fn row_slot(dpi: u32, pos: i32) -> RECT {
    let top = list_top(dpi) + row_height(dpi) * pos;
    rect(
        px(EDGE_DP, dpi),
        top,
        window_width(dpi) - px(EDGE_DP, dpi),
        top + row_height(dpi),
    )
}

/// Where a row's text goes: in from the slot's left edge, the same for action
/// rows, headers and hints, and in from the right edge by as much, so a long
/// name ends in its ellipsis inside the row's fill and well clear of the scroll
/// thumb (#1000).
fn text_slot(dpi: u32, slot: RECT) -> RECT {
    RECT {
        left: slot.left + px(TEXT_INSET_DP, dpi),
        right: slot.right - px(TEXT_INSET_DP, dpi),
        ..slot
    }
}

/// What a selected or hovered row fills: the slot, less a little above and
/// below.
pub(super) fn row_fill(dpi: u32, pos: i32) -> RECT {
    let s = row_slot(dpi, pos);
    let inset = px(ROW_FILL_INSET_Y_DP, dpi);
    rect(s.left, s.top + inset, s.right, s.bottom - inset)
}

/// #357: pure hit-test -- given a client-area `y` in physical pixels (as
/// `WM_MOUSEMOVE`/`WM_LBUTTONUP` deliver it), the current viewport `offset`
/// and the real row count, returns the real row index (into
/// `PaletteState::rows`, the same space `PaletteState::selected` lives in)
/// under that `y`, or `None` when `y` is above the list, below the last real
/// row, or below the whole painted viewport. Uses the same slots as
/// [`build_ops`], so a click lands on the row it looks like it landed on.
pub(super) fn row_at(y: i32, dpi: u32, offset: usize, row_count: usize) -> Option<usize> {
    let top = list_top(dpi);
    let row_h = row_height(dpi);
    if row_h <= 0 || y < top {
        return None;
    }
    let visible_pos = ((y - top) / row_h) as usize;
    if visible_pos >= MAX_VISIBLE_ROWS {
        return None;
    }
    let idx = offset + visible_pos;
    (idx < row_count).then_some(idx)
}

/// Where the scroll thumb is drawn (#883), in client pixels at `dpi`, for a
/// list of `row_count` rows scrolled to `offset`. `None` when every row fits.
/// It sits in the margin beside the rows, so it never covers a row's fill or
/// text.
pub(super) fn thumb_rect(dpi: u32, offset: usize, row_count: usize) -> Option<RECT> {
    let track = row_height(dpi) * MAX_VISIBLE_ROWS as i32;
    let (top, len) = crate::ui::palette_model::scroll_thumb(
        offset,
        row_count,
        MAX_VISIBLE_ROWS,
        track,
        px(THUMB_MIN_DP, dpi),
    )?;
    let right = window_width(dpi) - px(THUMB_EDGE_DP, dpi);
    let top = list_top(dpi) + top;
    Some(rect(
        right - px(THUMB_WIDTH_DP, dpi).max(2),
        top,
        right,
        top + len,
    ))
}

/// Whether a line of text ends in an ellipsis when it is wider than its
/// rectangle (#1000). Everything does but the icon glyph, which is one
/// character in a box of its own.
pub(super) fn ellipsizes(face: Face) -> bool {
    face != Face::Icon
}

/// Whole device pixels for `v` dp at `dpi`, rounded to the nearest.
fn snap_px(v: i32, dpi: u32) -> i32 {
    (v * dpi as i32 + 48).div_euclid(96)
}

/// A rectangle with its edges on whole device pixels, in dp (what the
/// Direct2D target is drawn in, see the module doc). At 150 percent a 1 dp
/// edge is 1.5 px, which Direct2D anti-aliases across two rows of pixels; the
/// GDI path only ever has whole pixels, so the Direct2D executor snaps to match
/// (#1001).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SnappedRect {
    pub(super) left: f32,
    pub(super) top: f32,
    pub(super) right: f32,
    pub(super) bottom: f32,
}

/// `r` (in dp) with each edge moved to the nearest whole device pixel at `dpi`.
pub(super) fn snap_rect(r: RECT, dpi: u32) -> SnappedRect {
    let scale = dpi as f32 / 96.0;
    let dp = |v: i32| snap_px(v, dpi) as f32 / scale;
    SnappedRect {
        left: dp(r.left),
        top: dp(r.top),
        right: dp(r.right),
        bottom: dp(r.bottom),
    }
}

/// An outline for Direct2D: the path to stroke and how thick to stroke it, all
/// in dp.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct SnappedStroke {
    pub(super) rect: SnappedRect,
    pub(super) radius: f32,
    pub(super) width: f32,
}

/// The outline of `r` (in dp), `width_dp` thick, drawn inside `r`, crisp at
/// `dpi` (#1001). The width is a whole number of pixels, as in the GDI path
/// (`px`, at least one), and the path is half that width in from each snapped
/// edge, so the stroke covers exactly whole pixel rows and columns.
pub(super) fn snap_stroke(r: RECT, radius_dp: i32, width_dp: i32, dpi: u32) -> SnappedStroke {
    let scale = dpi as f32 / 96.0;
    let width_px = px(width_dp, dpi).max(1);
    let half_px = width_px as f32 / 2.0;
    let edge = |v: i32, inward: f32| (snap_px(v, dpi) as f32 + inward * half_px) / scale;
    SnappedStroke {
        rect: SnappedRect {
            left: edge(r.left, 1.0),
            top: edge(r.top, 1.0),
            right: edge(r.right, -1.0),
            bottom: edge(r.bottom, -1.0),
        },
        radius: ((radius_dp as f32 * scale - half_px) / scale).max(0.0),
        width: width_px as f32 / scale,
    }
}

/// The top of the band a focus line is cut to: far enough up that the box's
/// side and the same shape moved up by `width` are both still straight, so the
/// line thins to nothing there instead of ending in a step.
pub(super) fn focus_band_top(bottom: i32, radius: i32, width: i32) -> i32 {
    bottom - radius - width
}

// ---------------------------------------------------------------------------
// The paint list
// ---------------------------------------------------------------------------

/// Which font a line of text is drawn in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Face {
    /// Body: rows, headers, hints.
    Body,
    /// Caption: the footer.
    Caption,
    /// The icon font (Segoe Fluent Icons, Segoe MDL2 Assets as the fallback).
    Icon,
}

/// One thing to paint, in the order given.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Op {
    /// The whole window.
    Clear { color: u32 },
    /// A filled rounded rectangle.
    FillRound { rect: RECT, radius: i32, color: u32 },
    /// An outline drawn inside `rect`, `width` thick.
    StrokeRound {
        rect: RECT,
        radius: i32,
        width: i32,
        color: u32,
    },
    /// The bottom edge of the rounded box `rect`, `width` thick, following the
    /// corner arcs and thinning to nothing where they meet the sides (#1039):
    /// the box's shape less the same shape moved up by `width`, cut to the
    /// bottom band ([`focus_band_top`] down). It paints over the border there.
    FocusLine {
        rect: RECT,
        radius: i32,
        width: i32,
        color: u32,
    },
    /// One line, left aligned and centred top to bottom in `rect`.
    Text {
        rect: RECT,
        text: String,
        color: u32,
        face: Face,
    },
}

/// What the paint list is made from.
pub(super) struct PaintInput<'a> {
    pub(super) rows: &'a [Row],
    pub(super) offset: usize,
    pub(super) selected: usize,
    pub(super) hover: Option<usize>,
    pub(super) footer: &'a str,
    pub(super) colors: &'a PaletteColors,
}

/// Everything the palette paints at `dpi`, back to front.
pub(super) fn build_ops(dpi: u32, input: &PaintInput) -> Vec<Op> {
    let c = input.colors;
    let radius = px(CONTROL_RADIUS_DP, dpi);
    let line = px(1, dpi).max(1);
    let mut ops = vec![Op::Clear {
        color: c.background,
    }];

    // The query box: filled, outlined, a glyph, and the focus line under it
    // (the box is the only thing with focus while the palette is open).
    let b = box_rect(dpi);
    ops.push(Op::FillRound {
        rect: b,
        radius,
        color: c.box_fill,
    });
    ops.push(Op::StrokeRound {
        rect: b,
        radius,
        width: line,
        color: c.border,
    });
    let focus = px(FOCUS_LINE_DP, dpi).max(1);
    ops.push(Op::FocusLine {
        rect: b,
        radius,
        width: focus,
        color: c.focus_line,
    });
    ops.push(Op::Text {
        rect: glyph_rect(dpi),
        text: GLYPH_SEARCH.to_string(),
        color: c.glyph,
        face: Face::Icon,
    });

    // #217: only the rows inside [offset, offset + MAX_VISIBLE_ROWS) are
    // painted; `i` stays the row's real index into `rows` (what `selected`
    // compares against), while `pos` (i - offset) places it vertically.
    for (i, row) in input
        .rows
        .iter()
        .enumerate()
        .skip(input.offset)
        .take(MAX_VISIBLE_ROWS)
    {
        let pos = (i - input.offset) as i32;
        let slot = row_slot(dpi, pos);
        match row {
            // #951: a header and a hint start where the row text starts, so
            // a group reads as a label for what is under it. The header is
            // in the caption size, quieter than the rows.
            Row::Header(name) => ops.push(Op::Text {
                rect: text_slot(dpi, slot),
                text: name.clone(),
                color: c.header,
                face: Face::Caption,
            }),
            Row::Hint(text) => ops.push(Op::Text {
                rect: text_slot(dpi, slot),
                text: text.clone(),
                color: c.hint,
                face: Face::Body,
            }),
            Row::Action { name, .. } => {
                let fill = row_fill(dpi, pos);
                let selected = i == input.selected;
                // The selected row has the strong fill and the bar; a row only
                // hovered gets a quiet fill (an outline in high contrast), so
                // the two never look alike.
                if selected {
                    ops.push(Op::FillRound {
                        rect: fill,
                        radius,
                        color: c.selected_fill,
                    });
                } else if input.hover == Some(i) {
                    ops.push(Op::FillRound {
                        rect: fill,
                        radius,
                        color: c.hover_fill,
                    });
                    if let Some(ink) = c.hover_outline {
                        ops.push(Op::StrokeRound {
                            rect: fill,
                            radius,
                            width: line,
                            color: ink,
                        });
                    }
                }
                if let (true, Some(bar)) = (selected, c.bar) {
                    let h = px(BAR_HEIGHT_DP, dpi);
                    let w = px(BAR_WIDTH_DP, dpi).max(2);
                    let left = fill.left + px(BAR_INSET_DP, dpi);
                    let top = fill.top + (fill.bottom - fill.top - h) / 2;
                    ops.push(Op::FillRound {
                        rect: rect(left, top, left + w, top + h),
                        radius: w / 2,
                        color: bar,
                    });
                }
                ops.push(Op::Text {
                    rect: text_slot(dpi, slot),
                    text: name.clone(),
                    color: if selected { c.selected_text } else { c.text },
                    face: Face::Body,
                });
            }
        }
    }

    // #883: more rows than fit, so say where the viewport is.
    if let Some(t) = thumb_rect(dpi, input.offset, input.rows.len()) {
        ops.push(Op::FillRound {
            rect: t,
            radius: (t.right - t.left) / 2,
            color: c.thumb,
        });
    }

    ops.push(Op::Text {
        rect: footer_rect(dpi),
        text: input.footer.to_string(),
        color: c.footer,
        face: Face::Caption,
    });
    ops
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::palette_model::Row;
    use crate::ui::theme::{contrast_ratio, relative_luminance, HC_BLACK, HC_WHITE};

    const DPIS: [u32; 8] = [96, 120, 144, 168, 192, 216, 240, 288];

    fn action(name: &str) -> Row {
        Row::Action {
            id: name.to_lowercase(),
            name: name.to_string(),
            score: 0,
        }
    }

    fn rows(n: usize) -> Vec<Row> {
        (0..n).map(|i| action(&format!("Row {i}"))).collect()
    }

    fn colors(theme: Theme) -> PaletteColors {
        palette_colors(theme, &HC_BLACK, 0x00D4_7800)
    }

    fn ops_for(
        dpi: u32,
        rows: &[Row],
        offset: usize,
        selected: usize,
        hover: Option<usize>,
    ) -> Vec<Op> {
        let colors = colors(Theme::Dark);
        build_ops(
            dpi,
            &PaintInput {
                rows,
                offset,
                selected,
                hover,
                footer: "Auto",
                colors: &colors,
            },
        )
    }

    fn ratio(a: u32, b: u32) -> f64 {
        contrast_ratio(relative_luminance(a), relative_luminance(b))
    }

    /// The row fill of the selected row.
    fn selected_fill_op(ops: &[Op], color: u32) -> RECT {
        ops.iter()
            .find_map(|op| match op {
                Op::FillRound { rect, color: c, .. } if *c == color => Some(*rect),
                _ => None,
            })
            .expect("a selected row fill")
    }

    #[test]
    fn selected_and_hovered_rows_are_inset_and_rounded_at_every_scale() {
        let c = colors(Theme::Dark);
        for dpi in DPIS {
            let rows = rows(4);
            let ops = ops_for(dpi, &rows, 0, 1, Some(2));
            let radius = px(CONTROL_RADIUS_DP, dpi);
            for color in [c.selected_fill, c.hover_fill] {
                let fill = ops
                    .iter()
                    .find_map(|op| match op {
                        Op::FillRound {
                            rect,
                            radius: r,
                            color: col,
                        } if *col == color => {
                            assert_eq!(*r, radius, "dpi {dpi}: 4 dp corners");
                            Some(*rect)
                        }
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("dpi {dpi}: no fill {color:#08x}"));
                assert!(fill.left >= px(4, dpi), "dpi {dpi}: inset from the left");
                assert!(
                    window_width(dpi) - fill.right >= px(4, dpi),
                    "dpi {dpi}: inset from the right"
                );
                let slot_h = row_height(dpi);
                assert!(
                    fill.bottom - fill.top < slot_h,
                    "dpi {dpi}: room between rows"
                );
            }
        }
    }

    #[test]
    fn the_rows_and_the_query_box_share_a_left_and_right_edge() {
        for dpi in DPIS {
            let b = box_rect(dpi);
            let ops = ops_for(dpi, &rows(3), 0, 0, None);
            let fill = selected_fill_op(&ops, colors(Theme::Dark).selected_fill);
            assert_eq!((fill.left, fill.right), (b.left, b.right), "dpi {dpi}");
        }
    }

    #[test]
    fn the_glyph_the_row_text_and_the_footer_share_a_left_edge() {
        for dpi in DPIS {
            let ops = ops_for(dpi, &rows(3), 0, 0, None);
            let glyph = glyph_rect(dpi).left;
            let row_text = ops
                .iter()
                .find_map(|op| match op {
                    Op::Text { rect, text, .. } if text == "Row 0" => Some(rect.left),
                    _ => None,
                })
                .unwrap();
            assert_eq!(row_text, glyph, "dpi {dpi}: row text vs glyph");
            assert_eq!(footer_rect(dpi).left, glyph, "dpi {dpi}: footer vs glyph");
        }
    }

    #[test]
    fn group_headers_and_hints_start_where_row_text_starts_and_headers_are_smaller() {
        for dpi in DPIS {
            let rows = vec![
                Row::Header("Answer".to_string()),
                action("Check my work"),
                Row::Hint("Add a model".to_string()),
            ];
            let ops = ops_for(dpi, &rows, 0, 1, None);
            let find = |wanted: &str| {
                ops.iter()
                    .find_map(|op| match op {
                        Op::Text {
                            rect, text, face, ..
                        } if text == wanted => Some((rect.left, *face)),
                        _ => None,
                    })
                    .unwrap_or_else(|| panic!("dpi {dpi}: no text {wanted:?}"))
            };
            let (row_left, row_face) = find("Check my work");
            let (header_left, header_face) = find("Answer");
            let (hint_left, _) = find("Add a model");
            assert_eq!(header_left, row_left, "dpi {dpi}: header vs row text");
            assert_eq!(hint_left, row_left, "dpi {dpi}: hint vs row text");
            assert_eq!(row_face, Face::Body);
            assert_eq!(
                header_face,
                Face::Caption,
                "dpi {dpi}: smaller than the rows"
            );
        }
    }

    #[test]
    fn the_query_box_is_rounded_outlined_and_has_a_search_glyph() {
        let c = colors(Theme::Dark);
        for dpi in DPIS {
            let ops = ops_for(dpi, &[], 0, 0, None);
            let radius = px(CONTROL_RADIUS_DP, dpi);
            assert!(ops.contains(&Op::FillRound {
                rect: box_rect(dpi),
                radius,
                color: c.box_fill
            }));
            assert!(ops.contains(&Op::StrokeRound {
                rect: box_rect(dpi),
                radius,
                width: px(1, dpi).max(1),
                color: c.border
            }));
            assert!(
                ops.iter()
                    .any(|op| matches!(op, Op::Text { text, face: Face::Icon, .. }
                    if text == "\u{E721}")),
                "dpi {dpi}: the magnifier glyph"
            );
            // #1039: the focus line is the box's own bottom edge, corners
            // included, not a bar that stops where the corners begin.
            let b = box_rect(dpi);
            let (line, r, width) = ops
                .iter()
                .find_map(|op| match op {
                    Op::FocusLine {
                        rect,
                        radius,
                        width,
                        color,
                    } if *color == c.focus_line => Some((*rect, *radius, *width)),
                    _ => None,
                })
                .expect("a focus line");
            assert_eq!(line, b, "dpi {dpi}");
            assert_eq!(r, radius, "dpi {dpi}: the box's corner radius");
            assert!(width >= 1, "dpi {dpi}");
        }
    }

    #[test]
    fn the_focus_band_starts_above_where_the_corner_curve_does() {
        for dpi in DPIS {
            let b = box_rect(dpi);
            let (radius, width) = (px(CONTROL_RADIUS_DP, dpi), px(FOCUS_LINE_DP, dpi).max(1));
            let top = focus_band_top(b.bottom, radius, width);
            // The curve of the moved-up copy starts `width` higher than the
            // box's own, so the band reaches that far.
            assert!(top <= b.bottom - radius - width, "dpi {dpi}");
            assert!(top > b.top, "dpi {dpi}: inside the box");
        }
    }

    #[test]
    fn the_edit_sits_after_the_glyph_inside_the_box_and_is_centred() {
        for dpi in DPIS {
            let text_h = px(20, dpi);
            let (r, b, g) = (query_edit_rect(dpi, text_h), box_rect(dpi), glyph_rect(dpi));
            assert_eq!(r.bottom - r.top, text_h, "dpi {dpi}");
            assert!(r.left > g.right, "dpi {dpi}: clear of the glyph");
            assert!(r.right < b.right, "dpi {dpi}");
            let (above, below) = (r.top - b.top, b.bottom - r.bottom);
            assert!(above > 0 && (above - below).abs() <= 1, "dpi {dpi}");
            // The glyph is centred too.
            let (ga, gb) = (g.top - b.top, b.bottom - g.bottom);
            assert!((ga - gb).abs() <= 1, "dpi {dpi}");
        }
    }

    #[test]
    fn a_font_taller_than_the_box_is_clamped_to_the_box() {
        let r = query_edit_rect(96, 500);
        let b = box_rect(96);
        assert_eq!(r.bottom - r.top, b.bottom - b.top);
        let r = query_edit_rect(96, 0);
        assert!(
            r.bottom > r.top,
            "a zero-height font still leaves a control"
        );
    }

    #[test]
    fn the_footer_has_a_twelve_dp_margin_and_room_above_it() {
        for dpi in DPIS {
            let f = footer_rect(dpi);
            assert_eq!(window_height(dpi) - f.bottom, px(12, dpi), "dpi {dpi}");
            let last_fill = row_fill(dpi, MAX_VISIBLE_ROWS as i32 - 1);
            assert!(f.top - last_fill.bottom >= px(8, dpi), "dpi {dpi}");
            let ops = ops_for(dpi, &rows(3), 0, 0, None);
            assert!(
                matches!(ops.last(), Some(Op::Text { face: Face::Caption, text, .. }) if text == "Auto"),
                "the footer is the last line, in the caption face"
            );
        }
    }

    #[test]
    fn the_even_gap_under_the_query_box_is_eight_dp_and_the_window_adds_up() {
        for dpi in DPIS {
            assert_eq!(
                list_top(dpi) - box_rect(dpi).bottom,
                px(8, dpi),
                "dpi {dpi}"
            );
            assert_eq!(
                list_bottom(dpi) - list_top(dpi),
                row_height(dpi) * MAX_VISIBLE_ROWS as i32
            );
        }
    }

    #[test]
    fn the_scroll_thumb_is_a_pill_inset_from_the_edge_and_clear_of_the_rows() {
        for dpi in DPIS {
            let t = thumb_rect(dpi, 0, 40).unwrap();
            assert!(window_width(dpi) - t.right >= px(2, dpi), "dpi {dpi}");
            assert!(
                t.left >= row_slot(dpi, 0).right,
                "dpi {dpi}: clear of the rows"
            );
            assert_eq!(thumb_rect(dpi, 0, MAX_VISIBLE_ROWS), None, "12 rows fit");
            let rows = rows(40);
            let ops = ops_for(dpi, &rows, 0, 0, None);
            let pill = ops.iter().any(|op| {
                matches!(op, Op::FillRound { rect, radius, .. }
                    if *rect == t && *radius == (t.right - t.left) / 2)
            });
            assert!(pill, "dpi {dpi}: drawn rounded");
        }
    }

    #[test]
    fn nothing_is_painted_outside_the_window_at_any_scale() {
        for dpi in DPIS {
            let rows = rows(40);
            let ops = ops_for(dpi, &rows, 7, 9, Some(10));
            let (w, h) = (window_width(dpi), window_height(dpi));
            let mut seen = 0;
            for op in &ops {
                let r = match op {
                    Op::Clear { .. } => continue,
                    Op::FillRound { rect, .. }
                    | Op::StrokeRound { rect, .. }
                    | Op::FocusLine { rect, .. }
                    | Op::Text { rect, .. } => rect,
                };
                assert!(
                    r.left >= 0 && r.top >= 0 && r.right <= w && r.bottom <= h,
                    "dpi {dpi}: {op:?} leaves the {w}x{h} window"
                );
                assert!(
                    r.right > r.left && r.bottom > r.top,
                    "dpi {dpi}: {op:?} is empty"
                );
                seen += 1;
            }
            assert!(seen > 10, "the test must look at the shapes");
        }
    }

    #[test]
    fn hit_testing_agrees_with_the_painted_slots() {
        for dpi in DPIS {
            for pos in 0..MAX_VISIBLE_ROWS as i32 {
                let s = row_slot(dpi, pos);
                assert_eq!(
                    row_at(s.top, dpi, 3, 100),
                    Some(3 + pos as usize),
                    "dpi {dpi}"
                );
                assert_eq!(
                    row_at(s.bottom - 1, dpi, 3, 100),
                    Some(3 + pos as usize),
                    "dpi {dpi}"
                );
            }
            assert_eq!(row_at(list_top(dpi) - 1, dpi, 0, 10), None);
            assert_eq!(row_at(list_bottom(dpi), dpi, 0, 100), None);
            assert_eq!(row_at(list_top(dpi), dpi, 0, 0), None);
        }
    }

    #[test]
    fn only_the_selected_row_gets_the_bar_and_it_sits_inside_the_fill() {
        let c = colors(Theme::Light);
        for dpi in DPIS {
            let rows = rows(4);
            let colors = c;
            let ops = build_ops(
                dpi,
                &PaintInput {
                    rows: &rows,
                    offset: 0,
                    selected: 2,
                    hover: Some(1),
                    footer: "",
                    colors: &colors,
                },
            );
            let bars: Vec<RECT> = ops
                .iter()
                .filter_map(|op| match op {
                    Op::FillRound { rect, color, .. }
                        if Some(*color) == c.bar && rect.top >= list_top(dpi) =>
                    {
                        Some(*rect)
                    }
                    _ => None,
                })
                .collect();
            let fill = row_fill(dpi, 2);
            assert_eq!(bars.len(), 1, "dpi {dpi}");
            let bar = bars[0];
            assert!(bar.left > fill.left && bar.top >= fill.top && bar.bottom <= fill.bottom);
            assert!(
                bar.right < fill.left + px(TEXT_INSET_DP, dpi),
                "clear of the text"
            );
        }
    }

    #[test]
    fn high_contrast_uses_system_colours_and_no_decorative_fills() {
        for sys in [HC_BLACK, HC_WHITE] {
            let c = palette_colors(Theme::HighContrast, &sys, 0x00D4_7800);
            assert_eq!(c.box_fill, sys.window, "no tinted box");
            assert_eq!(c.background, sys.window);
            assert_eq!(c.bar, None, "no accent bar: the row is the highlight");
            assert_eq!(c.focus_line, sys.highlight);
            assert_eq!(c.hover_fill, c.background);
            let rows = rows(4);
            let ops = build_ops(
                96,
                &PaintInput {
                    rows: &rows,
                    offset: 0,
                    selected: 0,
                    hover: Some(1),
                    footer: "x",
                    colors: &c,
                },
            );
            let allowed = [
                c.background,
                c.box_fill,
                c.border,
                c.focus_line,
                c.glyph,
                c.text,
                c.selected_fill,
                c.selected_text,
                c.thumb,
                c.footer,
                c.header,
                c.hint,
                sys.window_text,
            ];
            for op in &ops {
                let color = match op {
                    Op::Clear { color }
                    | Op::FillRound { color, .. }
                    | Op::StrokeRound { color, .. }
                    | Op::FocusLine { color, .. }
                    | Op::Text { color, .. } => *color,
                };
                assert!(allowed.contains(&color), "{op:?} is not a system colour");
            }
            // The hover is an outline, not a fill.
            assert!(ops
                .iter()
                .any(|op| matches!(op, Op::StrokeRound { color, .. } if *color == c.text)));
        }
    }

    #[test]
    fn the_selection_is_neutral_and_only_the_bar_is_the_accent() {
        for theme in [Theme::Light, Theme::Dark] {
            let t = Tokens::for_theme(theme, &HC_BLACK);
            let c = palette_colors(theme, &HC_BLACK, 0x00D4_7800);
            // The tints are greys between the surface and the text colour.
            for tint in [c.selected_fill, c.hover_fill] {
                let (r, g, b) = (tint & 0xFF, (tint >> 8) & 0xFF, (tint >> 16) & 0xFF);
                assert!(r == g && g == b, "{theme:?}: {tint:#08x} is not neutral");
            }
            assert_ne!(c.bar, None);
            assert!(ratio(c.bar.unwrap(), t.surface) >= 3.0);
        }
    }

    #[test]
    fn text_stops_before_the_scroll_thumb_and_inside_the_row_fill() {
        let c = colors(Theme::Dark);
        for dpi in DPIS {
            let long = "x".repeat(90);
            let mut list = vec![Row::Header(long.clone()), Row::Hint(long.clone())];
            list.extend((0..30).map(|i| action(&format!("{long} {i}"))));
            let thumb = thumb_rect(dpi, 0, list.len()).expect("a thumb");
            let ops = build_ops(
                dpi,
                &PaintInput {
                    rows: &list,
                    offset: 0,
                    selected: 2,
                    hover: None,
                    footer: &long,
                    colors: &c,
                },
            );
            let mut seen = 0;
            for op in &ops {
                let Op::Text { rect, face, .. } = op else {
                    continue;
                };
                if *face == Face::Icon {
                    continue;
                }
                seen += 1;
                assert!(
                    rect.right <= thumb.left,
                    "dpi {dpi}: text to {} runs under the thumb at {}",
                    rect.right,
                    thumb.left
                );
                assert!(rect.right > rect.left, "dpi {dpi}: room for text");
            }
            // Row text keeps the same padding on the right as on the left, so
            // the ellipsis is inside the selection fill.
            let fill = row_fill(dpi, 2);
            let text = ops
                .iter()
                .find_map(|op| match op {
                    Op::Text { rect, text, .. } if text.ends_with(" 0") => Some(*rect),
                    _ => None,
                })
                .expect("the first action row");
            assert_eq!(fill.right - text.right, text.left - fill.left, "dpi {dpi}");
            assert!(seen >= 12, "dpi {dpi}: the test must see the text lines");
        }
    }

    #[test]
    fn every_face_but_the_icon_ends_in_an_ellipsis() {
        assert!(ellipsizes(Face::Body));
        assert!(ellipsizes(Face::Caption));
        assert!(!ellipsizes(Face::Icon));
    }

    #[test]
    fn a_one_dp_border_is_whole_pixels_at_150_and_250_percent() {
        for dpi in [96u32, 144, 168, 240] {
            let scale = dpi as f32 / 96.0;
            let ops = ops_for(96, &rows(3), 0, 0, None);
            let mut seen = 0;
            for op in &ops {
                let Op::StrokeRound {
                    rect,
                    radius,
                    width,
                    ..
                } = op
                else {
                    continue;
                };
                seen += 1;
                let s = snap_stroke(*rect, *radius, *width, dpi);
                let wpx = s.width * scale;
                assert!((wpx - wpx.round()).abs() < 1e-3, "dpi {dpi}: width {wpx}");
                assert!(wpx >= 1.0, "dpi {dpi}: a visible line");
                // The outer edge of the stroke is a whole pixel on all sides.
                let outer = [
                    (s.rect.left - s.width / 2.0) * scale,
                    (s.rect.top - s.width / 2.0) * scale,
                    (s.rect.right + s.width / 2.0) * scale,
                    (s.rect.bottom + s.width / 2.0) * scale,
                ];
                for e in outer {
                    assert!((e - e.round()).abs() < 1e-3, "dpi {dpi}: edge {e} px");
                }
                assert!(s.radius >= 0.0);
            }
            assert!(seen > 0, "the test must see the query box border");
        }
    }

    #[test]
    fn snapped_rects_have_whole_pixel_edges_and_stay_within_half_a_pixel() {
        let r = rect(16, 16, 544, 48);
        for dpi in [96u32, 144, 168, 240] {
            let scale = dpi as f32 / 96.0;
            let s = snap_rect(r, dpi);
            for (snapped, dp) in [
                (s.left, r.left),
                (s.top, r.top),
                (s.right, r.right),
                (s.bottom, r.bottom),
            ] {
                let p = snapped * scale;
                assert!((p - p.round()).abs() < 1e-3, "dpi {dpi}: {p}");
                assert!((p - dp as f32 * scale).abs() <= 0.5 + 1e-3, "dpi {dpi}");
            }
        }
        // 1 dp at 150 percent: the 0.5 dp inset used to be 0.75 px.
        let s = snap_stroke(rect(0, 0, 100, 40), 4, 1, 144);
        assert_eq!(s.width, 1.0 / 1.5);
    }

    #[test]
    fn the_text_box_text_and_glyph_are_readable_on_the_box_fill() {
        for (theme, sys) in [
            (Theme::Light, HC_BLACK),
            (Theme::Dark, HC_BLACK),
            (Theme::HighContrast, HC_BLACK),
            (Theme::HighContrast, HC_WHITE),
        ] {
            let c = palette_colors(theme, &sys, 0x00D4_7800);
            let t = Tokens::for_theme(theme, &sys);
            assert!(ratio(t.text, c.box_fill) >= 4.5, "{theme:?} typed text");
            assert!(ratio(c.cue, c.box_fill) >= 4.5, "{theme:?} placeholder");
            assert!(ratio(c.glyph, c.box_fill) >= 3.0, "{theme:?} glyph");
            assert!(
                ratio(c.border, c.background)
                    >= if theme == Theme::HighContrast {
                        3.0
                    } else {
                        1.0
                    }
            );
        }
    }
}
