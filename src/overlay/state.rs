//! The overlay's staleness state machine (spec "When the screen changes").
//! Pure: it holds the targets, the clickable label rectangles and a record of
//! the hooks and timer that are installed, and decides what an input means.
//! The Win32 side (slice 2) installs and removes the real hooks to match the
//! record, and posts every hook callback's input to the main thread, which
//! calls [`Overlay::on_input`]. Callbacks never decide.
//!
//! Window handles are plain `isize` values here, so the table can be tested
//! with a fake event stream.

use crate::capture::ScreenRect;

/// How long after the overlay appears it hides itself, at most (spec config
/// `overlay.auto_dismiss_secs`, default 30, clamped 5 to 300).
pub const DEFAULT_AGE_MS: u64 = 30_000;
/// The Esc claim outlives the overlay's age cap by this much, so it lapses by
/// itself even if a hide path is somehow missed.
pub const CLAIM_GRACE_MS: u64 = 1_000;

/// Every hook the overlay installs while it is visible, and nothing else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    /// `EVENT_OBJECT_LOCATIONCHANGE`
    LocationChange,
    /// `EVENT_OBJECT_DESTROY`
    Destroy,
    /// `EVENT_SYSTEM_MINIMIZESTART`
    MinimizeStart,
    /// `EVENT_SYSTEM_FOREGROUND`
    Foreground,
    /// `WH_MOUSE_LL` (the existing watcher, armed for wheel and click)
    Mouse,
    /// `WH_KEYBOARD_LL` (the existing hook)
    Keyboard,
}

/// What the table needs installed, in install order.
pub const REQUIRED_HOOKS: [HookKind; 6] = [
    HookKind::LocationChange,
    HookKind::Destroy,
    HookKind::MinimizeStart,
    HookKind::Foreground,
    HookKind::Mouse,
    HookKind::Keyboard,
];

/// One input, as posted by a hook callback or timer to the main thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    /// A window moved, resized, maximised or restored (`OBJID_WINDOW`).
    WindowMoved(isize),
    WindowDestroyed(isize),
    Minimized(isize),
    /// Another window came to the foreground.
    Foreground(isize),
    /// The wheel turned with the pointer over this top-level window.
    Wheel {
        over: isize,
    },
    /// A mouse button went down at this point, over this top-level window.
    Click {
        x: i32,
        y: i32,
        over: isize,
    },
    /// A key went down while `foreground` was the foreground window.
    Key {
        foreground: isize,
        modifier: bool,
    },
    /// The one-shot age timer fired.
    AgeElapsed,
    /// Esc, through the bounded claim.
    Esc,
    /// The summary card or the preview it belongs to went away.
    OwnerGone,
}

/// Why the overlay hid, so the summary card can say so.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HideReason {
    Esc,
    /// A click that was not on a label.
    Click,
    /// The screen under the marks changed (a move, a scroll, a key, another
    /// app). The card's line becomes "Marks hidden, the screen changed".
    Changed,
    Age,
    OwnerGone,
}

/// What the Win32 side must undo when the overlay hides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Teardown {
    pub hooks: Vec<HookKind>,
    pub kill_age_timer: bool,
    pub clear_esc_claim: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Keep,
    /// A click on label number `.0`: a pick, not a dismissal.
    Label(usize),
    Hide(HideReason, Teardown),
}

#[derive(Debug)]
pub struct Overlay {
    targets: Vec<isize>,
    own: Vec<isize>,
    labels: Vec<ScreenRect>,
    hooks: Vec<HookKind>,
    age_timer: bool,
    esc_claim_expiry_ms: Option<u64>,
}

impl Overlay {
    /// Starts an overlay: installs each hook in [`REQUIRED_HOOKS`] through
    /// `install` and arms the age timer. `targets` are the top-level windows
    /// under the marks, `own` Wingman's windows (never targets), `labels` the
    /// clickable label rects (empty for a read-only set), `now_ms` a
    /// monotonic tick.
    ///
    /// If any hook cannot be installed the overlay is refused: `Err` carries
    /// the hooks that did install, for the caller to remove. An overlay with
    /// no way to go stale is the one outcome to avoid.
    pub fn show(
        targets: Vec<isize>,
        own: Vec<isize>,
        labels: Vec<ScreenRect>,
        now_ms: u64,
        age_ms: u64,
        install: &mut dyn FnMut(HookKind) -> bool,
    ) -> Result<Self, Vec<HookKind>> {
        let mut hooks = Vec::new();
        for kind in REQUIRED_HOOKS {
            if install(kind) {
                hooks.push(kind);
            } else {
                return Err(hooks);
            }
        }
        let targets: Vec<isize> = targets.into_iter().filter(|t| !own.contains(t)).collect();
        Ok(Self {
            targets,
            own,
            labels,
            hooks,
            age_timer: true,
            esc_claim_expiry_ms: Some(now_ms.saturating_add(age_ms).saturating_add(CLAIM_GRACE_MS)),
        })
    }

    /// Whether the overlay is showing (hooks or timer still recorded).
    pub fn is_visible(&self) -> bool {
        !self.hooks.is_empty() || self.age_timer || self.esc_claim_expiry_ms.is_some()
    }

    /// The hooks the record says are installed. Empty after any hide.
    pub fn installed_hooks(&self) -> &[HookKind] {
        &self.hooks
    }

    pub fn age_timer_armed(&self) -> bool {
        self.age_timer
    }

    /// What `input` means. A hidden overlay keeps everything.
    pub fn on_input(&mut self, input: Input) -> Decision {
        if !self.is_visible() {
            return Decision::Keep;
        }
        let is_target = |w: isize| self.targets.contains(&w);
        let reason = match input {
            Input::WindowMoved(w) | Input::WindowDestroyed(w) | Input::Minimized(w)
                if is_target(w) =>
            {
                HideReason::Changed
            }
            Input::Foreground(w) if !is_target(w) && !self.own.contains(&w) => HideReason::Changed,
            Input::Wheel { over } if is_target(over) => HideReason::Changed,
            Input::Click { x, y, .. } => {
                // A label is hit-tested first: a click on one is a pick.
                let on_label = self
                    .labels
                    .iter()
                    .position(|l| l.left <= x && x < l.right && l.top <= y && y < l.bottom);
                if let Some(i) = on_label {
                    return Decision::Label(i);
                }
                HideReason::Click
            }
            Input::Key {
                foreground,
                modifier: false,
            } if is_target(foreground) => HideReason::Changed,
            Input::AgeElapsed => HideReason::Age,
            Input::Esc => HideReason::Esc,
            Input::OwnerGone => HideReason::OwnerGone,
            _ => return Decision::Keep,
        };
        Decision::Hide(reason, self.hide())
    }

    /// The single hide function: every path ends here. Clears the record and
    /// says what the caller must undo.
    pub fn hide(&mut self) -> Teardown {
        Teardown {
            hooks: std::mem::take(&mut self.hooks),
            kill_age_timer: std::mem::take(&mut self.age_timer),
            clear_esc_claim: self.esc_claim_expiry_ms.take().is_some(),
        }
    }

    /// Whether the keyboard hook should swallow this Esc: only with no
    /// modifiers down, only while showing, and only until the expiry tick.
    pub fn esc_claimed(&self, modifiers_down: bool, now_ms: u64) -> bool {
        !modifiers_down && self.esc_claim_expiry_ms.is_some_and(|e| now_ms < e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TERM: isize = 100;
    const EDITOR: isize = 200;
    const OTHER: isize = 300;
    const CARD: isize = 900;

    fn r(left: i32, top: i32, right: i32, bottom: i32) -> ScreenRect {
        ScreenRect {
            left,
            top,
            right,
            bottom,
        }
    }

    fn shown() -> Overlay {
        Overlay::show(
            vec![TERM, EDITOR],
            vec![CARD],
            vec![r(10, 10, 60, 30)],
            1_000,
            DEFAULT_AGE_MS,
            &mut |_| true,
        )
        .unwrap()
    }

    fn hides(o: &mut Overlay, i: Input) -> HideReason {
        match o.on_input(i) {
            Decision::Hide(reason, _) => reason,
            other => panic!("{i:?} should hide, got {other:?}"),
        }
    }

    fn keeps(o: &mut Overlay, i: Input) {
        assert_eq!(o.on_input(i), Decision::Keep, "{i:?} should keep");
    }

    #[test]
    fn showing_installs_every_hook_and_arms_the_timer() {
        let o = shown();
        assert_eq!(o.installed_hooks(), &REQUIRED_HOOKS);
        assert!(o.age_timer_armed());
        assert!(o.is_visible());
    }

    #[test]
    fn a_hook_that_cannot_install_refuses_the_overlay_and_names_what_to_undo() {
        let mut tried = Vec::new();
        let r = Overlay::show(vec![TERM], vec![], vec![], 0, DEFAULT_AGE_MS, &mut |k| {
            tried.push(k);
            k != HookKind::Foreground
        });
        let undo = r.unwrap_err();
        assert_eq!(
            undo,
            vec![
                HookKind::LocationChange,
                HookKind::Destroy,
                HookKind::MinimizeStart
            ]
        );
        // Nothing after the failure was tried.
        assert_eq!(tried.last(), Some(&HookKind::Foreground));
        // The very first hook failing leaves nothing to undo.
        let r = Overlay::show(vec![TERM], vec![], vec![], 0, 5, &mut |_| false);
        assert_eq!(r.unwrap_err(), Vec::<HookKind>::new());
    }

    #[test]
    fn a_window_in_the_target_set_moving_hides_it() {
        for i in [
            Input::WindowMoved(TERM),
            Input::WindowDestroyed(EDITOR),
            Input::Minimized(TERM),
        ] {
            assert_eq!(hides(&mut shown(), i), HideReason::Changed, "{i:?}");
        }
    }

    #[test]
    fn a_window_outside_the_target_set_moving_does_not() {
        let mut o = shown();
        keeps(&mut o, Input::WindowMoved(OTHER));
        keeps(&mut o, Input::WindowDestroyed(OTHER));
        keeps(&mut o, Input::Minimized(OTHER));
        keeps(&mut o, Input::WindowMoved(CARD));
        assert!(o.is_visible());
    }

    #[test]
    fn another_app_coming_forward_hides_but_a_target_or_our_own_window_does_not() {
        assert_eq!(
            hides(&mut shown(), Input::Foreground(OTHER)),
            HideReason::Changed
        );
        let mut o = shown();
        keeps(&mut o, Input::Foreground(TERM));
        keeps(&mut o, Input::Foreground(EDITOR));
        // The form preview card takes focus (#329): the ghost text stays.
        keeps(&mut o, Input::Foreground(CARD));
    }

    #[test]
    fn the_wheel_hides_only_over_a_target() {
        assert_eq!(
            hides(&mut shown(), Input::Wheel { over: EDITOR }),
            HideReason::Changed
        );
        let mut o = shown();
        keeps(&mut o, Input::Wheel { over: OTHER });
        keeps(&mut o, Input::Wheel { over: CARD });
    }

    #[test]
    fn a_click_off_the_labels_hides_wherever_it_lands() {
        for over in [TERM, OTHER, CARD] {
            assert_eq!(
                hides(
                    &mut shown(),
                    Input::Click {
                        x: 500,
                        y: 500,
                        over
                    }
                ),
                HideReason::Click
            );
        }
    }

    #[test]
    fn a_click_on_a_label_is_a_pick_and_is_never_dismissed() {
        let mut o = shown();
        assert_eq!(
            o.on_input(Input::Click {
                x: 10,
                y: 10,
                over: TERM
            }),
            Decision::Label(0)
        );
        // Right and bottom edges are exclusive.
        assert_eq!(
            hides(
                &mut o,
                Input::Click {
                    x: 60,
                    y: 29,
                    over: TERM
                }
            ),
            HideReason::Click
        );
        assert!(!o.is_visible());
    }

    #[test]
    fn a_key_in_a_target_hides_but_a_modifier_or_a_key_in_our_own_card_does_not() {
        assert_eq!(
            hides(
                &mut shown(),
                Input::Key {
                    foreground: TERM,
                    modifier: false
                }
            ),
            HideReason::Changed
        );
        let mut o = shown();
        keeps(
            &mut o,
            Input::Key {
                foreground: TERM,
                modifier: true,
            },
        );
        keeps(
            &mut o,
            Input::Key {
                foreground: CARD,
                modifier: false,
            },
        );
        keeps(
            &mut o,
            Input::Key {
                foreground: OTHER,
                modifier: false,
            },
        );
    }

    #[test]
    fn age_esc_and_the_owner_going_away_each_hide() {
        assert_eq!(hides(&mut shown(), Input::AgeElapsed), HideReason::Age);
        assert_eq!(hides(&mut shown(), Input::Esc), HideReason::Esc);
        assert_eq!(hides(&mut shown(), Input::OwnerGone), HideReason::OwnerGone);
    }

    #[test]
    fn our_own_window_is_never_a_target_even_if_listed() {
        let mut o = Overlay::show(
            vec![TERM, CARD],
            vec![CARD],
            vec![],
            0,
            DEFAULT_AGE_MS,
            &mut |_| true,
        )
        .unwrap();
        keeps(&mut o, Input::WindowMoved(CARD));
        keeps(
            &mut o,
            Input::Key {
                foreground: CARD,
                modifier: false,
            },
        );
    }

    #[test]
    fn no_hook_timer_or_claim_remains_after_any_hide_path() {
        let inputs = [
            Input::WindowMoved(TERM),
            Input::WindowDestroyed(TERM),
            Input::Minimized(EDITOR),
            Input::Foreground(OTHER),
            Input::Wheel { over: TERM },
            Input::Click {
                x: 0,
                y: 0,
                over: OTHER,
            },
            Input::Key {
                foreground: EDITOR,
                modifier: false,
            },
            Input::AgeElapsed,
            Input::Esc,
            Input::OwnerGone,
        ];
        for i in inputs {
            let mut o = shown();
            let Decision::Hide(_, undo) = o.on_input(i) else {
                panic!("{i:?} should hide");
            };
            // The teardown names everything that was installed...
            assert_eq!(undo.hooks, REQUIRED_HOOKS.to_vec(), "{i:?}");
            assert!(undo.kill_age_timer && undo.clear_esc_claim, "{i:?}");
            // ...and the record is empty afterwards.
            assert!(o.installed_hooks().is_empty(), "{i:?}");
            assert!(!o.age_timer_armed(), "{i:?}");
            assert!(!o.is_visible(), "{i:?}");
            assert!(!o.esc_claimed(false, 1_001), "{i:?}");
        }
        // The direct hide function (a caller-initiated hide) is the same.
        let mut o = shown();
        let undo = o.hide();
        assert_eq!(undo.hooks, REQUIRED_HOOKS.to_vec());
        assert!(!o.is_visible());
    }

    #[test]
    fn a_hidden_overlay_keeps_everything_and_hides_once() {
        let mut o = shown();
        hides(&mut o, Input::Esc);
        keeps(&mut o, Input::Esc);
        keeps(&mut o, Input::WindowMoved(TERM));
        assert_eq!(
            o.hide(),
            Teardown {
                hooks: vec![],
                kill_age_timer: false,
                clear_esc_claim: false
            }
        );
    }

    #[test]
    fn the_esc_claim_needs_no_modifiers_a_visible_overlay_and_an_unexpired_tick() {
        let o = shown(); // shown at 1_000, age 30_000, grace 1_000
        assert!(o.esc_claimed(false, 1_000));
        assert!(o.esc_claimed(false, 31_999));
        assert!(
            !o.esc_claimed(true, 1_000),
            "Ctrl/Alt/Shift+Esc is not ours"
        );
        assert!(!o.esc_claimed(false, 32_000), "lapses by itself");
        assert!(!o.esc_claimed(false, u64::MAX));
    }

    #[test]
    fn huge_ticks_do_not_overflow() {
        let o = Overlay::show(vec![], vec![], vec![], u64::MAX, u64::MAX, &mut |_| true).unwrap();
        assert!(o.esc_claimed(false, u64::MAX - 1));
    }
}
