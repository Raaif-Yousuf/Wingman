//! The Models page's choice of what to show (#874), kept apart from the Win32
//! code so it can be tested without a window.
//!
//! The page shows the active provider's rows. A vendor that has a key is
//! always shown, and "Add another provider" shows the rest. The controls stay
//! alive when hidden, so Save still reads them and a value the user typed
//! survives a switch back and forth.

use crate::config::ActiveProvider;

/// The blocks of the Providers group that come and go, top to bottom, plus the
/// two kinds of control that only move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Slot {
    OpenAi,
    Anthropic,
    Gemini,
    Ollama,
    /// The "Add another provider" button.
    AddButton,
    /// The line about adding a missing model, with Open config file.
    Hint,
    /// No control is made under it: `slot_dy(Frame)` is how far the page
    /// shrinks in all, which the rows under the blocks move by.
    Frame,
    /// The Ollama card under the rows (status, Pull, link): shows with the
    /// Ollama block, and its top moves up with every hidden row.
    OllamaCard,
}

/// How many rows each flowing slot takes.
fn rows(slot: Slot) -> i32 {
    match slot {
        Slot::OpenAi | Slot::Anthropic | Slot::Gemini => 3,
        Slot::Ollama | Slot::AddButton | Slot::Hint => 1,
        Slot::Frame | Slot::OllamaCard => 0,
    }
}

/// The flowing slots in the order they stack.
const FLOW: [Slot; 6] = [
    Slot::OpenAi,
    Slot::Anthropic,
    Slot::Gemini,
    Slot::Ollama,
    Slot::AddButton,
    Slot::Hint,
];

/// Which provider blocks show, as `[OpenAI, Anthropic, Gemini, Ollama]`.
/// `configured` is whether each already has something of the user's: a key
/// stored or typed for a cloud vendor, a place in `providers.order` for Ollama.
/// With no recognisable active provider nothing can be picked out, so all show.
pub(crate) fn shown_providers(
    active: Option<ActiveProvider>,
    configured: [bool; 4],
    revealed: bool,
) -> [bool; 4] {
    let Some(active) = active else {
        return [true; 4];
    };
    if revealed {
        return [true; 4];
    }
    [
        active == ActiveProvider::OpenAi || configured[0],
        active == ActiveProvider::Anthropic || configured[1],
        active == ActiveProvider::Gemini || configured[2],
        active == ActiveProvider::Ollama || configured[3],
    ]
}

/// Whether "Add another provider" is offered: something is hidden and the user
/// has not asked for it yet.
pub(crate) fn add_button_shown(shown: [bool; 4], revealed: bool) -> bool {
    !revealed && shown.iter().any(|s| !s)
}

/// The first provider block (index into `[OpenAI, Anthropic, Gemini, Ollama]`)
/// that was hidden `before` and shows `after`: where focus goes when the
/// button that had it disappears.
pub(crate) fn first_revealed(before: [bool; 4], after: [bool; 4]) -> Option<usize> {
    (0..4).find(|i| !before[*i] && after[*i])
}

/// Whether `slot` itself is visible.
pub(crate) fn slot_shown(slot: Slot, shown: [bool; 4], add: bool) -> bool {
    match slot {
        Slot::OpenAi => shown[0],
        Slot::Anthropic => shown[1],
        Slot::Gemini => shown[2],
        Slot::Ollama | Slot::OllamaCard => shown[3],
        Slot::AddButton => add,
        Slot::Hint | Slot::Frame => true,
    }
}

/// How far (dp, zero or negative) `slot` moves up from its place when every
/// slot shows. `pitch` is one row's height with its gap. The frame's height
/// and the controls below it change by the whole hidden height.
pub(crate) fn slot_dy(slot: Slot, shown: [bool; 4], add: bool, pitch: i32) -> i32 {
    let hidden_rows: i32 = FLOW
        .iter()
        .take_while(|s| **s != slot)
        .filter(|s| !slot_shown(**s, shown, add))
        .map(|s| rows(*s))
        .sum();
    let hidden_rows = if matches!(slot, Slot::Frame | Slot::OllamaCard) {
        FLOW.iter()
            .filter(|s| !slot_shown(**s, shown, add))
            .map(|s| rows(*s))
            .sum()
    } else {
        hidden_rows
    };
    -hidden_rows * pitch
}

#[cfg(test)]
mod tests {
    use super::*;
    use ActiveProvider::*;

    #[test]
    fn only_the_active_provider_shows_with_no_keys() {
        assert_eq!(
            shown_providers(Some(OpenAi), [false; 4], false),
            [true, false, false, false]
        );
        assert_eq!(
            shown_providers(Some(Anthropic), [false; 4], false),
            [false, true, false, false]
        );
        assert_eq!(
            shown_providers(Some(Gemini), [false; 4], false),
            [false, false, true, false]
        );
        assert_eq!(
            shown_providers(Some(Ollama), [false; 4], false),
            [false, false, false, true]
        );
    }

    #[test]
    fn ollama_in_the_order_keeps_its_block_and_card() {
        assert_eq!(
            shown_providers(Some(OpenAi), [false, false, false, true], false),
            [true, false, false, true]
        );
        assert!(slot_shown(
            Slot::OllamaCard,
            [true, false, false, true],
            true
        ));
        assert!(!slot_shown(
            Slot::OllamaCard,
            [true, false, false, false],
            true
        ));
    }

    #[test]
    fn a_vendor_with_a_key_always_shows() {
        assert_eq!(
            shown_providers(Some(Anthropic), [true, false, true, false], false),
            [true, true, true, false]
        );
    }

    #[test]
    fn revealing_or_an_unknown_active_provider_shows_everything() {
        assert_eq!(shown_providers(Some(OpenAi), [false; 4], true), [true; 4]);
        assert_eq!(shown_providers(None, [false; 4], false), [true; 4]);
    }

    #[test]
    fn the_add_button_only_shows_while_something_is_hidden() {
        assert!(add_button_shown([true, false, false, false], false));
        assert!(!add_button_shown([true, false, false, false], true));
        assert!(!add_button_shown([true; 4], false));
    }

    #[test]
    fn one_vendor_shown_moves_the_rest_up_by_every_hidden_row() {
        let shown = [true, false, false, false];
        let pitch = 28;
        assert_eq!(slot_dy(Slot::OpenAi, shown, true, pitch), 0);
        // Anthropic and Gemini (6 rows) and the Ollama row are hidden.
        assert_eq!(slot_dy(Slot::AddButton, shown, true, pitch), -7 * pitch);
        assert_eq!(slot_dy(Slot::Hint, shown, true, pitch), -7 * pitch);
        assert_eq!(slot_dy(Slot::Frame, shown, true, pitch), -7 * pitch);
        assert_eq!(slot_dy(Slot::OllamaCard, shown, true, pitch), -7 * pitch);
    }

    #[test]
    fn a_shown_block_moves_up_by_the_hidden_blocks_above_it() {
        let shown = [false, true, false, false];
        assert_eq!(slot_dy(Slot::Anthropic, shown, true, 10), -30);
        assert_eq!(slot_dy(Slot::Gemini, shown, true, 10), -30);
    }

    #[test]
    fn with_everything_shown_the_blocks_stay_put() {
        for s in [Slot::OpenAi, Slot::Anthropic, Slot::Gemini, Slot::Ollama] {
            assert_eq!(slot_dy(s, [true; 4], false, 28), 0, "{s:?}");
        }
    }

    #[test]
    fn focus_goes_to_the_first_block_that_just_appeared() {
        assert_eq!(
            first_revealed([true, false, false, false], [true; 4]),
            Some(1)
        );
        assert_eq!(first_revealed([true; 4], [true; 4]), None);
    }

    #[test]
    fn a_gone_add_button_closes_its_row() {
        assert_eq!(slot_dy(Slot::Hint, [true; 4], false, 28), -28);
        assert_eq!(slot_dy(Slot::Frame, [true; 4], false, 28), -28);
    }
}
