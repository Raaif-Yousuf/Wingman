//! What one press of the Wingman key does, decided from what is on screen (#308).
//!
//! The "Options card" and "Looking" rows of the options card spec's key table,
//! single press only (double press and the window timer are a later slice, so
//! nothing here waits). Pure and without a `windows` dependency: `app.rs` reads
//! the card's state, asks [`on_key`], and does what it says.
//!
//! | State when the key is pressed | Single press |
//! |---|---|
//! | Looking (the router has not answered) | Cancel, no card |
//! | Options card, option 1 key-pickable | Run option 1 |
//! | Options card, option 1 not key-pickable | Dismiss |
//! | Anything else | Proceed: the primary action |

/// What the options card is doing when the key is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No options card. Any other card is left to the primary action, as
    /// before: the answer-card rows of the table belong to a later slice.
    Other,
    /// The options shell is up and the router has not answered.
    Looking,
    /// The options card shows its buttons.
    Options {
        /// Whether option 1 may be run by the key (see
        /// `palette_model::key_pickable`). `false` when it writes the
        /// clipboard, or when there is no option 1.
        first_key_pickable: bool,
    },
}

/// What the press does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    /// Run the primary action: the options card, or the pinned action.
    Proceed,
    /// Close the card and run nothing. Used for a cancel and for a dismiss:
    /// both end in no card and no message (a cancel is not an error).
    Close,
    /// Run option 1.
    RunFirst,
}

/// The single-press table.
pub fn on_key(phase: Phase) -> KeyAction {
    match phase {
        Phase::Other => KeyAction::Proceed,
        Phase::Looking => KeyAction::Close,
        Phase::Options {
            first_key_pickable: true,
        } => KeyAction::RunFirst,
        Phase::Options {
            first_key_pickable: false,
        } => KeyAction::Close,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_no_options_card_the_key_proceeds() {
        assert_eq!(on_key(Phase::Other), KeyAction::Proceed);
    }

    #[test]
    fn while_looking_the_key_cancels() {
        assert_eq!(on_key(Phase::Looking), KeyAction::Close);
    }

    #[test]
    fn a_key_pickable_first_option_is_run_by_the_key() {
        assert_eq!(
            on_key(Phase::Options {
                first_key_pickable: true
            }),
            KeyAction::RunFirst
        );
    }

    #[test]
    fn a_first_option_that_is_not_key_pickable_is_dismissed_never_run() {
        assert_eq!(
            on_key(Phase::Options {
                first_key_pickable: false
            }),
            KeyAction::Close
        );
    }
}
