//! The key claim for the options card (#307).
//!
//! The options card never takes focus (so typing keeps going to the app
//! underneath), which means keys 1 to 4 and Esc cannot reach it as window
//! messages. The low-level keyboard hook in `hotkey.rs` claims them instead,
//! but only while the card shows and only under the bounds the spec sets:
//! no modifier is down, the digit names an option that exists, the claim has
//! not expired, and the event is real input (not injected).
//!
//! The claim is one lock-free atomic, because the hook runs on every key
//! press system wide and Windows removes a hook that takes too long. It holds
//! the claim's expiry tick, so a claim left set by a bug lapses by itself with
//! no timer: the hook compares the key event's own timestamp with it.
//!
//! Everything that decides is pure and tested here; `hotkey.rs` only reads the
//! state of the modifier keys and posts the result.

use std::sync::atomic::{AtomicIsize, AtomicU64, Ordering};

use windows::Win32::UI::WindowsAndMessaging::WM_APP;

/// Posted to the main window when the user picks an option or dismisses the
/// options card (#307), by the card itself on a click and by the keyboard
/// hook on a claimed key. `wparam` is the choice: `0` closes the card, `1` to
/// `4` is the option, counted from 1. `lparam` is the generation of the card
/// the choice was made on (`GENERATION_MASK` bits), so a message that outlives
/// its card (the card was replaced, or its router reply arrived and changed
/// the buttons) runs nothing. No payload, so nothing to free when it is
/// dropped. Adding another `WM_APP_*` constant anywhere in the crate also
/// means adding it to `app.rs`'s `tests::ALL_WM_APP_IDS` and its
/// `count_declarations` file list.
pub const WM_APP_OPTIONS_CHOICE: u32 = WM_APP + 23;

/// The most options a card shows, and so the highest digit claimed.
pub const MAX_OPTIONS: usize = 4;

/// Generations are 24 bits so the claim fits one atomic.
pub const GENERATION_MASK: u32 = 0x00FF_FFFF;

/// The generation that follows `generation`, within [`GENERATION_MASK`].
pub fn next_generation(generation: u32) -> u32 {
    generation.wrapping_add(1) & GENERATION_MASK
}

/// What the options card is willing to have the hook swallow right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Claim {
    /// Which card showing this claim belongs to ([`GENERATION_MASK`] bits).
    pub generation: u32,
    /// How many options have buttons: digits `1..=count` are claimed. Zero
    /// while the card is still looking: only Esc is claimed then.
    pub count: usize,
    /// The tick (`GetTickCount` milliseconds) from which nothing is claimed.
    pub expiry_tick: u32,
}

/// The state of the four modifier groups, read by the hook per claimed key.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Modifiers {
    pub shift: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub win: bool,
}

impl Modifiers {
    fn any(self) -> bool {
        self.shift || self.ctrl || self.alt || self.win
    }
}

/// What a claimed key means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Choice {
    /// Esc: close the card, run nothing.
    Dismiss,
    /// Digit `n` (counted from 1): run that option.
    Pick(usize),
}

impl Choice {
    /// The `wparam` of [`WM_APP_OPTIONS_CHOICE`].
    pub fn to_wparam(self) -> usize {
        match self {
            Choice::Dismiss => 0,
            Choice::Pick(n) => n,
        }
    }

    /// Reads a `wparam`. `None` for a number no card offers.
    pub fn from_wparam(w: usize) -> Option<Choice> {
        match w {
            0 => Some(Choice::Dismiss),
            1..=MAX_OPTIONS => Some(Choice::Pick(w)),
            _ => None,
        }
    }
}

const VK_ESCAPE: u32 = 0x1B;
const VK_0: u32 = 0x30;
const VK_NUMPAD0: u32 = 0x60;

/// The digit a virtual key types, main row or numpad (`0..=9`).
fn digit_of(vk: u32) -> Option<usize> {
    match vk {
        0x30..=0x39 => Some((vk - VK_0) as usize),
        0x60..=0x69 => Some((vk - VK_NUMPAD0) as usize),
        _ => None,
    }
}

/// Whether `now` is at or past `expiry`, comparing on a wrapping 32-bit
/// millisecond clock (the tick wraps about every 49 days).
pub fn tick_expired(now: u32, expiry: u32) -> bool {
    (now.wrapping_sub(expiry) as i32) >= 0
}

/// The tick a claim armed at `now` lapses at: the card's auto-dismiss plus one
/// second, so the claim never outlives the card.
pub fn expiry_after(now: u32, auto_dismiss_secs: u32) -> u32 {
    now.wrapping_add(auto_dismiss_secs.saturating_add(1).saturating_mul(1000))
}

/// The decision the hook makes for one key-down: `Some` when the key is
/// claimed (the hook swallows it and posts the choice), `None` when it must
/// pass through untouched.
///
/// `injected` is true for input another program sent (an on-screen keyboard,
/// an automation tool): that is never claimed, so it cannot be used to dismiss
/// or pick, and a digit typed that way still reaches the app.
pub fn claimed_choice(
    claim: Option<Claim>,
    vk: u32,
    now_tick: u32,
    mods: Modifiers,
    injected: bool,
) -> Option<Choice> {
    let claim = claim?;
    if injected || mods.any() || tick_expired(now_tick, claim.expiry_tick) {
        return None;
    }
    if vk == VK_ESCAPE {
        return Some(Choice::Dismiss);
    }
    let n = digit_of(vk)?;
    (1..=claim.count.min(MAX_OPTIONS))
        .contains(&n)
        .then_some(Choice::Pick(n))
}

// Layout of the packed claim: bit 63 armed, bits 36..=59 the generation,
// bits 32..=35 the count, bits 0..=31 the expiry tick.
const ARMED_BIT: u64 = 1 << 63;

fn pack(c: Claim) -> u64 {
    ARMED_BIT
        | (u64::from(c.generation & GENERATION_MASK) << 36)
        | ((c.count.min(MAX_OPTIONS) as u64) << 32)
        | u64::from(c.expiry_tick)
}

fn unpack(v: u64) -> Option<Claim> {
    if v & ARMED_BIT == 0 {
        return None;
    }
    Some(Claim {
        generation: ((v >> 36) as u32) & GENERATION_MASK,
        count: ((v >> 32) & 0xF) as usize,
        expiry_tick: v as u32,
    })
}

/// The claim as one atomic word.
pub struct ClaimSlot(AtomicU64);

impl ClaimSlot {
    pub const fn new() -> Self {
        Self(AtomicU64::new(0))
    }

    pub fn arm(&self, claim: Claim) {
        self.0.store(pack(claim), Ordering::Relaxed);
    }

    pub fn clear(&self) {
        self.0.store(0, Ordering::Relaxed);
    }

    /// Arms only if nothing is armed: a compare-and-set, so a claim the main
    /// thread armed (or left cleared) in the meantime is never overwritten.
    /// `true` if it armed.
    pub fn arm_if_empty(&self, claim: Claim) -> bool {
        self.0
            .compare_exchange(0, pack(claim), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    pub fn current(&self) -> Option<Claim> {
        unpack(self.0.load(Ordering::Relaxed))
    }
}

impl Default for ClaimSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// The process-wide claim the keyboard hook reads.
static CLAIM: ClaimSlot = ClaimSlot::new();

/// The window that had the keyboard when the claim was armed (`HWND` as an
/// integer, `0` = none). The card never takes focus, so this is the window the
/// user is typing into: a digit typed after switching to another window is
/// theirs, not the card's. Written by the main thread before the claim is
/// armed, read by the hook.
static ARMED_FOREGROUND: AtomicIsize = AtomicIsize::new(0);

/// Records the window the claim belongs to. Call before [`arm`].
pub fn set_armed_foreground(hwnd: isize) {
    ARMED_FOREGROUND.store(hwnd, Ordering::Relaxed);
}

/// The window recorded by [`set_armed_foreground`].
pub fn armed_foreground() -> isize {
    ARMED_FOREGROUND.load(Ordering::Relaxed)
}

/// Whether the key goes to the window the claim was armed for. Fail-safe: when
/// either side is unknown (`0`: no foreground window, or the lookup failed) the
/// answer is no, so the key passes through to the app.
pub fn foreground_matches(armed: isize, now: isize) -> bool {
    armed != 0 && armed == now
}

/// The claim to put back after something cleared it while the card stayed up
/// (the tray menu closed without a choice, or a choice was ignored): the claim
/// as it was armed, unless it has lapsed by `now`.
pub fn rearm_plan(armed: Option<Claim>, now: u32) -> Option<Claim> {
    armed.filter(|c| !tick_expired(now, c.expiry_tick))
}

/// Whether the foreground keyboard layout is a Chinese, Japanese or Korean
/// one, from the layout handle's low word (the language id). With an input
/// method editor active the digit keys compose or select candidates, so the
/// claim defaults off for these layouts rather than eating a keystroke the
/// editor needs. The primary language id is the low 10 bits of the language id.
pub fn is_cjk_layout(hkl: usize) -> bool {
    const LANG_CHINESE: usize = 0x04;
    const LANG_JAPANESE: usize = 0x11;
    const LANG_KOREAN: usize = 0x12;
    matches!(hkl & 0x3FF, LANG_CHINESE | LANG_JAPANESE | LANG_KOREAN)
}

/// Whether a window can be the one the claim follows: there is one (`0` is
/// none) and it is not Wingman's own. With either, no key would ever match
/// [`foreground_matches`], so the card must not promise keys.
pub fn foreground_usable(hwnd: isize, is_own_window: bool) -> bool {
    hwnd != 0 && !is_own_window
}

/// Whether the card claims keys 1 to 4 and Esc: the `[options] claim_keys`
/// setting, unless the foreground keyboard layout is a CJK one (see
/// [`is_cjk_layout`]) or there is no usable foreground window (see
/// [`foreground_usable`]). Computed once when the card is shown, and the card's
/// hint follows it, so the hint never says "Press 1" for a claim that would
/// reject every key.
pub fn claim_enabled(setting: bool, foreground_hkl: usize, foreground_usable: bool) -> bool {
    setting && foreground_usable && !is_cjk_layout(foreground_hkl)
}

/// Starts claiming keys for the card showing `claim.generation`.
pub fn arm(claim: Claim) {
    CLAIM.arm(claim);
}

/// The hook's undo after a failed post: puts back the claim it just took, but
/// only while the card is still the one it took it from. If the main thread
/// cleared the claim meanwhile (the card hid, which also zeroes the armed
/// foreground) or armed a newer one, nothing is restored. `true` if restored.
pub fn restore_taken(claim: Claim) -> bool {
    armed_foreground() != 0 && CLAIM.arm_if_empty(claim)
}

/// Stops claiming. Called on every path that hides or replaces the card.
pub fn clear() {
    CLAIM.clear();
    ARMED_FOREGROUND.store(0, Ordering::Relaxed);
}

/// The claim in force, if any. Test seam and diagnostics; the hook uses
/// [`take_for_key`].
pub fn current() -> Option<Claim> {
    CLAIM.current()
}

/// The hook's one call per key-down. When the key is claimed it returns the
/// choice and the card generation, and the claim is spent on the spot: a
/// second digit before the main thread has handled the first reaches the app
/// instead of running a second option.
pub fn take_for_key(
    vk: u32,
    now_tick: u32,
    mods: Modifiers,
    injected: bool,
) -> Option<(Choice, u32)> {
    let claim = CLAIM.current()?;
    let choice = claimed_choice(Some(claim), vk, now_tick, mods, injected)?;
    CLAIM.clear();
    Some((choice, claim.generation))
}

#[cfg(test)]
pub(crate) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    const NONE: Modifiers = Modifiers {
        shift: false,
        ctrl: false,
        alt: false,
        win: false,
    };

    fn claim(count: usize) -> Option<Claim> {
        Some(Claim {
            generation: 7,
            count,
            expiry_tick: 21_000,
        })
    }

    fn decide(c: Option<Claim>, vk: u32, now: u32, mods: Modifiers) -> Option<Choice> {
        claimed_choice(c, vk, now, mods, false)
    }

    #[test]
    fn nothing_is_claimed_without_a_claim() {
        for vk in [VK_ESCAPE, 0x31, 0x62, 0x41, 0x0D] {
            assert_eq!(decide(None, vk, 0, NONE), None, "vk {vk:#x}");
        }
    }

    #[test]
    fn digits_up_to_the_option_count_are_claimed_main_row_and_numpad() {
        for n in 1..=3usize {
            assert_eq!(
                decide(claim(3), 0x30 + n as u32, 0, NONE),
                Some(Choice::Pick(n))
            );
            assert_eq!(
                decide(claim(3), 0x60 + n as u32, 0, NONE),
                Some(Choice::Pick(n))
            );
        }
    }

    #[test]
    fn a_digit_that_names_no_option_passes_through() {
        // Three options: 4 and 0 and 9 belong to the app underneath.
        for n in [0u32, 4, 9] {
            assert_eq!(decide(claim(3), 0x30 + n, 0, NONE), None, "main {n}");
            assert_eq!(decide(claim(3), 0x60 + n, 0, NONE), None, "numpad {n}");
        }
        // Four options claim 1 to 4 and still not 5.
        assert_eq!(decide(claim(4), 0x34, 0, NONE), Some(Choice::Pick(4)));
        assert_eq!(decide(claim(4), 0x35, 0, NONE), None);
    }

    #[test]
    fn a_count_above_the_cap_never_claims_more_than_four() {
        assert_eq!(decide(claim(9), 0x35, 0, NONE), None);
    }

    #[test]
    fn while_looking_only_esc_is_claimed() {
        assert_eq!(decide(claim(0), VK_ESCAPE, 0, NONE), Some(Choice::Dismiss));
        assert_eq!(decide(claim(0), 0x31, 0, NONE), None);
    }

    #[test]
    fn other_keys_are_never_claimed() {
        for vk in [0x0D, 0x20, 0x41, 0x09, 0x70, 0x6B] {
            assert_eq!(decide(claim(4), vk, 0, NONE), None, "vk {vk:#x}");
        }
    }

    #[test]
    fn any_modifier_down_passes_the_key_through() {
        let each = [
            Modifiers {
                shift: true,
                ..NONE
            },
            Modifiers { ctrl: true, ..NONE },
            Modifiers { alt: true, ..NONE },
            Modifiers { win: true, ..NONE },
        ];
        for mods in each {
            assert_eq!(decide(claim(4), 0x31, 0, mods), None, "{mods:?} + 1");
            assert_eq!(decide(claim(4), VK_ESCAPE, 0, mods), None, "{mods:?} + Esc");
        }
    }

    #[test]
    fn injected_input_is_never_claimed() {
        assert_eq!(claimed_choice(claim(3), 0x31, 0, NONE, true), None);
        assert_eq!(claimed_choice(claim(3), VK_ESCAPE, 0, NONE, true), None);
    }

    #[test]
    fn nothing_is_claimed_at_or_past_the_expiry_tick() {
        assert_eq!(decide(claim(3), 0x31, 20_999, NONE), Some(Choice::Pick(1)));
        assert_eq!(decide(claim(3), 0x31, 21_000, NONE), None);
        assert_eq!(decide(claim(3), VK_ESCAPE, 99_000, NONE), None);
    }

    #[test]
    fn the_expiry_compare_survives_the_tick_wrapping() {
        // Armed just before the 49-day wrap: the expiry lands after it.
        let expiry = expiry_after(u32::MAX - 500, 20);
        assert!(expiry < 30_000, "the expiry wrapped: {expiry}");
        assert!(!tick_expired(u32::MAX - 400, expiry));
        assert!(!tick_expired(10_000, expiry));
        assert!(tick_expired(expiry, expiry));
        assert!(tick_expired(expiry.wrapping_add(5), expiry));
    }

    #[test]
    fn the_expiry_is_the_auto_dismiss_plus_one_second() {
        assert_eq!(expiry_after(1_000, 20), 1_000 + 21_000);
        assert_eq!(expiry_after(0, 0), 1_000);
    }

    #[test]
    fn a_claim_round_trips_through_the_atomic_word() {
        let slot = ClaimSlot::new();
        assert_eq!(slot.current(), None);
        for c in [
            Claim {
                generation: 0,
                count: 0,
                expiry_tick: 0,
            },
            Claim {
                generation: GENERATION_MASK,
                count: 4,
                expiry_tick: u32::MAX,
            },
            Claim {
                generation: 123_456,
                count: 2,
                expiry_tick: 0xDEAD_BEEF,
            },
        ] {
            slot.arm(c);
            assert_eq!(slot.current(), Some(c));
        }
        slot.clear();
        assert_eq!(slot.current(), None);
    }

    #[test]
    fn an_armed_claim_is_never_the_zero_word() {
        // A claim with every field zero must still read as armed, or the
        // all-zero "cleared" value could not tell them apart.
        let slot = ClaimSlot::new();
        slot.arm(Claim {
            generation: 0,
            count: 0,
            expiry_tick: 0,
        });
        assert!(slot.current().is_some());
    }

    #[test]
    fn generations_wrap_inside_24_bits() {
        assert_eq!(next_generation(0), 1);
        assert_eq!(next_generation(GENERATION_MASK), 0);
    }

    #[test]
    fn choices_round_trip_through_the_message_word() {
        for c in [
            Choice::Dismiss,
            Choice::Pick(1),
            Choice::Pick(2),
            Choice::Pick(3),
            Choice::Pick(4),
        ] {
            assert_eq!(Choice::from_wparam(c.to_wparam()), Some(c));
        }
        assert_eq!(Choice::from_wparam(5), None);
        assert_eq!(Choice::from_wparam(usize::MAX), None);
    }

    #[test]
    fn a_key_only_goes_to_the_card_while_the_armed_window_has_the_keyboard() {
        assert!(foreground_matches(0x1234, 0x1234));
        assert!(!foreground_matches(0x1234, 0x5678), "another window");
        // Unknown on either side passes the key through.
        assert!(!foreground_matches(0, 0));
        assert!(!foreground_matches(0x1234, 0));
        assert!(!foreground_matches(0, 0x1234));
    }

    #[test]
    fn a_lapsed_claim_is_not_put_back() {
        let c = Claim {
            generation: 4,
            count: 2,
            expiry_tick: 21_000,
        };
        assert_eq!(rearm_plan(Some(c), 20_999), Some(c));
        assert_eq!(rearm_plan(Some(c), 21_000), None);
        assert_eq!(rearm_plan(None, 0), None);
    }

    #[test]
    fn chinese_japanese_and_korean_layouts_are_told_apart_by_language() {
        // Handle low word = language id: zh-CN, zh-TW, ja-JP, ko-KR.
        for hkl in [0x0804usize, 0x0404, 0x0411, 0x0412] {
            assert!(is_cjk_layout(hkl), "{hkl:#x}");
            // The high word is the device handle and must not matter.
            assert!(is_cjk_layout(0xE001_0000 | hkl), "{hkl:#x} with a device");
        }
        // en-US, de-DE, fr-FR, ru-RU, 0 (unknown).
        for hkl in [0x0409usize, 0x0407, 0x040C, 0x0419, 0] {
            assert!(!is_cjk_layout(hkl), "{hkl:#x}");
        }
    }

    #[test]
    fn the_claim_defaults_off_under_a_cjk_layout_and_never_overrides_the_setting() {
        assert!(claim_enabled(true, 0x0409, true));
        assert!(!claim_enabled(true, 0x0411, true), "Japanese IME");
        assert!(
            !claim_enabled(false, 0x0409, true),
            "the setting still wins"
        );
        assert!(!claim_enabled(false, 0x0411, true));
    }

    #[test]
    fn no_usable_foreground_means_no_claim_and_so_no_hint() {
        assert!(foreground_usable(0x1234, false));
        assert!(!foreground_usable(0, false), "no foreground window");
        assert!(!foreground_usable(0x1234, true), "Wingman's own window");
        assert!(!claim_enabled(true, 0x0409, false));
    }

    #[test]
    fn a_taken_claim_is_restored_only_into_an_empty_slot_of_a_live_card() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let taken = Claim {
            generation: 3,
            count: 2,
            expiry_tick: 50_000,
        };
        // Live card, slot empty (the hook just took it): restored.
        clear();
        set_armed_foreground(0xAAAA);
        assert!(restore_taken(taken));
        assert_eq!(current(), Some(taken));
        // A newer claim is in the slot: left alone.
        let newer = Claim {
            generation: 4,
            ..taken
        };
        arm(newer);
        assert!(!restore_taken(taken));
        assert_eq!(current(), Some(newer));
        // The main thread cleared the card meanwhile: nothing comes back.
        clear();
        assert!(!restore_taken(taken));
        assert_eq!(current(), None);
    }

    #[test]
    fn the_hook_spends_the_claim_on_the_first_key() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        arm(Claim {
            generation: 9,
            count: 3,
            expiry_tick: 50_000,
        });
        let taken = take_for_key(0x32, 1_000, NONE, false);
        assert_eq!(taken, Some((Choice::Pick(2), 9)));
        assert_eq!(current(), None, "spent: a second digit must pass through");
        assert_eq!(take_for_key(0x31, 1_000, NONE, false), None);
    }

    #[test]
    fn a_key_the_hook_does_not_claim_leaves_the_claim_in_force() {
        let _guard = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        arm(Claim {
            generation: 9,
            count: 3,
            expiry_tick: 50_000,
        });
        assert_eq!(take_for_key(0x41, 1_000, NONE, false), None);
        assert!(current().is_some());
        clear();
        assert_eq!(current(), None);
    }
}
