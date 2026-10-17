//! Restarts the knowledge watch and scan without holding the UI thread (#1201).
//!
//! A Reload or Settings save that changes the folder used to drop the old
//! watcher and scan (up to 150 ms each) and then wait for the new watcher's
//! first read to arm (up to 500 ms) on the UI thread, which also hosts the
//! keyboard hook. This moves all of that to one short-lived worker thread. The
//! worker drops the old handles first and starts the new ones only after that,
//! so two watchers never index the same root at once. A restart asked for while
//! one is under way is remembered and run when the first ends (only the latest
//! folder, so a burst of saves ends in one more start).

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use super::ingest::ScanHandle;
use super::watch::Watcher;

/// What the runner keeps running, and whether its watch has ended on its own.
pub trait Handles: Send + 'static {
    fn watch_ended(&self) -> bool;

    /// Stops everything and waits up to `within` for it to be over. For the
    /// worker thread. False when something was still running at the end.
    fn finish(self, _within: Duration) -> bool
    where
        Self: Sized,
    {
        drop(self);
        true
    }
}

/// The longest the worker waits for the old watch and scan to end before it
/// starts the new ones anyway (a slow or sleeping drive).
const FINISH_WAIT: Duration = Duration::from_secs(5);

/// The real handles: the folder watch and the start-up scan, either of which
/// may be missing (no folder, or the thread could not start).
pub struct Live {
    pub watch: Option<Watcher>,
    #[allow(dead_code)] // held so that dropping it stops the scan
    pub scan: Option<ScanHandle>,
}

impl Handles for Live {
    fn watch_ended(&self) -> bool {
        self.watch.as_ref().is_some_and(|w| !w.is_running())
    }

    fn finish(self, within: Duration) -> bool {
        if let Some(scan) = &self.scan {
            scan.request_stop();
        }
        let end = Instant::now() + within;
        let mut done = true;
        if let Some(watch) = &self.watch {
            done &= watch.stop_and_wait(within);
        }
        if let Some(scan) = &self.scan {
            while !scan.is_finished() && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(5));
            }
            done &= scan.is_finished();
        }
        done
    }
}

type Starter<H> = Arc<dyn Fn(&str) -> Option<H> + Send + Sync>;

struct State<H> {
    current: Option<H>,
    /// The folder text the handles were last started (or queued) for.
    folder: String,
    busy: bool,
    /// The folder to start once the restart under way is done.
    queued: Option<String>,
    closed: bool,
}

pub struct Runner<H: Handles> {
    state: Arc<Mutex<State<H>>>,
    start: Starter<H>,
}

fn lock<H>(state: &Mutex<State<H>>) -> MutexGuard<'_, State<H>> {
    state.lock().unwrap_or_else(|e| e.into_inner())
}

impl<H: Handles> Runner<H> {
    /// `current` was started synchronously for `folder` (start-up). `start`
    /// runs on the worker for every later restart.
    pub fn new(
        folder: String,
        current: Option<H>,
        start: impl Fn(&str) -> Option<H> + Send + Sync + 'static,
    ) -> Self {
        Runner {
            state: Arc::new(Mutex::new(State {
                current,
                folder,
                busy: false,
                queued: None,
                closed: false,
            })),
            start: Arc::new(start),
        }
    }

    /// Starts again for `wanted` when `needs(started_for, wanted, watch_ended)`
    /// says so. Never waits for the old handles or the new start: the caller
    /// returns at once. A restart already under way queues this one.
    pub fn restart_if_needed(&self, wanted: &str, needs: fn(&str, &str, bool) -> bool) {
        let old = {
            let mut s = lock(&self.state);
            let ended = s.current.as_ref().is_some_and(Handles::watch_ended);
            if !needs(&s.folder, wanted, ended) {
                return;
            }
            s.folder = wanted.to_string();
            if s.busy {
                s.queued = Some(wanted.to_string());
                return;
            }
            s.busy = true;
            s.current.take()
        };
        let state = Arc::clone(&self.state);
        let start = Arc::clone(&self.start);
        let target = wanted.to_string();
        let spawned = std::thread::Builder::new()
            .name("knowledge-restart".into())
            .spawn(move || work(&state, &start, old, target));
        if let Err(e) = spawned {
            // The closure (and the old handles inside it) is gone with the
            // error. Nothing runs now; a later Reload asks again.
            let mut s = lock(&self.state);
            s.busy = false;
            s.folder.clear();
            super::notice::report(&format!(
                "Wingman could not restart watching your knowledge folder ({}). Choose Reload settings to try again.",
                e.to_string().trim()
            ));
        }
    }

    #[cfg(test)]
    fn is_busy(&self) -> bool {
        lock(&self.state).busy
    }
}

impl<H: Handles> Drop for Runner<H> {
    fn drop(&mut self) {
        let old = {
            let mut s = lock(&self.state);
            s.closed = true;
            s.current.take()
        };
        drop(old);
    }
}

/// The worker: drop the old handles (it may wait on them), start the new ones,
/// install them, and go again if another folder was asked for meanwhile.
fn work<H: Handles>(state: &Mutex<State<H>>, start: &Starter<H>, old: Option<H>, target: String) {
    let (mut old, mut target) = (old, target);
    loop {
        if let Some(old) = old.take() {
            if !old.finish(FINISH_WAIT) {
                super::notice::report(
                    "The old knowledge folder was still being read when Wingman switched folders. Some files may be indexed twice until the next start.",
                );
            }
        }
        // A panic in the start must not leave the runner busy for good.
        let started = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| start(&target)))
            .unwrap_or(None);
        let mut s = lock(state);
        if s.closed {
            drop(s);
            // Dropped before `busy` clears, so "idle" means nothing is alive.
            drop(started);
            lock(state).busy = false;
            return;
        }
        if started.is_none() && s.queued.is_none() {
            // Nothing runs: forget the folder so the same Reload retries.
            s.folder.clear();
            super::notice::report(
                "Wingman could not restart watching your knowledge folder. Choose Reload settings to try again.",
            );
        }
        s.current = started;
        match s.queued.take() {
            Some(next) => {
                old = s.current.take();
                target = next;
            }
            None => {
                s.busy = false;
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// A stand-in for the watcher and scan that logs when it starts and ends.
    struct Fake {
        name: String,
        live: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
        drop_delay: Duration,
        ended: bool,
    }

    impl Handles for Fake {
        fn watch_ended(&self) -> bool {
            self.ended
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            std::thread::sleep(self.drop_delay);
            self.live.fetch_sub(1, Ordering::SeqCst);
            self.log.lock().unwrap().push(format!("end {}", self.name));
        }
    }

    struct Rig {
        live: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        log: Arc<Mutex<Vec<String>>>,
    }

    fn rig(start_delay: Duration, drop_delay: Duration) -> (Rig, Runner<Fake>) {
        let live = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let log = Arc::new(Mutex::new(Vec::new()));
        let make = {
            let (live, peak, log) = (Arc::clone(&live), Arc::clone(&peak), Arc::clone(&log));
            move |name: &str| {
                let now = live.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                log.lock().unwrap().push(format!("start {name}"));
                Fake {
                    name: name.to_string(),
                    live: Arc::clone(&live),
                    log: Arc::clone(&log),
                    drop_delay,
                    ended: false,
                }
            }
        };
        let first = make("A");
        let runner = Runner::new("A".to_string(), Some(first), move |f: &str| {
            std::thread::sleep(start_delay);
            Some(make(f))
        });
        (Rig { live, peak, log }, runner)
    }

    fn changed(started_for: &str, wanted: &str, ended: bool) -> bool {
        started_for != wanted || ended
    }

    fn wait_idle(r: &Runner<Fake>) {
        let end = Instant::now() + Duration::from_secs(10);
        while r.is_busy() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!r.is_busy(), "the restart never finished");
    }

    #[test]
    fn a_restart_returns_at_once_while_the_old_end_and_the_new_start_are_slow() {
        let (rig, runner) = rig(Duration::from_millis(600), Duration::from_millis(200));
        let t = Instant::now();
        runner.restart_if_needed("B", changed);
        let took = t.elapsed();
        assert!(
            took < Duration::from_millis(100),
            "the caller waited {took:?}"
        );
        wait_idle(&runner);
        assert_eq!(
            *rig.log.lock().unwrap(),
            ["start A", "end A", "start B"],
            "the old one ends before the new one starts"
        );
    }

    #[test]
    fn two_watchers_are_never_alive_at_once_across_a_burst_of_restarts() {
        let (rig, runner) = rig(Duration::from_millis(50), Duration::from_millis(50));
        for f in ["B", "C", "D", "E"] {
            runner.restart_if_needed(f, changed);
        }
        wait_idle(&runner);
        assert_eq!(
            rig.peak.load(Ordering::SeqCst),
            1,
            "{:?}",
            rig.log.lock().unwrap()
        );
        assert_eq!(rig.live.load(Ordering::SeqCst), 1);
        let log = rig.log.lock().unwrap().clone();
        assert_eq!(log.last().map(String::as_str), Some("start E"), "{log:?}");
        assert!(
            !log.contains(&"start C".to_string()) && !log.contains(&"start D".to_string()),
            "only the latest queued folder is started: {log:?}"
        );
    }

    #[test]
    fn an_unchanged_folder_with_a_live_watch_does_nothing() {
        let (rig, runner) = rig(Duration::ZERO, Duration::ZERO);
        runner.restart_if_needed("A", changed);
        assert!(!runner.is_busy());
        assert_eq!(*rig.log.lock().unwrap(), ["start A"]);
    }

    #[test]
    fn an_ended_watch_is_restarted_for_the_same_folder() {
        let (rig, runner) = rig(Duration::ZERO, Duration::ZERO);
        lock(&runner.state).current.as_mut().unwrap().ended = true;
        runner.restart_if_needed("A", changed);
        wait_idle(&runner);
        assert_eq!(*rig.log.lock().unwrap(), ["start A", "end A", "start A"]);
    }

    #[test]
    fn dropping_the_runner_ends_the_handles_even_when_a_start_finishes_late() {
        let (rig, runner) = rig(Duration::from_millis(300), Duration::ZERO);
        runner.restart_if_needed("B", changed);
        let state = Arc::clone(&runner.state);
        drop(runner);
        let end = Instant::now() + Duration::from_secs(10);
        while lock(&state).busy && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            rig.live.load(Ordering::SeqCst),
            0,
            "{:?}",
            rig.log.lock().unwrap()
        );
    }

    #[test]
    fn the_old_handles_are_finished_before_the_new_start_even_when_they_end_slowly() {
        let (rig, runner) = rig(Duration::ZERO, Duration::from_millis(300));
        runner.restart_if_needed("B", changed);
        wait_idle(&runner);
        let log = rig.log.lock().unwrap().clone();
        assert_eq!(log, ["start A", "end A", "start B"], "{log:?}");
    }

    #[test]
    fn a_failed_start_forgets_the_folder_so_the_same_reload_retries() {
        let tries = Arc::new(AtomicUsize::new(0));
        let for_start = Arc::clone(&tries);
        let runner: Runner<Fake> = Runner::new(String::new(), None, move |_| {
            for_start.fetch_add(1, Ordering::SeqCst);
            None
        });
        runner.restart_if_needed("B", changed);
        wait_idle(&runner);
        runner.restart_if_needed("B", changed);
        wait_idle(&runner);
        assert_eq!(
            tries.load(Ordering::SeqCst),
            2,
            "the second Reload must retry"
        );
    }

    #[test]
    fn a_panicking_start_does_not_leave_the_runner_busy() {
        let runner: Runner<Fake> = Runner::new(String::new(), None, |_| panic!("boom"));
        runner.restart_if_needed("B", changed);
        wait_idle(&runner);
        assert!(lock(&runner.state).current.is_none());
    }
}
