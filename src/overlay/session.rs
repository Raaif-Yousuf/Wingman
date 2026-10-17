//! Starting and stopping the overlay's hooks and timer (spec slice 2: "when
//! `Overlay::show` returns `Err(Refused)`, the window code must remove every
//! hook and the timer named in `undo` before it shows the summary card alone").
//! The Win32 calls sit behind [`Steps`], so the rule that nothing stays
//! installed after a refusal or a hide is a unit test over a fake, not a
//! hope about `SetWinEventHook`.

use crate::capture::ScreenRect;

use super::state::{Decision, Input, Install, Overlay, Refused, Teardown};

/// The real installs and removals. `install` returns `false` when the step
/// could not be done, and must then leave nothing of that step behind.
pub trait Steps {
    fn install(&mut self, step: Install) -> bool;
    fn remove(&mut self, step: Install);
}

/// Shows the state machine through `steps`. On refusal everything that did
/// install is removed again, newest first, before the error comes back.
pub fn start(
    targets: Vec<isize>,
    own: Vec<isize>,
    labels: Vec<ScreenRect>,
    now_ms: u64,
    age_ms: u64,
    steps: &mut dyn Steps,
) -> Result<Overlay, Refused> {
    let shown = Overlay::show(targets, own, labels, now_ms, age_ms, &mut |step| {
        steps.install(step)
    });
    if let Err(refused) = &shown {
        for step in refused.undo.iter().rev() {
            steps.remove(*step);
        }
    }
    shown
}

/// Removes what a hide says to remove: the hooks newest first, then the timer.
pub fn finish(teardown: &Teardown, steps: &mut dyn Steps) {
    for hook in teardown.hooks.iter().rev() {
        steps.remove(Install::Hook(*hook));
    }
    if teardown.kill_age_timer {
        steps.remove(Install::AgeTimer);
    }
}

/// Feeds one input to the state machine and, if it hides the overlay, undoes
/// the installs. The single place a hide is carried out.
pub fn handle(overlay: &mut Overlay, input: Input, steps: &mut dyn Steps) -> Decision {
    let decision = overlay.on_input(input);
    if let Decision::Hide(_, teardown) = &decision {
        finish(teardown, steps);
    }
    decision
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::overlay::state::{HideReason, DEFAULT_AGE_MS, REQUIRED_HOOKS};

    /// A fake that records what is installed right now and what it was asked.
    #[derive(Default)]
    struct Fake {
        installed: Vec<Install>,
        removed: Vec<Install>,
        fail_on: Option<Install>,
    }

    impl Steps for Fake {
        fn install(&mut self, step: Install) -> bool {
            if self.fail_on == Some(step) {
                return false;
            }
            self.installed.push(step);
            true
        }
        fn remove(&mut self, step: Install) {
            self.installed.retain(|s| *s != step);
            self.removed.push(step);
        }
    }

    fn go(fake: &mut Fake) -> Result<Overlay, Refused> {
        start(vec![10], vec![99], vec![], 1_000, DEFAULT_AGE_MS, fake)
    }

    #[test]
    fn a_successful_start_leaves_every_hook_and_the_timer_installed() {
        let mut fake = Fake::default();
        let o = go(&mut fake).unwrap();
        assert!(o.is_visible());
        assert_eq!(fake.installed.len(), REQUIRED_HOOKS.len() + 1);
        assert!(fake.installed.contains(&Install::AgeTimer));
        assert!(fake.removed.is_empty());
    }

    #[test]
    fn a_timer_that_cannot_be_set_removes_every_hook_it_installed() {
        let mut fake = Fake {
            fail_on: Some(Install::AgeTimer),
            ..Fake::default()
        };
        let refused = go(&mut fake).unwrap_err();
        assert_eq!(refused.undo.len(), REQUIRED_HOOKS.len());
        assert!(
            fake.installed.is_empty(),
            "left behind: {:?}",
            fake.installed
        );
        assert_eq!(fake.removed.len(), REQUIRED_HOOKS.len());
        // Newest first.
        assert_eq!(
            fake.removed.first(),
            Some(&Install::Hook(*REQUIRED_HOOKS.last().unwrap()))
        );
    }

    #[test]
    fn a_hook_that_fails_part_way_removes_only_the_ones_before_it() {
        for failing in REQUIRED_HOOKS {
            let step = Install::Hook(failing);
            let mut fake = Fake {
                fail_on: Some(step),
                ..Fake::default()
            };
            let refused = go(&mut fake).unwrap_err();
            let before = REQUIRED_HOOKS.iter().position(|h| *h == failing).unwrap();
            assert_eq!(refused.undo.len(), before, "{failing:?}");
            assert!(
                fake.installed.is_empty(),
                "{failing:?} left {:?}",
                fake.installed
            );
            assert_eq!(fake.removed.len(), before);
            assert!(!fake.removed.contains(&step), "never installed");
        }
    }

    #[test]
    fn no_target_installs_nothing_and_removes_nothing() {
        let mut fake = Fake::default();
        let refused = start(vec![99], vec![99], vec![], 0, DEFAULT_AGE_MS, &mut fake).unwrap_err();
        assert!(refused.undo.is_empty());
        assert!(fake.installed.is_empty() && fake.removed.is_empty());
    }

    #[test]
    fn every_hide_path_removes_every_install() {
        let inputs = [
            Input::Esc,
            Input::AgeElapsed,
            Input::OwnerGone,
            Input::WindowMoved(10),
            Input::WindowDestroyed(10),
            Input::Minimized(10),
            Input::Foreground(500),
            Input::Wheel { over: 10 },
            Input::Click {
                x: 1,
                y: 1,
                over: 500,
            },
            Input::Key {
                foreground: 10,
                modifier: false,
            },
        ];
        for input in inputs {
            let mut fake = Fake::default();
            let mut o = go(&mut fake).unwrap();
            let d = handle(&mut o, input, &mut fake);
            assert!(matches!(d, Decision::Hide(..)), "{input:?} should hide");
            assert!(
                fake.installed.is_empty(),
                "{input:?} left {:?}",
                fake.installed
            );
            assert!(!o.is_visible());
        }
    }

    #[test]
    fn an_input_that_keeps_the_overlay_removes_nothing() {
        let mut fake = Fake::default();
        let mut o = go(&mut fake).unwrap();
        let d = handle(&mut o, Input::WindowMoved(777), &mut fake);
        assert_eq!(d, Decision::Keep);
        assert!(fake.removed.is_empty());
        assert_eq!(fake.installed.len(), REQUIRED_HOOKS.len() + 1);
    }

    #[test]
    fn hiding_twice_removes_once() {
        let mut fake = Fake::default();
        let mut o = go(&mut fake).unwrap();
        let first = handle(&mut o, Input::Esc, &mut fake);
        assert!(matches!(first, Decision::Hide(HideReason::Esc, _)));
        let removed = fake.removed.len();
        assert_eq!(handle(&mut o, Input::Esc, &mut fake), Decision::Keep);
        assert_eq!(fake.removed.len(), removed);
    }
}
