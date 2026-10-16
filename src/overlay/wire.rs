//! How an input travels from a hook callback to the main thread (spec "Where
//! each decision is made"). A callback never decides: it posts
//! `WM_APP_OVERLAY_INPUT` with a [`Kind`] in `wparam` and one number in
//! `lparam`, and the main thread decodes it here, looks up the windows it
//! needs (`GetForegroundWindow`, `WindowFromPoint`: never inside a hook
//! callback), and asks the state machine what it means.
//!
//! Pure over a [`Windows`] lookup, so the whole table is tested with a fake.

use super::state::Input;

/// What happened, as posted in `wparam`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(usize)]
pub enum Kind {
    /// A mouse button went down. `lparam` is the packed screen point.
    Click = 1,
    /// The wheel turned. `lparam` is the packed screen point.
    Wheel = 2,
    /// A key that is not a modifier went down. `lparam` is its virtual key.
    Key = 3,
    /// Esc, claimed by the keyboard hook. `lparam` is unused.
    Esc = 4,
    /// A window moved or resized. `lparam` is its handle.
    Moved = 5,
    /// A window was destroyed. `lparam` is its handle.
    Destroyed = 6,
    /// A window was minimised. `lparam` is its handle.
    Minimized = 7,
    /// A window came to the foreground. `lparam` is its handle.
    Foreground = 8,
}

impl Kind {
    pub fn from_wparam(w: usize) -> Option<Kind> {
        Some(match w {
            1 => Kind::Click,
            2 => Kind::Wheel,
            3 => Kind::Key,
            4 => Kind::Esc,
            5 => Kind::Moved,
            6 => Kind::Destroyed,
            7 => Kind::Minimized,
            8 => Kind::Foreground,
            _ => return None,
        })
    }
}

/// The window lookups the main thread makes. All answers are raw handle
/// values; 0 means none.
pub trait Windows {
    /// The top-level window at a screen point.
    fn root_at(&self, x: i32, y: i32) -> isize;
    /// The foreground window's top-level window.
    fn foreground(&self) -> isize;
    /// The top-level window `h` belongs to, or `h` itself if it has none
    /// (a window that is already being destroyed).
    fn root_of(&self, h: isize) -> isize;
}

/// Decodes a posted input into what the state machine understands. `None`
/// for a `wparam` that is not a [`Kind`] (a stray message is dropped).
pub fn to_input(wparam: usize, lparam: isize, windows: &dyn Windows) -> Option<Input> {
    let kind = Kind::from_wparam(wparam)?;
    Some(match kind {
        Kind::Click => {
            let (x, y) = crate::dismiss::unpack_point(lparam as u32);
            Input::Click {
                x,
                y,
                over: windows.root_at(x, y),
            }
        }
        Kind::Wheel => {
            let (x, y) = crate::dismiss::unpack_point(lparam as u32);
            Input::Wheel {
                over: windows.root_at(x, y),
            }
        }
        Kind::Key => Input::Key {
            foreground: windows.foreground(),
            modifier: false,
        },
        Kind::Esc => Input::Esc,
        Kind::Moved => Input::WindowMoved(windows.root_of(lparam)),
        // A destroyed window has no ancestor to ask about: its own handle.
        Kind::Destroyed => Input::WindowDestroyed(lparam),
        Kind::Minimized => Input::Minimized(windows.root_of(lparam)),
        Kind::Foreground => Input::Foreground(windows.root_of(lparam)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capture::ScreenRect;
    use crate::dismiss::pack_point;
    use crate::overlay::state::{Decision, HideReason, Overlay, DEFAULT_AGE_MS};

    const TARGET: isize = 100;
    const OTHER: isize = 300;
    const OWN: isize = 900;
    const CHILD: isize = 101;

    /// Everything at x < 500 is the target window, the rest is another app.
    struct Fake {
        foreground: isize,
    }

    impl Windows for Fake {
        fn root_at(&self, x: i32, _y: i32) -> isize {
            if x < 500 {
                TARGET
            } else {
                OTHER
            }
        }
        fn foreground(&self) -> isize {
            self.foreground
        }
        fn root_of(&self, h: isize) -> isize {
            if h == CHILD {
                TARGET
            } else {
                h
            }
        }
    }

    fn overlay() -> Overlay {
        Overlay::show(
            vec![TARGET],
            vec![OWN],
            vec![ScreenRect {
                left: 10,
                top: 10,
                right: 60,
                bottom: 30,
            }],
            0,
            DEFAULT_AGE_MS,
            &mut |_| true,
        )
        .unwrap()
    }

    fn decide(kind: Kind, lparam: isize, foreground: isize) -> Decision {
        let input = to_input(kind as usize, lparam, &Fake { foreground }).unwrap();
        overlay().on_input(input)
    }

    fn hides(d: &Decision) -> Option<HideReason> {
        match d {
            Decision::Hide(r, _) => Some(*r),
            _ => None,
        }
    }

    #[test]
    fn every_row_of_the_staleness_table_hides_or_keeps_as_the_spec_says() {
        let at = |x, y| pack_point(x, y) as isize;
        // Window events hide only for a window in the target set; an event
        // for a child window counts as its top-level window.
        assert_eq!(
            hides(&decide(Kind::Moved, TARGET, OTHER)),
            Some(HideReason::Changed)
        );
        assert_eq!(
            hides(&decide(Kind::Moved, CHILD, OTHER)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Moved, OTHER, OTHER)), None);
        assert_eq!(
            hides(&decide(Kind::Destroyed, TARGET, OTHER)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Destroyed, OTHER, OTHER)), None);
        assert_eq!(
            hides(&decide(Kind::Minimized, TARGET, OTHER)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Minimized, OTHER, OTHER)), None);
        // Another app in front hides; the target or our own window does not.
        assert_eq!(
            hides(&decide(Kind::Foreground, OTHER, OTHER)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Foreground, TARGET, TARGET)), None);
        assert_eq!(hides(&decide(Kind::Foreground, OWN, OWN)), None);
        // The wheel hides over the target only.
        assert_eq!(
            hides(&decide(Kind::Wheel, at(200, 200), TARGET)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Wheel, at(700, 200), TARGET)), None);
        // A key hides while the target is in front, not while our card is.
        assert_eq!(
            hides(&decide(Kind::Key, 0x41, TARGET)),
            Some(HideReason::Changed)
        );
        assert_eq!(hides(&decide(Kind::Key, 0x41, OWN)), None);
        assert_eq!(hides(&decide(Kind::Key, 0x41, OTHER)), None);
        // A click anywhere off a label hides; Esc hides.
        assert_eq!(
            hides(&decide(Kind::Click, at(700, 200), TARGET)),
            Some(HideReason::Click)
        );
        assert_eq!(
            hides(&decide(Kind::Click, at(200, 200), TARGET)),
            Some(HideReason::Click)
        );
        assert_eq!(hides(&decide(Kind::Esc, 0, TARGET)), Some(HideReason::Esc));
    }

    #[test]
    fn a_click_on_a_label_is_a_pick_and_never_a_dismissal() {
        let d = decide(Kind::Click, pack_point(30, 20) as isize, TARGET);
        assert_eq!(d, Decision::Label(0));
    }

    #[test]
    fn a_click_left_of_the_primary_monitor_keeps_its_sign() {
        let input = to_input(
            Kind::Click as usize,
            pack_point(-1900, -40) as isize,
            &Fake { foreground: 0 },
        )
        .unwrap();
        assert!(matches!(
            input,
            Input::Click {
                x: -1900,
                y: -40,
                ..
            }
        ));
    }

    #[test]
    fn a_stray_wparam_is_dropped_not_guessed() {
        for w in [0usize, 9, 99, usize::MAX] {
            assert!(to_input(w, 0, &Fake { foreground: 0 }).is_none());
        }
    }

    #[test]
    fn every_kind_round_trips_through_its_wparam() {
        for k in [
            Kind::Click,
            Kind::Wheel,
            Kind::Key,
            Kind::Esc,
            Kind::Moved,
            Kind::Destroyed,
            Kind::Minimized,
            Kind::Foreground,
        ] {
            assert_eq!(Kind::from_wparam(k as usize), Some(k));
        }
    }
}
