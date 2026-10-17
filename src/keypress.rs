//! What one press of the Wingman key does, decided from what is on screen
//! (#308, #393).
//!
//! The key table of the options card spec: a single press and a double press
//! (two presses inside `hotkeys.double_press_ms`), per card state and per key
//! mode. Pure and without a `windows` dependency: `app.rs` reads the card's
//! state, asks [`on_key`] (and [`needs_window`], to know whether to wait out
//! the window), and does what it says. The window itself is a one-shot timer
//! armed by `app.rs` after a press, never while idle (rule 5).
//!
//! | State when the key is pressed | Single | Double |
//! |---|---|---|
//! | No card (or a card this table does not cover) | Proceed: the primary action | same |
//! | Looking, inside the window of the press that started it | Absorb (the second tap) | n/a |
//! | Looking, inside the window, and a last action exists | Repeat it (#42) | n/a |
//! | Looking | Cancel, no card | Cancel, no card |
//! | Options card, option 1 key-pickable | Run option 1 | Run option 2 if key-pickable, else nothing |
//! | Options card, option 1 not key-pickable | Dismiss | Run option 2 if key-pickable, else dismiss |
//! | Answer, error or info card | Dismiss | Dismiss |
//!
//! Key modes change only the rows where a card is showing: Dynamic is the
//! table, Pick makes an answer card ignore the key, Close makes every card
//! (an options card too) just close.

/// Single press or double press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Single,
    Double,
}

/// `hotkeys.key_mode`: what the key does when a card is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyMode {
    /// Picks an option if there is one, otherwise closes the card.
    #[default]
    Dynamic,
    /// Always picks an option: as Dynamic on an options card, and does
    /// nothing on any other card.
    Pick,
    /// Always closes the card; nothing is run by the key.
    Close,
}

impl KeyMode {
    /// Reads the config value. Matched without regard to case or surrounding
    /// spaces; anything unknown is the default, Dynamic.
    pub fn parse(text: &str) -> Self {
        match text.trim().to_ascii_lowercase().as_str() {
            "pick" => Self::Pick,
            "close" => Self::Close,
            _ => Self::Dynamic,
        }
    }
}

/// What is on screen when the key is pressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    /// No card, or a card this table does not cover: the primary action, as
    /// before.
    Other,
    /// An answer, error or info card.
    Answer,
    /// The options shell is up and the router has not answered.
    Looking {
        /// Still inside the double-press window of the press that started the
        /// Look, so this press is the second tap of a double.
        early: bool,
    },
    /// The second tap of a double on a fresh Look, and the app has a last
    /// action to run again (#42). Without one this is `Looking { early: true }`.
    Repeat,
    /// The options card shows its buttons.
    Options {
        /// How many buttons there are.
        count: usize,
        /// Whether options 1 and 2 may be run by the key (see
        /// `palette_model::key_pickable`). `false` when one writes the
        /// clipboard, or does not exist.
        pickable: [bool; 2],
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
    /// Run this option (0-based).
    RunOption(usize),
    /// Run the last action again on a fresh look at the screen (#42). The app
    /// still checks the action may be run by the key (never a clipboard one).
    Repeat,
    /// Do nothing: the second tap of a double with nothing to repeat, or a
    /// key that must not act in this mode.
    Absorb,
}

/// The key table.
pub fn on_key(phase: Phase, mode: KeyMode, press: Press) -> KeyAction {
    match phase {
        Phase::Other => KeyAction::Proceed,
        Phase::Answer => match mode {
            KeyMode::Pick => KeyAction::Absorb,
            KeyMode::Dynamic | KeyMode::Close => KeyAction::Close,
        },
        Phase::Repeat => KeyAction::Repeat,
        Phase::Looking { early: true } => KeyAction::Absorb,
        Phase::Looking { early: false } => KeyAction::Close,
        Phase::Options { count, pickable } => {
            if mode == KeyMode::Close {
                return KeyAction::Close;
            }
            // With one button there is no option 2: a double is a single.
            let press = if count < 2 { Press::Single } else { press };
            // A flag for a button that is not there never counts.
            let (p1, p2) = (pickable[0] && count > 0, pickable[1] && count > 1);
            match (press, p1, p2) {
                (Press::Single, true, _) => KeyAction::RunOption(0),
                (Press::Single, false, _) => KeyAction::Close,
                (Press::Double, _, true) => KeyAction::RunOption(1),
                (Press::Double, true, false) => KeyAction::Absorb,
                (Press::Double, false, false) => KeyAction::Close,
            }
        }
    }
}

/// Whether a first press has to wait out the window to tell single from
/// double: only where the two would do different things, so everywhere else
/// the key acts at once and the window costs nothing.
pub fn needs_window(phase: Phase, mode: KeyMode) -> bool {
    on_key(phase, mode, Press::Single) != on_key(phase, mode, Press::Double)
}

/// The shortest and longest `hotkeys.double_press_ms` that is honoured.
pub const DOUBLE_PRESS_MIN_MS: u32 = 150;
pub const DOUBLE_PRESS_MAX_MS: u32 = 600;

/// The double-press window from the config value: `0` turns double press off
/// (`None`), anything else is clamped to 150 to 600 ms.
pub fn double_press_window_ms(configured: u32) -> Option<u32> {
    if configured == 0 {
        None
    } else {
        Some(configured.clamp(DOUBLE_PRESS_MIN_MS, DOUBLE_PRESS_MAX_MS))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use KeyMode::*;
    use Press::*;

    fn options(count: usize, p1: bool, p2: bool) -> Phase {
        Phase::Options {
            count,
            pickable: [p1, p2],
        }
    }

    const PICKS: [(bool, bool); 4] = [(true, true), (true, false), (false, true), (false, false)];

    #[test]
    fn with_no_options_card_the_key_proceeds_on_either_press() {
        for mode in [Dynamic, Pick, Close] {
            for press in [Single, Double] {
                assert_eq!(on_key(Phase::Other, mode, press), KeyAction::Proceed);
            }
        }
    }

    #[test]
    fn while_looking_the_key_cancels() {
        for mode in [Dynamic, Pick, Close] {
            for press in [Single, Double] {
                assert_eq!(
                    on_key(Phase::Looking { early: false }, mode, press),
                    KeyAction::Close
                );
            }
        }
    }

    #[test]
    fn the_second_tap_of_a_double_on_a_fresh_look_is_absorbed_not_a_cancel() {
        for mode in [Dynamic, Pick, Close] {
            assert_eq!(
                on_key(Phase::Looking { early: true }, mode, Single),
                KeyAction::Absorb
            );
        }
    }

    // #42: the second tap of a double on a fresh Look, with a last action to
    // repeat, repeats it in every mode. Nothing else repeats.
    #[test]
    fn the_second_tap_with_a_last_action_repeats_it_in_every_mode() {
        for mode in [Dynamic, Pick, Close] {
            for press in [Single, Double] {
                assert_eq!(on_key(Phase::Repeat, mode, press), KeyAction::Repeat);
            }
            assert!(!needs_window(Phase::Repeat, mode), "acts at once");
        }
        for phase in [
            Phase::Other,
            Phase::Answer,
            Phase::Looking { early: true },
            Phase::Looking { early: false },
            options(2, true, true),
        ] {
            for mode in [Dynamic, Pick, Close] {
                for press in [Single, Double] {
                    assert_ne!(on_key(phase, mode, press), KeyAction::Repeat, "{phase:?}");
                }
            }
        }
    }

    #[test]
    fn a_single_press_runs_option_one_only_when_the_key_may() {
        assert_eq!(
            on_key(options(3, true, true), Dynamic, Single),
            KeyAction::RunOption(0)
        );
        assert_eq!(
            on_key(options(3, false, true), Dynamic, Single),
            KeyAction::Close,
            "a clipboard option 1 is dismissed, never run"
        );
    }

    #[test]
    fn a_double_press_runs_option_two_when_the_key_may() {
        assert_eq!(
            on_key(options(3, true, true), Dynamic, Double),
            KeyAction::RunOption(1)
        );
        assert_eq!(
            on_key(options(3, false, true), Dynamic, Double),
            KeyAction::RunOption(1)
        );
    }

    #[test]
    fn a_double_press_with_option_two_not_pickable_does_nothing_or_dismisses() {
        assert_eq!(
            on_key(options(3, true, false), Dynamic, Double),
            KeyAction::Absorb
        );
        assert_eq!(
            on_key(options(3, false, false), Dynamic, Double),
            KeyAction::Close
        );
    }

    #[test]
    fn a_single_option_makes_a_double_the_same_as_a_single() {
        assert_eq!(
            on_key(options(1, true, false), Dynamic, Double),
            KeyAction::RunOption(0)
        );
        assert_eq!(
            on_key(options(1, false, false), Dynamic, Double),
            KeyAction::Close
        );
    }

    #[test]
    fn pick_mode_acts_on_an_options_card_as_dynamic_does() {
        for press in [Single, Double] {
            for (p1, p2) in PICKS {
                assert_eq!(
                    on_key(options(3, p1, p2), Pick, press),
                    on_key(options(3, p1, p2), Dynamic, press)
                );
            }
        }
    }

    #[test]
    fn close_mode_only_ever_closes_and_never_runs_an_option() {
        for press in [Single, Double] {
            assert_eq!(
                on_key(options(3, true, true), Close, press),
                KeyAction::Close
            );
            assert_eq!(on_key(Phase::Answer, Close, press), KeyAction::Close);
        }
    }

    #[test]
    fn an_answer_card_is_dismissed_by_the_key_unless_the_mode_is_pick() {
        for press in [Single, Double] {
            assert_eq!(on_key(Phase::Answer, Dynamic, press), KeyAction::Close);
            assert_eq!(on_key(Phase::Answer, Pick, press), KeyAction::Absorb);
        }
    }

    /// The key never runs an option that is not pickable, in any mode, press
    /// or combination: the clipboard guarantee of #859.
    #[test]
    fn a_not_key_pickable_option_is_never_run_by_any_combination() {
        let mut ran = 0;
        for mode in [Dynamic, Pick, Close] {
            for press in [Single, Double] {
                for count in 0..=4 {
                    for (p1, p2) in PICKS {
                        if let KeyAction::RunOption(i) = on_key(options(count, p1, p2), mode, press)
                        {
                            ran += 1;
                            assert!([p1, p2][i], "{mode:?} {press:?} {count} {p1} {p2}");
                            assert!(i < count);
                        }
                    }
                }
            }
        }
        assert!(ran > 0, "the loop must reach a run, or it checks nothing");
    }

    #[test]
    fn only_a_press_whose_single_and_double_differ_waits_for_the_window() {
        // Two buttons, one pickable: option 1 versus option 2.
        assert!(needs_window(options(2, true, true), Dynamic));
        assert!(needs_window(options(2, true, false), Dynamic));
        assert!(needs_window(options(2, false, true), Pick));
        // Nothing to tell apart: act at once.
        assert!(!needs_window(options(1, true, false), Dynamic));
        assert!(!needs_window(options(2, false, false), Dynamic));
        assert!(!needs_window(options(3, true, true), Close));
        assert!(!needs_window(Phase::Answer, Dynamic));
        assert!(!needs_window(Phase::Other, Dynamic));
        assert!(!needs_window(Phase::Looking { early: false }, Dynamic));
    }

    #[test]
    fn the_window_is_clamped_and_zero_turns_double_press_off() {
        assert_eq!(double_press_window_ms(0), None);
        assert_eq!(double_press_window_ms(300), Some(300));
        assert_eq!(double_press_window_ms(1), Some(150));
        assert_eq!(double_press_window_ms(9999), Some(600));
    }

    #[test]
    fn an_unknown_key_mode_is_dynamic() {
        assert_eq!(KeyMode::parse("pick"), Pick);
        assert_eq!(KeyMode::parse(" CLOSE "), Close);
        assert_eq!(KeyMode::parse("dynamic"), Dynamic);
        assert_eq!(KeyMode::parse("banana"), Dynamic);
        assert_eq!(KeyMode::parse(""), Dynamic);
    }
}
