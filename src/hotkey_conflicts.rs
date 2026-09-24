//! Issue #112: hotkey conflict detection. Consulted from `app.rs`'s
//! `on_learned` when learn mode (`hotkey.rs`) captures a new chord for
//! `hotkeys.primary`/`hotkeys.secondary`.
//!
//! Two layers, the module's usual split (AGENTS.md rule 8):
//!
//! - **Pure** (this file, top-level): a fixed table of known system and
//!   common-app global shortcuts ([`known_bindings`]), a lookup
//!   ([`check`]), and the learn-mode decision ([`decide`]) -- all plain
//!   `Chord` values in, no Win32.
//! - **Win32** ([`win32`]): a best-effort `RegisterHotKey` probe. Exercised
//!   by hand only (see that module's doc comment for exactly what it can
//!   and cannot detect); this file's own table above is the primary
//!   detection mechanism precisely because the probe's coverage is so
//!   narrow.
//!
//! # The flow (no dialogs, AGENTS.md rule 7's "every failure ends in a
//! card", never a blocking prompt)
//!
//! The **first** time learn mode captures a chord that [`check`] finds in
//! the table, `app.rs` shows a card naming the owner and does **not** apply
//! the new binding -- the previous binding stays in effect, exactly like
//! nothing happened except the card. If the user runs learn mode again and
//! captures the **exact same** chord a second time in a row, that repeat is
//! read as "yes, I meant it" and the binding is applied despite the
//! conflict. Binding to the Copilot key's own chord (`Win+Shift+F23`) is
//! never treated as a conflict at all -- see [`OwnerKind::CopilotKey`].

use crate::hotkey::Chord;

/// What kind of thing owns a known binding -- changes how [`check`] treats
/// it, not just how it is displayed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OwnerKind {
    /// A Windows shell/system shortcut (Task Manager, lock, Snipping Tool,
    /// ...).
    System,
    /// A shortcut conventionally used by common third-party apps (a
    /// browser's own accelerator, for instance) rather than the OS itself.
    App,
    /// `Win+Shift+F23`, the Copilot key itself. Listed here so the table is
    /// a complete record of "keys with a fixed meaning on this machine",
    /// but [`check`] never reports it as a conflict -- rebinding back to it
    /// is the expected, common case, not a collision with another app.
    CopilotKey,
}

struct KnownBinding {
    chord: Chord,
    owner: &'static str,
    kind: OwnerKind,
}

const fn chord(vk: u32, ctrl: bool, shift: bool, alt: bool, win: bool) -> Chord {
    Chord {
        vk,
        ctrl,
        shift,
        alt,
        win,
    }
}

// Virtual-key codes used below, standard Windows values (matching
// `hotkey.rs`'s own `VK_*` constants, not imported from `windows` so this
// table stays free of any Win32 type -- same rationale as
// `inputs::selection`'s pure section).
const VK_ESCAPE: u32 = 0x1B;
const VK_TAB: u32 = 0x09;
const VK_DELETE: u32 = 0x2E;
const VK_F4: u32 = 0x73;
const VK_F23: u32 = 0x86;
const VK_PERIOD: u32 = 0xBE;

fn known_bindings() -> &'static [KnownBinding] {
    use OwnerKind::*;
    const TABLE: &[KnownBinding] = &[
        // -- System (Windows shell / OS) -----------------------------------
        KnownBinding {
            chord: chord(VK_ESCAPE, true, true, false, false), // Ctrl+Shift+Esc
            owner: "Task Manager",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'L' as u32, false, false, false, true), // Win+L
            owner: "Lock screen",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'D' as u32, false, false, false, true), // Win+D
            owner: "Show desktop",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'E' as u32, false, false, false, true), // Win+E
            owner: "File Explorer",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'R' as u32, false, false, false, true), // Win+R
            owner: "Run dialog",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'S' as u32, false, true, false, true), // Win+Shift+S
            owner: "Snipping Tool",
            kind: System,
        },
        KnownBinding {
            chord: chord(VK_DELETE, true, false, true, false), // Ctrl+Alt+Del
            owner: "Windows security screen",
            kind: System,
        },
        KnownBinding {
            chord: chord(VK_TAB, false, false, true, false), // Alt+Tab
            owner: "Task switcher",
            kind: System,
        },
        KnownBinding {
            chord: chord(VK_TAB, false, false, false, true), // Win+Tab
            owner: "Task view",
            kind: System,
        },
        KnownBinding {
            chord: chord(b'V' as u32, false, false, false, true), // Win+V
            owner: "Clipboard history",
            kind: System,
        },
        KnownBinding {
            chord: chord(VK_PERIOD, false, false, false, true), // Win+.
            owner: "Emoji panel",
            kind: System,
        },
        KnownBinding {
            chord: chord(VK_F4, false, false, true, false), // Alt+F4
            owner: "Close window",
            kind: System,
        },
        // -- Common apps (browsers, mostly) --------------------------------
        KnownBinding {
            chord: chord(b'N' as u32, true, true, false, false), // Ctrl+Shift+N
            owner: "most browsers: new private/incognito window",
            kind: App,
        },
        KnownBinding {
            chord: chord(b'T' as u32, true, true, false, false), // Ctrl+Shift+T
            owner: "most browsers: reopen last closed tab",
            kind: App,
        },
        KnownBinding {
            chord: chord(b'B' as u32, true, true, false, false), // Ctrl+Shift+B
            owner: "most browsers: toggle bookmarks bar",
            kind: App,
        },
        // -- Wingman's own key, expected, never a conflict -------------------
        KnownBinding {
            chord: chord(VK_F23, false, true, false, true), // Win+Shift+F23
            owner: "the Copilot key itself",
            kind: CopilotKey,
        },
    ];
    TABLE
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Conflict {
    pub owner: &'static str,
    pub kind: OwnerKind,
}

/// Looks `chord` up in the known-bindings table. `None` means either
/// nothing in the table matches, or the only match is
/// [`OwnerKind::CopilotKey`] (never reported as a conflict -- see the
/// module doc comment).
pub fn check(chord: Chord) -> Option<Conflict> {
    known_bindings()
        .iter()
        .find(|b| b.chord == chord)
        .filter(|b| !matches!(b.kind, OwnerKind::CopilotKey))
        .map(|b| Conflict {
            owner: b.owner,
            kind: b.kind,
        })
}

/// What `app.rs`'s `on_learned` should do with a freshly captured chord for
/// binding slot `which`, given `pending` -- the `(slot, chord)` pair last
/// warned about (`None` if nothing has been warned about, or the most
/// recent learn either applied cleanly or warned about a *different* pair).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LearnDecision {
    /// No conflict, or the same conflicting chord confirmed a second time in
    /// a row: apply it.
    Apply,
    /// A new conflict: warn and keep the previous binding. The caller should
    /// remember `(which, chord)` as the new `pending` so a repeat of the
    /// exact same chord next time applies it instead.
    Warn(Conflict),
}

pub fn decide(pending: Option<(usize, Chord)>, which: usize, chord: Chord) -> LearnDecision {
    match check(chord) {
        None => LearnDecision::Apply,
        Some(conflict) => {
            if pending == Some((which, chord)) {
                LearnDecision::Apply
            } else {
                LearnDecision::Warn(conflict)
            }
        }
    }
}

pub mod win32 {
    //! A best-effort `RegisterHotKey` probe: register a chord against a
    //! scratch id on a window the caller owns, then immediately unregister
    //! it, with no lasting side effect either way. Exercised by hand, named
    //! per AGENTS.md rule 8 -- see [`probe_available`]'s doc comment for the
    //! manual check.
    //!
    //! # What this can and cannot detect
    //!
    //! - **Detects:** another process's own live `RegisterHotKey`
    //!   registration of the exact same chord -- a well-behaved app that
    //!   registers its global shortcuts the documented way.
    //! - **Does NOT detect:** the shell's own baked-in shortcuts (`Win+L`,
    //!   `Win+D`, `Win+E`, `Win+Tab`, `Win+Shift+S`, `Win+.`, ...) -- none of
    //!   these are implemented via `RegisterHotKey` at all, so this probe
    //!   reports them as free even though Windows itself intercepts them
    //!   first. Also does not detect an app's own accelerator table, or
    //!   another low-level keyboard hook (like Wingman's own) -- neither of
    //!   those calls `RegisterHotKey` either. `Ctrl+Alt+Del` is intercepted
    //!   by Winlogon below the level any `RegisterHotKey` caller could ever
    //!   observe.
    //! - This is exactly why [`super::known_bindings`]'s fixed table, not
    //!   this probe, is the primary detection mechanism -- this probe is a
    //!   narrow supplementary signal for the one case it can see, not a
    //!   substitute for the table.
    //!
    //! `THEORY (unverified)`: whether registering and immediately
    //! unregistering can itself cause a visible flicker (e.g. momentarily
    //! preventing the real owner's hotkey from firing) has not been
    //! measured on this machine.

    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
        MOD_SHIFT, MOD_WIN,
    };

    use crate::hotkey::Chord;

    /// Registers `chord` against `hwnd` under `scratch_id`, then immediately
    /// unregisters it. Returns `true` when the registration succeeded
    /// (meaning: at that instant, no other `RegisterHotKey` caller held this
    /// exact chord), `false` on failure (most likely
    /// `ERROR_HOTKEY_ALREADY_REGISTERED`).
    ///
    /// **Manual check** (`wired-to-nothing` skill): run two copies of a
    /// throwaway `RegisterHotKey(Ctrl+Shift+F13)` test harness at once --
    /// the second `RegisterHotKey` call must fail, and this function must
    /// return `false` for it. Filed as a manual step under issue #166 rather
    /// than exercised automatically: it needs two real processes racing for
    /// the same hotkey, which a `cargo test` binary cannot set up safely
    /// (AGENTS.md rule 9 -- it also must never collide with a hotkey the
    /// developer's own machine actually uses).
    // Not called anywhere yet: this probe is a narrow supplementary signal
    // (see this module's doc comment), and #112's Done-when is satisfied by
    // the fixed table above alone. Wiring a caller (e.g. a background check
    // before `on_learned` applies a binding) is a follow-up, not this
    // issue's scope -- kept here, tested, and exercised by hand (see this
    // function's own doc comment) so it's ready when that wiring lands.
    #[allow(dead_code)]
    pub fn probe_available(hwnd: HWND, chord: &Chord, scratch_id: i32) -> bool {
        let mut mask = MOD_NOREPEAT.0;
        if chord.ctrl {
            mask |= MOD_CONTROL.0;
        }
        if chord.alt {
            mask |= MOD_ALT.0;
        }
        if chord.shift {
            mask |= MOD_SHIFT.0;
        }
        if chord.win {
            mask |= MOD_WIN.0;
        }
        let modifiers = HOT_KEY_MODIFIERS(mask);

        let ok = unsafe { RegisterHotKey(Some(hwnd), scratch_id, modifiers, chord.vk) }.is_ok();
        if ok {
            let _ = unsafe { UnregisterHotKey(Some(hwnd), scratch_id) };
        }
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(vk: u32, ctrl: bool, shift: bool, alt: bool, win: bool) -> Chord {
        chord(vk, ctrl, shift, alt, win)
    }

    // -- table lookups ---------------------------------------------------

    #[test]
    fn task_manager_conflict_is_detected() {
        let conflict = check(c(VK_ESCAPE, true, true, false, false)).unwrap();
        assert_eq!(conflict.owner, "Task Manager");
        assert_eq!(conflict.kind, OwnerKind::System);
    }

    #[test]
    fn win_l_conflict_is_detected() {
        let conflict = check(c(b'L' as u32, false, false, false, true)).unwrap();
        assert_eq!(conflict.owner, "Lock screen");
    }

    #[test]
    fn an_unbound_chord_has_no_conflict() {
        // Ctrl+Shift+/ -- not in the table.
        assert_eq!(check(c(0xBF, true, true, false, false)), None);
    }

    #[test]
    fn the_copilot_key_itself_is_never_a_conflict() {
        assert_eq!(check(c(VK_F23, false, true, false, true)), None);
    }

    #[test]
    fn table_of_known_conflicts() {
        let cases: &[(Chord, &str)] = &[
            (c(VK_ESCAPE, true, true, false, false), "Task Manager"),
            (c(b'L' as u32, false, false, false, true), "Lock screen"),
            (c(b'D' as u32, false, false, false, true), "Show desktop"),
            (c(b'E' as u32, false, false, false, true), "File Explorer"),
            (c(b'R' as u32, false, false, false, true), "Run dialog"),
            (c(b'S' as u32, false, true, false, true), "Snipping Tool"),
            (
                c(VK_DELETE, true, false, true, false),
                "Windows security screen",
            ),
            (c(VK_TAB, false, false, true, false), "Task switcher"),
            (c(VK_TAB, false, false, false, true), "Task view"),
            (
                c(b'V' as u32, false, false, false, true),
                "Clipboard history",
            ),
            (c(VK_PERIOD, false, false, false, true), "Emoji panel"),
            (c(VK_F4, false, false, true, false), "Close window"),
        ];
        for (chord, owner) in cases {
            let conflict = check(*chord).unwrap_or_else(|| panic!("{chord:?} should conflict"));
            assert_eq!(conflict.owner, *owner, "{chord:?}");
        }
    }

    #[test]
    fn app_owned_bindings_are_marked_app_not_system() {
        let conflict = check(c(b'N' as u32, true, true, false, false)).unwrap();
        assert_eq!(conflict.kind, OwnerKind::App);
    }

    // -- decide: the learn-mode flow --------------------------------------

    #[test]
    fn no_conflict_always_applies() {
        let free = c(0xBF, true, true, false, false);
        assert_eq!(decide(None, HK_PRIMARY_TEST, free), LearnDecision::Apply);
    }

    #[test]
    fn a_first_time_conflict_warns_and_does_not_apply() {
        let taskmgr = c(VK_ESCAPE, true, true, false, false);
        let decision = decide(None, HK_PRIMARY_TEST, taskmgr);
        assert_eq!(
            decision,
            LearnDecision::Warn(Conflict {
                owner: "Task Manager",
                kind: OwnerKind::System,
            })
        );
    }

    #[test]
    fn repeating_the_same_conflicting_chord_on_the_same_slot_applies_it() {
        let taskmgr = c(VK_ESCAPE, true, true, false, false);
        let pending = Some((HK_PRIMARY_TEST, taskmgr));
        assert_eq!(
            decide(pending, HK_PRIMARY_TEST, taskmgr),
            LearnDecision::Apply
        );
    }

    #[test]
    fn a_different_pending_conflict_still_warns() {
        let taskmgr = c(VK_ESCAPE, true, true, false, false);
        let lock = c(b'L' as u32, false, false, false, true);
        // Pending was a warn for `lock`; capturing `taskmgr` next is a
        // DIFFERENT conflict and must warn again, not silently apply.
        let pending = Some((HK_PRIMARY_TEST, lock));
        assert_eq!(
            decide(pending, HK_PRIMARY_TEST, taskmgr),
            LearnDecision::Warn(Conflict {
                owner: "Task Manager",
                kind: OwnerKind::System,
            })
        );
    }

    #[test]
    fn the_same_pending_chord_on_a_different_slot_still_warns() {
        // #112 conflicts are per-binding-slot: confirming the conflict for
        // the primary key must not silently apply it to the secondary key
        // too.
        let taskmgr = c(VK_ESCAPE, true, true, false, false);
        let pending = Some((HK_PRIMARY_TEST, taskmgr));
        assert_eq!(
            decide(pending, HK_SECONDARY_TEST, taskmgr),
            LearnDecision::Warn(Conflict {
                owner: "Task Manager",
                kind: OwnerKind::System,
            })
        );
    }

    #[test]
    fn rebinding_to_the_copilot_key_never_warns_even_if_pending_is_set() {
        let copilot = c(VK_F23, false, true, false, true);
        let pending = Some((HK_PRIMARY_TEST, c(VK_ESCAPE, true, true, false, false)));
        assert_eq!(
            decide(pending, HK_PRIMARY_TEST, copilot),
            LearnDecision::Apply
        );
    }

    // Local stand-ins for `hotkey::HK_PRIMARY`/`HK_SECONDARY` so this test
    // module has no dependency on their exact numeric values.
    const HK_PRIMARY_TEST: usize = 1;
    const HK_SECONDARY_TEST: usize = 2;
}
