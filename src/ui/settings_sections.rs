//! The Settings page's section layout (#1292): a page title, then titled
//! sections whose rows sit inside one rounded card, like the Windows 11
//! Settings app. Pure maths in dp (96 DPI units) so it can be tested; the
//! window code scales the results once, at creation, and again on a DPI change.

use crate::ui::theme::{SECTION_GAP_DP, SPACE_LG_DP, SPACE_SM_DP};

/// Height of the page title's line: the 20 px subtitle ramp step and its leading.
pub(crate) const PAGE_TITLE_H_DP: i32 = 28;
/// Gap under the page title.
pub(crate) const PAGE_TITLE_GAP_DP: i32 = SPACE_LG_DP;
/// Height of a section title's line: the 14 px body-strong ramp step.
pub(crate) const SECTION_TITLE_H_DP: i32 = 20;
/// Gap between a section title and its card.
pub(crate) const TITLE_CARD_GAP_DP: i32 = SPACE_SM_DP;
/// Space between a card's edge and the row text and controls inside it. The
/// rows keep the x and width every page's rows have, so the label and control
/// columns line up across pages (a test holds that): the card reaches out by
/// this much around them. Windows 11 uses 16; the page leaves room for 8.
pub(crate) const CARD_PAD_DP: i32 = SPACE_SM_DP;

/// Where the first section starts, given where the page title starts.
pub(crate) fn first_section_top(page_top: i32) -> i32 {
    page_top + PAGE_TITLE_H_DP + PAGE_TITLE_GAP_DP
}

/// Where a card starts, given where its section starts (the title is above it).
pub(crate) fn card_top(section_top: i32) -> i32 {
    section_top + SECTION_TITLE_H_DP + TITLE_CARD_GAP_DP
}

/// Where the next section starts, given where a card ends.
pub(crate) fn next_section_top(card_bottom: i32) -> i32 {
    card_bottom + SECTION_GAP_DP
}

/// The x and width of the card around rows at `x` of width `w`.
pub(crate) fn card_outer(x: i32, w: i32) -> (i32, i32) {
    (x - CARD_PAD_DP, w + 2 * CARD_PAD_DP)
}

/// The height a page of `sections` sections spends on titles and gaps, beyond
/// its rows: the page title and its gap, each section's title and the gap
/// under it, and the gap between two sections. Zero for a page with none.
#[cfg(test)]
pub(crate) fn chrome_h(sections: i32) -> i32 {
    if sections <= 0 {
        return 0;
    }
    PAGE_TITLE_H_DP
        + PAGE_TITLE_GAP_DP
        + sections * (SECTION_TITLE_H_DP + TITLE_CARD_GAP_DP)
        + (sections - 1) * SECTION_GAP_DP
}

/// A rectangle as `(x, y, w, h)`.
#[cfg(test)]
pub(crate) type Box4 = (i32, i32, i32, i32);

/// Whether `child` belongs to `card`: its centre lies inside the card.
#[cfg(test)]
pub(crate) fn card_holds(card: Box4, child: Box4) -> bool {
    let (cx, cy) = (child.0 + child.2 / 2, child.1 + child.3 / 2);
    cx >= card.0 && cx < card.0 + card.2 && cy >= card.1 && cy < card.1 + card.3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::theme::{
        contrast_ratio, relative_luminance, SysColors, Theme, Tokens, DARK, HC_BLACK, LIGHT,
    };

    #[test]
    fn the_chrome_of_a_page_is_the_titles_and_the_gaps() {
        assert_eq!(chrome_h(0), 0);
        assert_eq!(chrome_h(1), 28 + 16 + 28);
        assert_eq!(chrome_h(3), 28 + 16 + 3 * 28 + 2 * 24);
        // The same sum, walked down the page the way the window code does.
        let mut y = first_section_top(0);
        for i in 0..3 {
            if i > 0 {
                y = next_section_top(y);
            }
            y = card_top(y);
        }
        assert_eq!(y, chrome_h(3));
    }

    #[test]
    fn the_first_section_sits_under_the_page_title() {
        assert_eq!(first_section_top(8), 8 + 28 + 16);
    }

    #[test]
    fn a_card_starts_under_its_section_title() {
        assert_eq!(card_top(52), 52 + 20 + 8);
    }

    #[test]
    fn sections_are_24_dp_apart() {
        assert_eq!(next_section_top(300), 324);
        assert_eq!(crate::ui::theme::SPACE_XL_DP, 24);
    }

    #[test]
    fn a_card_reaches_out_around_its_rows_on_both_sides() {
        assert_eq!(card_outer(16, 588), (8, 604));
        let (x, w) = card_outer(40, 100);
        assert_eq!(40 - x, x + w - 140, "symmetric");
    }

    #[test]
    fn a_child_belongs_to_the_card_that_holds_its_centre() {
        let card = (10, 100, 400, 200);
        assert!(card_holds(card, (26, 110, 100, 22)));
        assert!(!card_holds(card, (26, 70, 100, 22)), "title above the card");
        assert!(!card_holds(card, (26, 310, 100, 22)), "row below the card");
        assert!(!card_holds(card, (500, 110, 100, 22)), "right of the card");
        assert!(
            card_holds(card, (10, 100, 0, 0)),
            "the top left pixel is in"
        );
        assert!(!card_holds(card, (410, 100, 0, 0)), "the right edge is out");
    }

    #[test]
    fn the_card_layer_is_off_the_page_colour_in_light_and_dark() {
        assert_ne!(LIGHT.card, LIGHT.surface);
        assert_ne!(DARK.card, DARK.surface);
        assert!(relative_luminance(LIGHT.card) > relative_luminance(LIGHT.surface));
        assert!(relative_luminance(DARK.card) > relative_luminance(DARK.surface));
    }

    #[test]
    fn text_reads_on_the_card_in_every_theme() {
        let hc = Tokens::for_theme(Theme::HighContrast, &HC_BLACK);
        assert!(contrast_ratio(relative_luminance(hc.text), relative_luminance(hc.card)) >= 7.0);
        for t in [LIGHT, DARK] {
            // Labels and the helper lines (`hint_text_color` is `text_dim`).
            for ink in [t.text, t.text_dim] {
                assert!(contrast_ratio(relative_luminance(ink), relative_luminance(t.card)) >= 4.5);
            }
        }
    }

    #[test]
    fn high_contrast_takes_the_card_from_the_system_colours() {
        let sys = SysColors {
            window: 0x0012_3456,
            window_text: 0x0065_4321,
            ..HC_BLACK
        };
        let t = Tokens::for_theme(Theme::HighContrast, &sys);
        assert_eq!(t.card, sys.window);
        assert_eq!(t.card_stroke, sys.window_text);
    }

    #[test]
    fn the_stroke_is_visible_against_the_card_without_shouting() {
        for t in [LIGHT, DARK] {
            let r = contrast_ratio(
                relative_luminance(t.card_stroke),
                relative_luminance(t.card),
            );
            assert!(r > 1.05 && r < 3.0, "stroke contrast {r}");
        }
    }
}
