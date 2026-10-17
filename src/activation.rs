//! Who made Wingman ask, and how often an untrusted caller may (#236).
//!
//! `WM_APP_ACTIVATE` can be posted by any process of the same user, so it
//! cannot be authenticated (see `docs/superpowers/specs/2026-09-19-activation-trust-design.md`,
//! section 2). What can be done is to bound it, show it and attribute it:
//!
//! - [`Trigger`] says what started an ask, so the egress log can tell a key
//!   press from an activation message.
//! - [`ActivationLimiter`] caps activation asks only. Key presses, tray
//!   clicks and the palette are never limited.
//! - [`suppressed_count`] is what the diagnostics report shows.
//!
//! Nothing here polls or keeps a timer: the limiter is a pure function of
//! the time it is asked at.

use std::cell::Cell;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// What started an ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// The keyboard hook inside this process.
    Hotkey,
    /// `WM_APP_ACTIVATE`: a second launch of the exe, or any other process.
    Activation,
    /// "Ask now" in the tray menu.
    Tray,
    /// A row chosen in the Quick Ask palette.
    Palette,
    /// A row chosen on the options card (#964).
    Options,
}

impl Trigger {
    /// The word the egress log records.
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Hotkey => "hotkey",
            Trigger::Activation => "activation",
            Trigger::Tray => "tray",
            Trigger::Palette => "palette",
            Trigger::Options => "options",
        }
    }
}

/// Decision: fixed values, not settings. A person double-launching the app
/// does not need sub-second repeats; a loop posting the message does.
pub const MIN_INTERVAL: Duration = Duration::from_secs(3);
/// Decision: at most this many activation asks in any rolling hour.
pub const MAX_PER_HOUR: usize = 20;
const HOUR: Duration = Duration::from_secs(3600);

/// What the limiter says about one activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Go ahead and ask.
    Allowed,
    /// Too soon. `notify` is true for at most one refusal per
    /// [`MIN_INTERVAL`], so the user gets one short card instead of silence
    /// (AGENTS.md rule 7) without a flood being able to spam cards.
    Refused { notify: bool },
}

/// Pure rate limiter for activation asks; time is passed in.
#[derive(Debug, Default)]
pub struct ActivationLimiter {
    allowed: VecDeque<Instant>,
    last_notice: Option<Instant>,
}

impl ActivationLimiter {
    /// The verdict for an activation at `now`. An allowed one is recorded; a
    /// refusal changes nothing about the wait, so a flood cannot push the
    /// next allowed time further away.
    pub fn check(&mut self, now: Instant) -> Verdict {
        while let Some(&oldest) = self.allowed.front() {
            if now.saturating_duration_since(oldest) >= HOUR {
                self.allowed.pop_front();
            } else {
                break;
            }
        }
        let too_soon = self
            .allowed
            .back()
            .is_some_and(|&last| now.saturating_duration_since(last) < MIN_INTERVAL);
        if !too_soon && self.allowed.len() < MAX_PER_HOUR {
            self.allowed.push_back(now);
            return Verdict::Allowed;
        }
        let notify = self
            .last_notice
            .map_or(true, |n| now.saturating_duration_since(n) >= MIN_INTERVAL);
        if notify {
            self.last_notice = Some(now);
        }
        Verdict::Refused { notify }
    }
}

static LIMITER: Mutex<Option<ActivationLimiter>> = Mutex::new(None);
static SUPPRESSED: AtomicU64 = AtomicU64::new(0);

/// Called by the `WM_APP_ACTIVATE` handler, only once an ask would really
/// run (not busy, not paused), so a refused ask never costs the user an
/// allowance. A refusal is counted, never silent.
pub fn check_activation_now() -> Verdict {
    let verdict = match LIMITER.lock() {
        Ok(mut guard) => guard
            .get_or_insert_with(ActivationLimiter::default)
            .check(Instant::now()),
        // A poisoned lock cannot happen (nothing panics while holding it);
        // if it ever did, failing open keeps the Copilot key working.
        Err(_) => Verdict::Allowed,
    };
    if matches!(verdict, Verdict::Refused { .. }) {
        SUPPRESSED.fetch_add(1, Ordering::Relaxed);
    }
    verdict
}

/// How many activations have been refused since the process started.
pub fn suppressed_count() -> u64 {
    SUPPRESSED.load(Ordering::Relaxed)
}

thread_local! {
    static WORKER_TRIGGER: Cell<Option<Trigger>> = const { Cell::new(None) };
}

/// Marks the calling worker thread as running an ask started by `trigger`,
/// so every egress row it writes carries the label. A thread-local rather
/// than a global: other actions' threads stay unlabelled instead of
/// inheriting the last ask's trigger.
pub fn set_worker_trigger(trigger: Trigger) {
    WORKER_TRIGGER.with(|t| t.set(Some(trigger)));
}

/// Like [`set_worker_trigger`], for a run whose entry point is not known
/// (a confirmation step, say): `None` leaves the row unlabelled.
pub fn set_worker_trigger_opt(trigger: Option<Trigger>) {
    WORKER_TRIGGER.with(|t| t.set(trigger));
}

/// The label for the calling thread, if it was marked.
pub fn worker_trigger_label() -> Option<&'static str> {
    WORKER_TRIGGER.with(|t| t.get()).map(Trigger::label)
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFUSED: Verdict = Verdict::Refused { notify: false };

    #[test]
    fn the_first_activation_is_allowed() {
        let mut l = ActivationLimiter::default();
        assert_eq!(l.check(Instant::now()), Verdict::Allowed);
    }

    #[test]
    fn a_second_activation_inside_the_interval_is_refused() {
        let mut l = ActivationLimiter::default();
        let t0 = Instant::now();
        assert_eq!(l.check(t0), Verdict::Allowed);
        assert!(matches!(
            l.check(t0 + Duration::from_millis(500)),
            Verdict::Refused { .. }
        ));
        assert!(matches!(
            l.check(t0 + MIN_INTERVAL - Duration::from_millis(1)),
            Verdict::Refused { .. }
        ));
    }

    #[test]
    fn a_refused_activation_does_not_extend_the_wait() {
        let mut l = ActivationLimiter::default();
        let t0 = Instant::now();
        assert_eq!(l.check(t0), Verdict::Allowed);
        assert!(matches!(
            l.check(t0 + Duration::from_secs(2)),
            Verdict::Refused { .. }
        ));
        // 3 s after the allowed one, not 3 s after the refused one.
        assert_eq!(l.check(t0 + MIN_INTERVAL), Verdict::Allowed);
    }

    #[test]
    fn the_hourly_cap_holds_and_then_releases() {
        let mut l = ActivationLimiter::default();
        let t0 = Instant::now();
        for i in 0..MAX_PER_HOUR {
            assert_eq!(
                l.check(t0 + MIN_INTERVAL * i as u32),
                Verdict::Allowed,
                "ask {i}"
            );
        }
        let next = t0 + MIN_INTERVAL * MAX_PER_HOUR as u32;
        assert!(
            matches!(l.check(next), Verdict::Refused { .. }),
            "the cap must refuse ask {MAX_PER_HOUR}"
        );
        // An hour after the first one it frees up.
        assert_eq!(l.check(t0 + HOUR), Verdict::Allowed);
    }

    #[test]
    fn a_refusal_asks_for_a_card_at_most_once_per_interval() {
        let mut l = ActivationLimiter::default();
        let t0 = Instant::now();
        assert_eq!(l.check(t0), Verdict::Allowed);
        // First refusal notifies, the flood after it does not.
        assert_eq!(
            l.check(t0 + Duration::from_millis(100)),
            Verdict::Refused { notify: true }
        );
        assert_eq!(l.check(t0 + Duration::from_millis(200)), REFUSED);
        assert_eq!(l.check(t0 + Duration::from_millis(900)), REFUSED);
        // A new interval, and the cap is what refuses now: notify again.
        let mut l = ActivationLimiter::default();
        for i in 0..MAX_PER_HOUR {
            l.check(t0 + MIN_INTERVAL * i as u32);
        }
        let t1 = t0 + MIN_INTERVAL * MAX_PER_HOUR as u32;
        assert_eq!(l.check(t1), Verdict::Refused { notify: true });
        assert_eq!(l.check(t1 + Duration::from_secs(1)), REFUSED);
        assert_eq!(
            l.check(t1 + MIN_INTERVAL),
            Verdict::Refused { notify: true }
        );
    }

    #[test]
    fn trigger_labels_are_distinct_words() {
        let all = [
            Trigger::Hotkey,
            Trigger::Activation,
            Trigger::Tray,
            Trigger::Palette,
        ];
        let mut labels: Vec<&str> = all.iter().map(|t| t.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), all.len());
        assert_eq!(Trigger::Activation.label(), "activation");
    }

    #[test]
    fn the_options_card_has_its_own_trigger_label() {
        assert_eq!(Trigger::Options.label(), "options");
        std::thread::spawn(|| {
            set_worker_trigger_opt(None);
            assert_eq!(worker_trigger_label(), None);
            set_worker_trigger_opt(Some(Trigger::Options));
            assert_eq!(worker_trigger_label(), Some("options"));
        })
        .join()
        .unwrap();
    }

    #[test]
    fn the_worker_trigger_is_per_thread() {
        assert_eq!(worker_trigger_label(), None);
        set_worker_trigger(Trigger::Hotkey);
        assert_eq!(worker_trigger_label(), Some("hotkey"));
        let other = std::thread::spawn(worker_trigger_label).join().unwrap();
        assert_eq!(other, None, "another thread must not inherit the label");
    }

    #[test]
    fn refusals_are_counted() {
        // The global limiter is shared by the process, so only assert the
        // direction: hammering it must refuse something and count it.
        let before = suppressed_count();
        let allowed = (0..5)
            .filter(|_| check_activation_now() == Verdict::Allowed)
            .count();
        assert!(allowed <= 1, "five instant activations, at most one asks");
        assert!(suppressed_count() >= before + 4);
    }
}
