//! The local store: one SQLite database for everything Wingman keeps.
//!
//! Design: `docs/superpowers/specs/2026-10-15-local-store-design.md`. Slice 1
//! holds the plumbing (open, pragmas, owner-only directory, migrations,
//! corruption recovery) and the first table, `usage_day`, which replaces
//! `spend.json`.
//!
//! * **Where:** `%LOCALAPPDATA%\Wingman\store\wingman.db`. The directory is
//!   restricted to the current user with an inheritable ACL ([`acl`]), so the
//!   `-wal` and `-shm` files SQLite creates later are covered too.
//! * **Threading:** one write connection behind a `Mutex`; every write is one
//!   short transaction and its error goes straight back to the caller.
//! * **Idle cost (AGENTS.md rule 5):** no thread, no timer. The database
//!   opens on first use and nothing runs until a caller asks.
//! * **Failures (rule 7):** a failure never fails the user's request. It is
//!   reported once per run through [`report`], and `app.rs` adds the text to
//!   the next card ([`take_notice`]).
//! * **Only this module names `rusqlite`.** Callers get typed methods.

mod acl;
pub mod chunks;
mod egress;
mod import;
pub mod knowledge;
mod migrate;
mod profile_field;
mod usage_day;

pub use egress::append_pending;
#[cfg(test)]
pub(crate) use egress::append_pending_at;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rusqlite::{Connection, ErrorCode};

#[derive(Debug, Clone)]
pub enum StoreError {
    /// The file was made by a newer Wingman; it is left untouched.
    Newer {
        found: i64,
        known: i64,
    },
    /// The file is damaged.
    Corrupt(String),
    /// The directory could not be restricted to the current user.
    Acl(String),
    /// A migration step failed and was rolled back.
    Migration(i64, String),
    /// The saved profile could not be read or written.
    Profile(String),
    /// A saved profile value is sealed for another Windows user or PC.
    Undecryptable(String),
    /// A profile value looked like payment data and was not saved.
    Refused(String),
    Other(String),
}

impl std::fmt::Display for StoreError {
    /// Plain words for a card.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Newer { found, known } => write!(
                f,
                "Wingman's saved data was made by a newer version of Wingman (data version {found}, this one knows {known}), so it was left alone."
            ),
            StoreError::Corrupt(why) => write!(f, "Wingman's saved data is damaged ({why})."),
            StoreError::Acl(why) => write!(
                f,
                "Wingman could not make its data folder private to you ({why}), so it is not saving history."
            ),
            StoreError::Migration(v, why) => write!(
                f,
                "Wingman could not update its saved data (step {v}: {why})."
            ),
            StoreError::Profile(why) => {
                write!(f, "Wingman could not read or save your profile ({why}).")
            }
            StoreError::Undecryptable(why) => write!(
                f,
                "Wingman could not unlock your saved profile ({why}). It was probably saved by another Windows user or on another PC, and only that user on that PC can open it. Your saved profile was left as it is."
            ),
            StoreError::Refused(why) => write!(
                f,
                "Wingman did not save your profile: {why}. Remove that text and try again."
            ),
            StoreError::Other(why) => write!(f, "Wingman could not save its history ({why})."),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        if let rusqlite::Error::SqliteFailure(f, _) = &e {
            if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) {
                CORRUPT_EVENTS.fetch_add(1, Ordering::SeqCst);
                if is_fts_corrupt(&e) {
                    FTS_CORRUPT_EVENTS.fetch_add(1, Ordering::SeqCst);
                }
                return StoreError::Corrupt(e.to_string());
            }
        }
        StoreError::Other(e.to_string())
    }
}

/// Whether `e` says the file is damaged, without counting it as an event.
fn is_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if matches!(f.code, ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase)
    )
}

/// Whether `e` is SQLite's "virtual table is damaged" (`SQLITE_CORRUPT_VTAB`,
/// extended code 267, primary code 11): what a full-text index that disagrees
/// with its table raises. MEASURED 2026-10-17: real damage to the index's own
/// rows raises 267 (see `real_index_damage_raises_the_virtual_table_code`).
fn is_fts_corrupt(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.extended_code == rusqlite::ffi::SQLITE_CORRUPT_VTAB
    )
}

/// Counts corrupt-class errors seen anywhere in the process (#660). A store
/// whose own count is behind checks itself the next time it is locked.
static CORRUPT_EVENTS: AtomicU64 = AtomicU64::new(0);

/// The part of [`CORRUPT_EVENTS`] that came from a virtual table (the
/// full-text index, #1159). The index check reads the whole corpus, so it runs
/// only when this moved.
static FTS_CORRUPT_EVENTS: AtomicU64 = AtomicU64::new(0);

pub struct Store {
    conn: Mutex<Connection>,
    /// Where the file lives; `None` for an in-memory store, which has nothing
    /// to move aside.
    path: Option<PathBuf>,
    /// The value of [`CORRUPT_EVENTS`] this store last checked itself against.
    seen: AtomicU64,
    /// The value of [`FTS_CORRUPT_EVENTS`] the index was last checked against.
    fts_seen: AtomicU64,
    /// Only touched with `conn` held, so it never contends.
    heal_state: Mutex<HealState>,
    /// How long a failed heal waits before the next try.
    retry_gap: Duration,
    /// Called after the file was replaced (#1158).
    replaced_hooks: Mutex<Vec<(u64, ReplacedHook)>>,
    next_hook: AtomicU64,
}

/// What a subscriber runs after the store's file was replaced. It runs with
/// the connection locked, so it must only signal (set an event, flip a flag)
/// and never touch the store.
pub type ReplacedHook = Arc<dyn Fn() + Send + Sync>;

/// Keeps a [`Store::on_replaced`] hook alive; dropping it removes the hook.
pub struct ReplacedSubscription {
    store: Arc<Store>,
    id: u64,
}

impl Drop for ReplacedSubscription {
    fn drop(&mut self) {
        self.store
            .replaced_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| *id != self.id);
    }
}

/// What a store that could not finish healing remembers between locks.
#[derive(Default)]
struct HealState {
    /// A failed heal does not run again before this.
    not_before: Option<Instant>,
    /// Whether the user was already told the store is stuck.
    told: bool,
    /// Whether the user was already told the search index is stuck. Separate
    /// from `told`, so the milder index notice never hides the file notice.
    index_told: bool,
    /// Set once the damaged file was moved here but no fresh database is open
    /// yet: the next try only has to open one.
    kept: Option<PathBuf>,
    /// Set once the connection was swapped for the empty in-memory stand-in
    /// and no file connection has been installed since (#1165). A check on
    /// that stand-in always passes, so while this is set the next try goes
    /// straight to opening the file instead of checking.
    detached: bool,
}

/// How a heal ended.
enum Heal {
    /// Nothing to do: not this store's file, or a check that could not run.
    Fine,
    /// The file was never moved and is open again as it was.
    Reopened,
    /// Only the full-text index was damaged; it was rebuilt in place.
    Rebuilt,
    /// A fresh database replaced the damaged (or missing) one; the notice for
    /// the user.
    Replaced(String),
    /// Could not finish. The text says why. Tried again after [`RETRY_GAP`].
    Stuck(String),
    /// Only the index rebuild could not finish; the file itself is fine and
    /// history is still saved. Tried again after [`RETRY_GAP`].
    IndexStuck(String),
}

/// Wait between tries after a heal failed, so a file that stays held does not
/// get a `quick_check` on every lock.
const RETRY_GAP: Duration = Duration::from_secs(30);

impl Store {
    /// Opens (creating if needed) the database at `path`: pragmas, then
    /// migrations. No ACL, no recovery, no import: see [`open_default`].
    fn open_at(path: &Path) -> Result<Store, StoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| StoreError::Other(e.to_string()))?;
        }
        let mut conn = Connection::open(path)?;
        configure(&conn)?;
        migrate::run(&mut conn, migrate::MIGRATIONS, Some(path))?;
        Ok(Store::over(conn, Some(path.to_path_buf())))
    }

    fn over(conn: Connection, path: Option<PathBuf>) -> Store {
        Store {
            conn: Mutex::new(conn),
            path,
            seen: AtomicU64::new(CORRUPT_EVENTS.load(Ordering::SeqCst)),
            fts_seen: AtomicU64::new(FTS_CORRUPT_EVENTS.load(Ordering::SeqCst)),
            heal_state: Mutex::new(HealState::default()),
            retry_gap: RETRY_GAP,
            replaced_hooks: Mutex::new(Vec::new()),
            next_hook: AtomicU64::new(0),
        }
    }

    /// Runs `hook` each time a damaged file is replaced by a fresh one while
    /// Wingman runs (#1158): row ids kept from before the swap are then
    /// stale. Event-driven, no polling. See [`ReplacedHook`] for the rule.
    pub fn on_replaced(self: &Arc<Self>, hook: ReplacedHook) -> ReplacedSubscription {
        let id = self.next_hook.fetch_add(1, Ordering::SeqCst);
        self.replaced_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, hook));
        ReplacedSubscription {
            store: Arc::clone(self),
            id,
        }
    }

    fn fire_replaced(&self) {
        let hooks: Vec<ReplacedHook> = self
            .replaced_hooks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(_, h)| Arc::clone(h))
            .collect();
        for hook in hooks {
            hook();
        }
    }

    /// A store over a file in a test directory, for tests outside this module.
    #[cfg(test)]
    pub(crate) fn open_for_test(path: &Path) -> Result<Store, StoreError> {
        Store::open_at(path)
    }

    /// Overwrites this store's file with garbage while it stays open, so the
    /// next query reads it (#660). The checkpoint moves the data into the main
    /// file and `shrink_memory` drops the page cache first.
    #[cfg(test)]
    pub(crate) fn damage_file_for_test(&self) {
        let path = self.path.clone().expect("a file-backed store");
        self.lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA shrink_memory;")
            .unwrap();
        std::fs::write(path, vec![b'x'; 8192]).unwrap();
    }

    /// [`Store::open_at`], but a damaged file is moved aside to
    /// `<name>.corrupt-<unix time>` (with its `-wal` and `-shm`) and a fresh
    /// database is created. The second value is the notice for the user.
    fn open_recovering(path: &Path) -> Result<(Store, Option<String>), StoreError> {
        match Store::open_at(path) {
            Ok(s) => Ok((s, None)),
            Err(StoreError::Corrupt(_)) => {
                let kept = quarantine(path)?;
                let s = Store::open_at(path)?;
                let notice = format!(
                    "Wingman's saved data was damaged. A copy was kept at {}. Starting fresh.",
                    kept.display()
                );
                Ok((s, Some(notice)))
            }
            Err(e) => Err(e),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Connection> {
        // A panic elsewhere must not take the history with it.
        let mut conn = self.conn.lock().unwrap_or_else(|e| e.into_inner());
        // One atomic load when nothing happened. The later store is safe: the
        // connection mutex is held.
        if self.seen.load(Ordering::SeqCst) != CORRUPT_EVENTS.load(Ordering::SeqCst) {
            self.check(&mut conn);
        }
        conn
    }

    /// Runs after a corrupt-class error was seen (#660), with `conn` held.
    fn check(&self, conn: &mut Connection) {
        let mut state = self.heal_state.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if state.not_before.is_some_and(|t| now < t) {
            return;
        }
        match self.heal(conn, &mut state) {
            Heal::Fine => {
                self.seen
                    .store(CORRUPT_EVENTS.load(Ordering::SeqCst), Ordering::SeqCst);
            }
            Heal::Reopened => {
                *state = HealState::default();
                self.seen
                    .store(CORRUPT_EVENTS.load(Ordering::SeqCst), Ordering::SeqCst);
                note_recovery(reopened_notice());
            }
            Heal::Rebuilt => {
                self.seen
                    .store(CORRUPT_EVENTS.load(Ordering::SeqCst), Ordering::SeqCst);
                note_recovery(rebuilt_notice());
            }
            Heal::Replaced(notice) => {
                *state = HealState::default();
                self.seen
                    .store(CORRUPT_EVENTS.load(Ordering::SeqCst), Ordering::SeqCst);
                note_recovery(notice);
                self.fire_replaced();
            }
            Heal::Stuck(why) => {
                // `seen` stays behind, so a later lock tries again.
                state.not_before = Some(now + self.retry_gap);
                if let Some(text) = stuck_notice_once(&mut state, false, &why) {
                    note_recovery(text);
                }
            }
            Heal::IndexStuck(why) => {
                state.not_before = Some(now + self.retry_gap);
                if let Some(text) = stuck_notice_once(&mut state, true, &why) {
                    note_recovery(text);
                }
            }
        }
    }

    /// If this store's file really is damaged, closes it, moves it aside and
    /// opens a fresh one. Never deletes the damaged file. When it cannot move
    /// the file, the original stays open (calls keep failing with ordinary
    /// errors the callers already handle) and a later lock tries again.
    fn heal(&self, conn: &mut Connection, state: &mut HealState) -> Heal {
        let Some(path) = self.path.as_ref() else {
            return Heal::Fine;
        };
        if state.kept.is_none() {
            if state.detached {
                // `conn` is the empty stand-in, so a check would pass. Open
                // the file as it is; only a damaged one is moved aside.
                let existed = path.exists();
                match Store::open_at(path) {
                    Ok(fresh) => {
                        *conn = fresh.conn.into_inner().unwrap_or_else(|e| e.into_inner());
                        // A missing file was created empty: not the old data.
                        return if existed {
                            Heal::Reopened
                        } else {
                            Heal::Replaced(missing_file_notice())
                        };
                    }
                    Err(StoreError::Corrupt(_)) => {}
                    Err(e) => return Heal::Stuck(e.to_string()),
                }
            } else {
                if !file_is_damaged(conn) {
                    // The file passes, but the full-text index may disagree
                    // with its table (#1159): rebuild just that, so the user
                    // keeps history and documents. A rebuild that hits damage
                    // too means the file is damaged after all.
                    //
                    // That check reads the whole corpus, so it runs only when
                    // a virtual-table error was seen since the last one.
                    let fts_now = FTS_CORRUPT_EVENTS.load(Ordering::SeqCst);
                    if self.fts_seen.load(Ordering::SeqCst) == fts_now
                        || !fts_index_out_of_step(conn)
                    {
                        self.fts_seen.store(fts_now, Ordering::SeqCst);
                        return Heal::Fine;
                    }
                    match rebuild_verdict(rebuild_fts_index(conn)) {
                        RebuildVerdict::Rebuilt => {
                            self.fts_seen.store(fts_now, Ordering::SeqCst);
                            return Heal::Rebuilt;
                        }
                        RebuildVerdict::Damaged => {}
                        // Busy or disk full: the index is still wrong, so
                        // try again later instead of calling it healthy.
                        RebuildVerdict::Retry(why) => return Heal::IndexStuck(why),
                    }
                }
                // Closing the old connection first: Windows will not rename an open file.
                let Ok(empty) = Connection::open_in_memory() else {
                    return Heal::Fine;
                };
                drop(std::mem::replace(conn, empty));
                state.detached = true;
            }
            match quarantine(path) {
                Ok(kept) => state.kept = Some(kept),
                Err(e) => {
                    state.detached = !reopen_original(conn, path);
                    return Heal::Stuck(e.to_string());
                }
            }
        }
        // A `-wal` left behind would replay the old log onto the new file.
        if side_files_present(path) {
            return Heal::Stuck("old log files are still in the way".into());
        }
        match Store::open_at(path) {
            Ok(fresh) => {
                *conn = fresh.conn.into_inner().unwrap_or_else(|e| e.into_inner());
                let kept = state.kept.take().unwrap_or_else(|| path.clone());
                Heal::Replaced(runtime_recovery_notice(Ok(&kept)))
            }
            Err(e) => Heal::Stuck(e.to_string()),
        }
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Store {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate::run(&mut conn, migrate::MIGRATIONS, None).unwrap();
        Store::over(conn, None)
    }
}

/// True only when SQLite itself says the file is damaged. Any other outcome,
/// including a check that could not run (busy, I/O), counts as healthy: a file
/// is never moved aside on a guess.
fn file_is_damaged(conn: &Connection) -> bool {
    match conn.query_row("PRAGMA quick_check(1)", [], |r| r.get::<_, String>(0)) {
        Ok(verdict) => verdict != "ok",
        Err(e) => is_corrupt(&e),
    }
}

/// Puts the original file back under `conn` after it could not be moved. Its
/// errors keep surfacing; nothing is created if the file is gone. `true` when
/// a connection to the file is installed.
fn reopen_original(conn: &mut Connection, path: &Path) -> bool {
    if !path.exists() {
        return false;
    }
    match Connection::open(path) {
        Ok(c) => {
            let _ = configure(&c);
            *conn = c;
            true
        }
        Err(_) => false,
    }
}

/// The card text after a store lost its file connection for a while and got it
/// back without moving anything.
fn reopened_notice() -> String {
    "Wingman's saved data could not be opened for a while and is open again. Usage and request history from that time may be missing.".into()
}

/// True when the full-text index says it no longer matches the chunk table.
/// `quick_check` does not look at that. Only SQLite's own corrupt verdict
/// counts: a check that could not run (busy, missing table) is "no".
fn fts_index_out_of_step(conn: &Connection) -> bool {
    match conn.execute(
        // Rank 1 also compares the index with the chunk table (external content).
        "INSERT INTO chunk_fts (chunk_fts, rank) VALUES ('integrity-check', 1)",
        [],
    ) {
        Ok(_) => false,
        Err(e) => is_corrupt(&e),
    }
}

/// Rebuilds the full-text index from the chunk table.
fn rebuild_fts_index(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute("INSERT INTO chunk_fts (chunk_fts) VALUES ('rebuild')", [])
        .map(|_| ())
}

/// How an index rebuild ended.
enum RebuildVerdict {
    Rebuilt,
    /// The rebuild itself hit damage: the file is damaged after all.
    Damaged,
    /// Failed for another reason (busy, disk full); worth trying again.
    Retry(String),
}

fn rebuild_verdict(r: rusqlite::Result<()>) -> RebuildVerdict {
    match r {
        Ok(()) => RebuildVerdict::Rebuilt,
        Err(e) if is_corrupt(&e) => RebuildVerdict::Damaged,
        Err(e) => RebuildVerdict::Retry(e.to_string()),
    }
}

/// The card text after only the search index was rebuilt.
fn rebuilt_notice() -> String {
    "Wingman's search index over your documents was damaged while it was running and has been rebuilt. Nothing was lost.".into()
}

/// The card text after a store lost its file connection and the file was gone
/// when it came back, so a new one was started.
fn missing_file_notice() -> String {
    "Wingman's saved data file was missing, so a new one was started. Usage and request history are lost, and your saved profile and indexed documents start empty.".into()
}

/// True if a `-wal` or `-shm` sits next to `path`.
fn side_files_present(path: &Path) -> bool {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    ["-wal", "-shm"]
        .iter()
        .any(|suffix| path.with_file_name(format!("{name}{suffix}")).exists())
}

/// The card text after a store was replaced mid-run. `Ok` holds where the
/// damaged copy was kept; `Err` holds why it could not be replaced.
fn runtime_recovery_notice(kept: Result<&Path, &str>) -> String {
    match kept {
        Ok(path) => format!(
            "Wingman's saved data was damaged while it was running. A copy was kept at {}. Starting fresh: usage and request history are lost, and your saved profile and indexed documents start empty.",
            path.display()
        ),
        Err(why) => format!(
            "Wingman's saved data was damaged while it was running and could not be replaced ({why}), so history is not being saved for now. Wingman will try again. The damaged file was not deleted."
        ),
    }
}

/// The card text when only the search index could not be rebuilt (#1182):
/// history and documents are still saved.
fn index_stuck_notice(why: &str) -> String {
    format!(
        "Wingman's search index could not be rebuilt ({why}), so searching saved data may miss results for now. Your history is still being saved. Wingman will try again."
    )
}

/// The notice for a stuck heal, once per kind: the index one and the file one
/// each have their own flag, so the milder index notice never hides the file
/// one (#1182).
fn stuck_notice_once(state: &mut HealState, index_only: bool, why: &str) -> Option<String> {
    let told = if index_only {
        &mut state.index_told
    } else {
        &mut state.told
    };
    if *told {
        return None;
    }
    *told = true;
    Some(if index_only {
        index_stuck_notice(why)
    } else {
        runtime_recovery_notice(Err(why))
    })
}

/// Pragmas set on every connection; the reasons are in the design spec.
fn configure(conn: &Connection) -> Result<(), StoreError> {
    conn.busy_timeout(Duration::from_secs(5))?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA trusted_schema = OFF;
         PRAGMA secure_delete = ON;
         PRAGMA cache_size = -2000;
         PRAGMA mmap_size = 0;",
    )?;
    Ok(())
}

/// Renames the database and its `-wal`/`-shm` to `*.corrupt-<unix time>`.
fn quarantine(path: &Path) -> Result<PathBuf, StoreError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .ok();
    quarantine_at(path, now)
}

/// [`quarantine`] with the clock reading injected (`None`: the clock failed).
fn quarantine_at(path: &Path, now: Option<u64>) -> Result<PathBuf, StoreError> {
    let mut stamp = now.unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let moves = |stamp: u64| -> Vec<(PathBuf, PathBuf)> {
        ["", "-wal", "-shm"]
            .iter()
            .map(|suffix| {
                (
                    path.with_file_name(format!("{name}{suffix}")),
                    path.with_file_name(format!("{name}{suffix}.corrupt-{stamp}")),
                )
            })
            .collect()
    };
    // Never replace an earlier copy: two damaged files in one second get
    // consecutive stamps.
    while moves(stamp).iter().any(|(_, to)| to.exists()) {
        stamp += 1;
    }
    let moves = moves(stamp);
    let main_target = moves[0].1.clone();
    let mut done: Vec<&(PathBuf, PathBuf)> = Vec::new();
    for pair in &moves {
        if !pair.0.exists() {
            continue;
        }
        if let Err(e) = std::fs::rename(&pair.0, &pair.1) {
            // Undo, so a stale `-wal` never meets a fresh database.
            for (from, to) in done.iter().rev().map(|p| (&p.0, &p.1)) {
                let _ = std::fs::rename(to, from);
            }
            return Err(StoreError::Other(format!(
                "could not move the damaged data aside: {e}"
            )));
        }
        done.push(pair);
    }
    // With no usable clock the fresh copy is stamped 0, which would sort as
    // the oldest and be deleted at once, so nothing is pruned then.
    if now.is_some() {
        prune_quarantined(path, &name, KEEP_CORRUPT_COPIES);
    }
    Ok(main_target)
}

/// How many damaged-database copies are kept (#795). Each holds a full copy of
/// the egress log and the sealed profile rows, so they are not kept forever.
const KEEP_CORRUPT_COPIES: usize = 2;

/// The stamp in `<base>[-wal|-shm].corrupt-<unix time>`, if `file` is one.
fn corrupt_stamp(file: &str, base: &str) -> Option<u64> {
    let rest = file.strip_prefix(base)?;
    let rest = rest
        .strip_prefix("-wal")
        .or_else(|| rest.strip_prefix("-shm"))
        .unwrap_or(rest);
    let digits = rest.strip_prefix(".corrupt-")?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// The stamps to delete: all but the `keep` newest.
fn stamps_to_prune(mut stamps: Vec<u64>, keep: usize) -> Vec<u64> {
    stamps.sort_unstable_by(|a, b| b.cmp(a));
    stamps.dedup();
    stamps.split_off(keep.min(stamps.len()))
}

/// Deletes older quarantined copies, keeping the newest `keep`. Best effort: a
/// file that cannot be listed or removed is left alone.
fn prune_quarantined(path: &Path, base: &str, keep: usize) {
    let Some(dir) = path.parent() else { return };
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let files: Vec<(String, u64)> = entries
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let stamp = corrupt_stamp(&name, base)?;
            Some((name, stamp))
        })
        .collect();
    let drop_stamps = stamps_to_prune(files.iter().map(|(_, s)| *s).collect(), keep);
    for (name, stamp) in files {
        if drop_stamps.contains(&stamp) {
            let _ = std::fs::remove_file(dir.join(name));
        }
    }
}

// -- the process-wide store ------------------------------------------------

/// The outcome of the one open attempt this run. A failure is kept too, so
/// a broken store does not re-run the ACL step and the migrations (a spawned
/// `icacls` each time) on every request.
static GLOBAL: Slot = Mutex::new(None);
static NOTICE: Mutex<Option<String>> = Mutex::new(None);
static NOTICE_USED: AtomicBool = AtomicBool::new(false);

/// `%LOCALAPPDATA%\Wingman\store\wingman.db`. Under `#[cfg(test)]` a
/// per-process directory in `%TEMP%` instead (AGENTS.md rule 9).
fn db_path() -> Result<PathBuf, StoreError> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir()
            .join(format!("wingman-test-store-{}", std::process::id()))
            .join("wingman.db"))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::local_app_data()
            .map_err(|e| StoreError::Other(format!("no local app data folder: {e}")))?;
        Ok(base.join("Wingman").join("store").join("wingman.db"))
    }
}

/// `%LOCALAPPDATA%\Wingman\spend.json`, the file this store replaced. Under
/// `#[cfg(test)]` a per-process file in `%TEMP%` (rule 9).
fn legacy_spend_path() -> Result<PathBuf, StoreError> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("wingman-test-spend-{}.json", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::local_app_data()
            .map_err(|e| StoreError::Other(format!("no local app data folder: {e}")))?;
        Ok(base.join("Wingman").join("spend.json"))
    }
}

/// `%LOCALAPPDATA%\Wingman\egress.log`, the file the `egress` table
/// replaced. Under `#[cfg(test)]` a per-process file in `%TEMP%` (rule 9).
fn legacy_egress_path() -> Result<PathBuf, StoreError> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("wingman-test-egress-{}.log", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::local_app_data()
            .map_err(|e| StoreError::Other(format!("no local app data folder: {e}")))?;
        Ok(base.join("Wingman").join("egress.log"))
    }
}

/// `%LOCALAPPDATA%\Wingman\egress-pending.log`: where request log rows wait
/// while the store cannot open (`egress::record`). Under `#[cfg(test)]` a
/// per-process file in `%TEMP%` (rule 9).
fn pending_egress_path() -> Result<PathBuf, StoreError> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!(
            "wingman-test-egress-pending-{}.log",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::local_app_data()
            .map_err(|e| StoreError::Other(format!("no local app data folder: {e}")))?;
        Ok(base.join("Wingman").join("egress-pending.log"))
    }
}

/// `profile.bin` in the roaming folder, the file the `profile_field` table
/// replaced. Under `#[cfg(test)]` a per-process file in `%TEMP%` (rule 9).
fn legacy_profile_path() -> Result<PathBuf, StoreError> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!("wingman-test-profile-{}.bin", std::process::id())))
    }
    #[cfg(not(test))]
    {
        let base = crate::known_folder::roaming_app_data()
            .map_err(|e| StoreError::Other(format!("no app data folder: {e}")))?;
        Ok(base.join("Wingman").join("profile.bin"))
    }
}

/// The folder the database lives in, for cards that say where data is kept.
pub fn data_dir() -> Option<PathBuf> {
    db_path().ok()?.parent().map(Path::to_path_buf)
}

/// The old files that are imported once, then renamed `.migrated`, and the
/// pending file that is imported on every open that finds it.
struct Legacy<'a> {
    spend: &'a Path,
    egress: &'a Path,
    pending: &'a Path,
    profile: &'a Path,
}

/// The real open, at the real paths.
fn open_default() -> Result<Store, StoreError> {
    sweep_stale_test_dirs();
    let (spend, egress, pending, profile) = (
        legacy_spend_path()?,
        legacy_egress_path()?,
        pending_egress_path()?,
        legacy_profile_path()?,
    );
    open_in(
        &db_path()?,
        &Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &profile,
        },
        acl::restrict_dir,
    )
}

/// Written into the store folder once its owner-only restriction succeeded.
/// The `-2` is #995: folders restricted before it still carry SYSTEM and
/// Administrators entries, so the old marker no longer counts and the next
/// launch restricts them again (one `icacls` call, once).
const ACL_MARKER: &str = ".owner-only-2";

/// Owner-only directory, recovery, migrations, imports.
///
/// The directory is restricted once, not on every launch: spawning `icacls`
/// costs tens of milliseconds on the UI thread at startup (MEASURED
/// 2026-10-15, see the PR for the numbers). "Once" means until the marker
/// file exists, so a launch cut short after the folder was made, or a folder
/// first made in portable mode, is restricted on the next normal launch. If
/// the restriction fails on a folder made this run, the folder is removed
/// again so the next launch retries instead of trusting an unrestricted one.
/// Limit: the marker records only that the step ran once, so an ACL reset by
/// someone later is not detected.
fn open_in(
    path: &Path,
    legacy: &Legacy<'_>,
    restrict: impl FnOnce(&Path) -> Result<(), StoreError>,
) -> Result<Store, StoreError> {
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::Other("the store path has no folder".to_string()))?;
    // A marker file records that the restriction finished. A folder with no
    // marker was either interrupted between create and restrict, or made in
    // portable mode (no ACL by design) and now opened in normal mode, so it
    // is restricted now. One `icacls` call, once.
    let marker = dir.join(ACL_MARKER);
    if !dir.exists() || !marker.exists() {
        let created = !dir.exists();
        if created {
            std::fs::create_dir_all(dir).map_err(|e| StoreError::Other(e.to_string()))?;
        }
        if let Err(e) = restrict(dir) {
            if created {
                let _ = std::fs::remove_dir(dir);
            }
            return Err(e);
        }
        // Portable mode skips the restriction, so it must not claim it ran.
        if !crate::portable::skip_acl() {
            // Best effort: a missing marker only means one more restrict.
            let _ = std::fs::write(&marker, b"");
        }
    }
    let (store, recovered) = Store::open_recovering(path)?;
    if let Some(n) = recovered {
        note(n);
    }
    if let Some(n) = import::spend_json(&mut store.lock(), legacy.spend)? {
        note(n);
    }
    for n in request_log_notices(&store, legacy) {
        note(n);
    }
    if let Some(n) = profile_notice(&store, legacy) {
        note(n);
    }
    Ok(store)
}

/// Runs the request log imports. They are not allowed to take the store
/// down: a failure is a notice, and the store still opens (rows would
/// otherwise have nowhere to go for the whole run). Each import takes the
/// lock on its own; guards held across both would deadlock the second.
fn request_log_notices(store: &Store, legacy: &Legacy<'_>) -> Vec<String> {
    let old_log = import::egress_log(&mut store.lock(), legacy.egress);
    let pending = import::egress_pending(&mut store.lock(), legacy.pending);
    [old_log, pending]
        .into_iter()
        .filter_map(|r| match r {
            Ok(n) => n,
            Err(e) => Some(e.to_string()),
        })
        .collect()
}

/// Imports the old `profile.bin`. Like the request logs it is not allowed to
/// take the store down: any failure is a notice, the old file is left where
/// it is, and the next launch tries again.
fn profile_notice(store: &Store, legacy: &Legacy<'_>) -> Option<String> {
    match import::profile_bin(&mut store.lock(), legacy.profile) {
        Ok(n) => n,
        Err(e) => Some(e.to_string()),
    }
}

/// Test builds only: removes `wingman-test-store-<pid>` directories left in
/// `%TEMP%` by earlier runs (older than an hour, so a test process running
/// right now in another worktree keeps its own).
#[cfg(test)]
fn sweep_stale_test_dirs() {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return;
    };
    let hour = Duration::from_secs(3600);
    for e in entries.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with("wingman-test-store-") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > hour);
        if old {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

#[cfg(not(test))]
fn sweep_stale_test_dirs() {}

/// The shared store, opened on first use. The result of that one attempt,
/// success or failure, is kept for the rest of the run; a failure is reported
/// once. Restarting Wingman is the retry.
pub fn global() -> Result<Arc<Store>, StoreError> {
    global_with(&GLOBAL, open_default)
}

/// The shared store if an earlier call already opened it, without opening it
/// and without waiting for an open that is under way: `None` means "not ready
/// yet". For the UI thread (#1010), where the first open (an `icacls` spawn
/// and SQLite) would stall the keyboard hook. [`open_in_background`] does the
/// open at start-up. No caller right now (the tooltip read moved to a worker,
/// #1181); kept for the next UI-thread reader.
#[allow(dead_code)]
pub fn global_if_ready() -> Option<Result<Arc<Store>, StoreError>> {
    ready_in(&GLOBAL)
}

fn ready_in(slot: &Slot) -> Option<Result<Arc<Store>, StoreError>> {
    slot.try_lock().ok()?.clone()
}

/// Opens the shared store once on a short-lived thread, so no UI-thread caller
/// ever pays for the first open (#1010). One shot at start-up, no timer. If
/// the open fails, the failure is kept and reported on the next card, exactly
/// as when a request opens it; a request still calls [`global`] itself.
pub fn open_in_background() {
    let _ = std::thread::Builder::new()
        .name("store-open".into())
        .spawn(|| {
            let _ = global();
        });
}

type Slot = Mutex<Option<Result<Arc<Store>, StoreError>>>;

fn global_with(
    slot: &Slot,
    open: impl FnOnce() -> Result<Store, StoreError>,
) -> Result<Arc<Store>, StoreError> {
    let mut slot = slot.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(done) = slot.as_ref() {
        return done.clone();
    }
    let opened = open().map(Arc::new);
    if let Err(e) = &opened {
        report(e);
    }
    *slot = Some(opened.clone());
    opened
}

/// Records a store failure for the next card. Only the first one per run is
/// kept, so a broken store cannot flood the user with cards.
pub fn report(e: &StoreError) {
    note(e.to_string());
}

fn note(text: String) {
    if !NOTICE_USED.swap(true, Ordering::SeqCst) {
        *NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
    }
}

/// Like [`note`], but a recovery replaces an earlier notice: it is the one the
/// user most needs to read.
fn note_recovery(text: String) {
    NOTICE_USED.store(true, Ordering::SeqCst);
    *NOTICE.lock().unwrap_or_else(|e| e.into_inner()) = Some(text);
}

/// Takes the pending notice, if any. `app.rs` adds it to the next card.
pub fn take_notice() -> Option<String> {
    NOTICE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-store-t-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_fresh_open_is_at_the_latest_version_with_the_tables() {
        let dir = scratch("fresh");
        let s = Store::open_at(&dir.join("wingman.db")).unwrap();
        let conn = s.lock();
        assert_eq!(
            migrate::current_version(&conn).unwrap(),
            migrate::MIGRATIONS.last().unwrap().version
        );
        for table in ["meta", "usage_day", "egress", "profile_field"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        let mode: String = conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store with one root and one indexed document, for the knowledge
    /// table tests below. Goes through `open_at`, so the real pragmas
    /// (foreign keys, trusted_schema off, secure_delete) are in force.
    fn knowledge_store(name: &str) -> (Store, PathBuf) {
        let dir = scratch(name);
        let s = Store::open_at(&dir.join("wingman.db")).unwrap();
        {
            let conn = s.lock();
            conn.execute_batch(
                "INSERT INTO knowledge_root (id, path, kind, added_unix)
                     VALUES (1, 'C:/Notes', 'default', 10);
                 INSERT INTO document (id, root_id, rel_path, status)
                     VALUES (1, 1, 'notes.md', 'indexed');",
            )
            .unwrap();
        }
        (s, dir)
    }

    fn fts_hits(conn: &Connection, term: &str) -> i64 {
        conn.query_row(
            "SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH ?1",
            [term],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn a_fresh_open_has_the_knowledge_tables() {
        let dir = scratch("kn-tables");
        let s = Store::open_at(&dir.join("wingman.db")).unwrap();
        let conn = s.lock();
        for table in ["knowledge_root", "document", "chunk", "chunk_fts"] {
            let n: i64 = conn
                .query_row(
                    "SELECT count(*) FROM sqlite_master WHERE name = ?1",
                    [table],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "{table}");
        }
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn chunk_text_is_searchable_the_moment_it_is_inserted() {
        let (s, dir) = knowledge_store("kn-insert");
        let conn = s.lock();
        conn.execute(
            "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
             VALUES (1, 0, 'My start date at Acme was 3 March', 'h0', 8)",
            [],
        )
        .unwrap();
        assert_eq!(fts_hits(&conn, "acme"), 1);
        assert_eq!(fts_hits(&conn, "zebra"), 0);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_edited_chunk_is_searchable_by_its_new_words_only() {
        let (s, dir) = knowledge_store("kn-update");
        let conn = s.lock();
        conn.execute(
            "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
             VALUES (1, 0, 'alpha beta', 'h0', 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE chunk SET text = 'gamma delta' WHERE ordinal = 0",
            [],
        )
        .unwrap();
        assert_eq!(fts_hits(&conn, "alpha"), 0);
        assert_eq!(fts_hits(&conn, "gamma"), 1);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn deleting_a_document_leaves_no_chunk_and_no_full_text_row() {
        // The done-when of #83: delete the file, and nothing of it remains.
        let (s, dir) = knowledge_store("kn-delete");
        let conn = s.lock();
        conn.execute(
            "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
             VALUES (1, 0, 'findable words', 'h0', 3), (1, 1, 'more findable', 'h1', 3)",
            [],
        )
        .unwrap();
        assert_eq!(fts_hits(&conn, "findable"), 2);

        conn.execute("DELETE FROM document WHERE id = 1", [])
            .unwrap();

        let chunks: i64 = conn
            .query_row("SELECT count(*) FROM chunk", [], |r| r.get(0))
            .unwrap();
        assert_eq!(chunks, 0);
        assert_eq!(fts_hits(&conn, "findable"), 0);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn removing_a_root_cascades_to_its_documents_and_chunks() {
        let (s, dir) = knowledge_store("kn-root");
        let conn = s.lock();
        conn.execute(
            "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
             VALUES (1, 0, 'rooted text', 'h0', 2)",
            [],
        )
        .unwrap();

        conn.execute("DELETE FROM knowledge_root WHERE id = 1", [])
            .unwrap();

        let rows: i64 = conn
            .query_row(
                "SELECT (SELECT count(*) FROM document) + (SELECT count(*) FROM chunk)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 0);
        assert_eq!(fts_hits(&conn, "rooted"), 0);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_document_path_is_unique_within_its_root() {
        let (s, dir) = knowledge_store("kn-unique");
        let conn = s.lock();
        let dup = conn.execute(
            "INSERT INTO document (root_id, rel_path, status) VALUES (1, 'notes.md', 'pending')",
            [],
        );
        assert!(dup.is_err());
        // The same relative path under another root is a different document.
        conn.execute(
            "INSERT INTO knowledge_root (id, path, kind, added_unix) VALUES (2, 'D:/Vault', 'vault', 11)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO document (root_id, rel_path, status) VALUES (2, 'notes.md', 'pending')",
            [],
        )
        .unwrap();
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_document_status_outside_the_four_known_ones_is_refused() {
        let (s, dir) = knowledge_store("kn-status");
        let conn = s.lock();
        let bad = conn.execute(
            "INSERT INTO document (root_id, rel_path, status) VALUES (1, 'x.md', 'weird')",
            [],
        );
        assert!(bad.is_err());
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn diacritics_do_not_hide_a_word() {
        let (s, dir) = knowledge_store("kn-diacritics");
        let conn = s.lock();
        conn.execute(
            "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
             VALUES (1, 0, 'Curriculum vitae of Zoë Müller', 'h0', 5)",
            [],
        )
        .unwrap();
        assert_eq!(fts_hits(&conn, "zoe"), 1);
        assert_eq!(fts_hits(&conn, "muller"), 1);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fts5_is_compiled_in() {
        // The knowledge track (#85) needs FTS5; the bundled build must have it.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE VIRTUAL TABLE t USING fts5(x);
             INSERT INTO t VALUES ('the quick brown fox');",
        )
        .unwrap();
        let hits: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'quick'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(hits, 1);
    }

    /// #1095: the bundled SQLite is built without its JSON functions (about
    /// 36 KB). No store SQL uses them; this fails if the flag in
    /// `.cargo/config.toml` is dropped or stops reaching the build, and tells
    /// the next slice that wants `json_extract` where to turn it back on.
    #[test]
    fn the_bundled_sqlite_has_no_json_functions() {
        let conn = Connection::open_in_memory().unwrap();
        let r: rusqlite::Result<String> = conn.query_row("SELECT json('{}')", [], |r| r.get(0));
        assert!(
            r.is_err(),
            "json() exists: -DSQLITE_OMIT_JSON is not applied"
        );
    }

    #[test]
    fn a_damaged_file_is_moved_aside_and_a_fresh_store_opens() {
        let dir = scratch("corrupt");
        let path = dir.join("wingman.db");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();

        let (s, notice) = Store::open_recovering(&path).unwrap();

        let notice = notice.expect("a damaged file must produce a notice");
        assert!(notice.contains("corrupt-"), "{notice}");
        assert!(!notice.contains('\u{2014}'));
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        let moved: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".db.corrupt-"))
            .collect();
        assert_eq!(moved.len(), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store with one saved day, then its file overwritten with garbage
    /// while the connection stays open (#660). The checkpoint moves the data
    /// into the main file and `shrink_memory` drops the page cache, so the next
    /// query really reads the garbage.
    fn store_damaged_while_open(name: &str) -> (Store, PathBuf) {
        let dir = scratch(name);
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        s.lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA shrink_memory;")
            .unwrap();
        std::fs::write(&path, vec![b'x'; 8192]).unwrap();
        (s, dir)
    }

    /// Quarantined main files only: `wingman.db.corrupt-<n>`, not the `-wal`
    /// and `-shm` copies that come with it.
    fn damaged_copies(dir: &Path) -> usize {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                name.contains(".db.corrupt-") && corrupt_stamp(&name, "wingman.db").is_some()
            })
            .count()
    }

    /// Lets the next `lock()` retry a stuck heal at once (the real wait is
    /// `RETRY_GAP`).
    fn expire_retry_wait(_s: &Store) {
        _s.heal_state.lock().unwrap().not_before = None;
    }

    #[test]
    fn a_file_damaged_while_running_is_replaced_by_a_fresh_store() {
        let (s, dir) = store_damaged_while_open("runtime-corrupt");

        // Another test's corrupt error can make this very call heal first.
        let first = s.day_total("2026-10-15");
        assert!(
            matches!(first, Err(StoreError::Corrupt(_)) | Ok(None)),
            "{first:?}"
        );

        // The next call heals: the old file is kept, a fresh one works.
        assert_eq!(s.day_total("2026-10-15").unwrap(), None);
        s.record_usage("2026-10-16", "a", "m", Some(0.2), None)
            .unwrap();
        assert!(s.day_total("2026-10-16").unwrap().is_some());
        assert_eq!(damaged_copies(&dir), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1158: whoever holds rows from the old file (the knowledge watcher keeps
    /// a root id) is told when the file was replaced, and not before.
    #[test]
    fn a_replaced_store_tells_its_subscribers_once() {
        let (s, dir) = store_damaged_while_open("runtime-subscribers");
        let s = Arc::new(s);
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        let sub = s.on_replaced(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing replaced yet");

        // Another test's corrupt error can make the first call heal already.
        let _ = s.day_total("2026-10-15");
        let _ = s.day_total("2026-10-15");
        assert_eq!(damaged_copies(&dir), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(sub);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A store whose connection was swapped for the empty in-memory stand-in
    /// and never got a file connection back (#1165): what a failed undo leaves.
    fn detached_store(s: &Store) {
        let empty = Connection::open_in_memory().unwrap();
        drop(std::mem::replace(
            &mut *s.conn.lock().unwrap_or_else(|e| e.into_inner()),
            empty,
        ));
        s.heal_state.lock().unwrap().detached = true;
        // Any corrupt-class error in the process makes every store check itself.
        let _ = StoreError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
    }

    /// The undo worked but reopening failed once: the next lock must reopen
    /// the file, not pass a check on the empty in-memory database.
    #[test]
    fn a_detached_store_reopens_its_untouched_file_on_the_next_lock() {
        let dir = scratch("detached-healthy");
        let s = Store::open_at(&dir.join("wingman.db")).unwrap();
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        detached_store(&s);

        assert!(s.day_total("2026-10-15").unwrap().is_some());
        assert_eq!(damaged_copies(&dir), 0, "a healthy file is not moved");
        assert!(!s.heal_state.lock().unwrap().detached);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A dropped subscription is not called any more.
    #[test]
    fn a_dropped_subscription_is_not_called() {
        let (s, dir) = store_damaged_while_open("runtime-unsubscribed");
        let s = Arc::new(s);
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        drop(s.on_replaced(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        })));
        let _ = s.day_total("2026-10-15");
        let _ = s.day_total("2026-10-15");
        assert_eq!(damaged_copies(&dir), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Detached and the file really is damaged: it goes aside and a fresh one
    /// opens, so the store is not left empty until restart.
    #[test]
    fn a_detached_store_over_a_damaged_file_gets_a_fresh_one() {
        let dir = scratch("detached-damaged");
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        s.lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA shrink_memory;")
            .unwrap();
        std::fs::write(&path, vec![b'x'; 8192]).unwrap();
        detached_store(&s);

        assert_eq!(s.day_total("2026-10-15").unwrap(), None);
        s.record_usage("2026-10-16", "a", "m", Some(0.2), None)
            .unwrap();
        assert_eq!(damaged_copies(&dir), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A chunk saved while the full-text triggers are gone: the table is fine,
    /// the index no longer agrees with it. `quick_check` cannot see that.
    fn store_with_index_out_of_step(name: &str) -> (Store, PathBuf) {
        let (s, dir) = knowledge_store(name);
        {
            let conn = s.lock();
            conn.execute_batch("DROP TRIGGER chunk_fts_insert;")
                .unwrap();
            conn.execute(
                "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
                 VALUES (1, 0, 'ledgerword in the notes', 'h0', 4)",
                [],
            )
            .unwrap();
            conn.execute_batch(
                "CREATE TRIGGER chunk_fts_insert AFTER INSERT ON chunk BEGIN
                     INSERT INTO chunk_fts (rowid, text) VALUES (new.id, new.text);
                 END;",
            )
            .unwrap();
            assert_eq!(fts_hits(&conn, "ledgerword"), 0, "the index misses it");
            assert!(!file_is_damaged(&conn), "quick_check passes");
        }
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        (s, dir)
    }

    /// #1159: an index that disagrees with its table is rebuilt in place. The
    /// file is not moved aside and the usage history stays.
    #[test]
    fn an_index_out_of_step_is_rebuilt_without_losing_anything() {
        let _serial = FTS_EVENT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (s, dir) = store_with_index_out_of_step("fts-rebuild");
        // A virtual-table error is what a mismatched index raises.
        let _ = StoreError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT_VTAB),
            None,
        ));

        assert!(s.day_total("2026-10-15").unwrap().is_some(), "history kept");
        assert_eq!(s.fts_hits("ledgerword").unwrap(), 1, "searchable again");
        assert_eq!(damaged_copies(&dir), 0, "the file stays where it is");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Tests that move the process-wide virtual-table counter run one at a
    /// time, so one cannot make another store check its index.
    static FTS_EVENT_TESTS: Mutex<()> = Mutex::new(());

    #[test]
    fn the_extended_code_for_a_damaged_virtual_table_counts_as_corrupt() {
        let vtab = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT_VTAB),
            None,
        );
        assert_eq!(rusqlite::ffi::SQLITE_CORRUPT_VTAB, 267);
        assert_eq!(rusqlite::ffi::SQLITE_CORRUPT_VTAB & 0xff, 11);
        assert!(is_corrupt(&vtab));
        assert!(is_fts_corrupt(&vtab));
        let plain = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        );
        assert!(is_corrupt(&plain));
        assert!(!is_fts_corrupt(&plain));
    }

    /// A rebuild that fails for a reason other than damage is retried later,
    /// not reported as healthy.
    #[test]
    fn a_rebuild_that_fails_for_another_reason_is_retried() {
        let busy = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        );
        assert!(matches!(
            rebuild_verdict(Err(busy)),
            RebuildVerdict::Retry(_)
        ));
        let full = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
            None,
        );
        assert!(matches!(
            rebuild_verdict(Err(full)),
            RebuildVerdict::Retry(_)
        ));
        let vtab = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT_VTAB),
            None,
        );
        assert!(matches!(
            rebuild_verdict(Err(vtab)),
            RebuildVerdict::Damaged
        ));
        assert!(matches!(rebuild_verdict(Ok(())), RebuildVerdict::Rebuilt));
    }

    /// Real damage to the full-text index, one kind at a time.
    fn store_with_indexed_chunks(name: &str) -> (Store, PathBuf) {
        let (s, dir) = knowledge_store(name);
        {
            let conn = s.lock();
            for (i, text) in [
                "ledgerword in the notes",
                "second page about ledgerword budgets",
                "third unrelated paragraph",
                "fourth ledgerword entry",
            ]
            .iter()
            .enumerate()
            {
                conn.execute(
                    "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
                     VALUES (1, ?1, ?2, ?3, 4)",
                    rusqlite::params![i as i64, text, format!("h{i}")],
                )
                .unwrap();
            }
            assert_eq!(fts_hits(&conn, "ledgerword"), 3);
        }
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        (s, dir)
    }

    /// The extended code a query raised, or `None` when it ran.
    fn extended_code(conn: &Connection, sql: &str) -> Option<i32> {
        match conn.query_row(sql, [], |r| r.get::<_, i64>(0)) {
            Ok(_) => None,
            Err(rusqlite::Error::SqliteFailure(f, _)) => Some(f.extended_code),
            Err(e) => panic!("not a SQLite failure: {e}"),
        }
    }

    const MATCH_COUNT: &str = "SELECT count(*) FROM chunk_fts WHERE chunk_fts MATCH 'ledgerword'";

    /// MEASURED 2026-10-17 (SQLite bundled with rusqlite): real damage to the
    /// full-text index raises extended code 267 (`SQLITE_CORRUPT_VTAB`), so
    /// `is_fts_corrupt` recognises it, for the index's segment rows and for
    /// its `docsize` rows. Only the `docsize` kind passes `quick_check`, so
    /// only that one reaches the in-place rebuild. Damage to the segment rows
    /// is also seen by `quick_check`, so the whole file is moved aside.
    #[test]
    fn real_index_damage_raises_the_virtual_table_code() {
        let _serial = FTS_EVENT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let query = "\"ledgerword\"";

        // docsize rows gone: 267 on a ranked read, quick_check passes, index rebuilt.
        let (s, dir) = store_with_indexed_chunks("fts-real-docsize");
        {
            let conn = s.lock();
            conn.execute_batch("DELETE FROM chunk_fts_docsize;")
                .unwrap();
            assert_eq!(
                extended_code(&conn, MATCH_COUNT),
                None,
                "a bare count does not read docsize"
            );
            assert!(!file_is_damaged(&conn), "quick_check passes");
        }
        let before = FTS_CORRUPT_EVENTS.load(Ordering::SeqCst);
        let err = s.search_chunks("C:/Notes", query, 10).unwrap_err();
        assert!(matches!(err, StoreError::Corrupt(_)), "{err:?}");
        assert_eq!(FTS_CORRUPT_EVENTS.load(Ordering::SeqCst), before + 1);
        assert!(s.day_total("2026-10-15").unwrap().is_some(), "history kept");
        assert_eq!(s.fts_hits("ledgerword").unwrap(), 3, "index rebuilt");
        assert_eq!(s.search_chunks("C:/Notes", query, 10).unwrap().len(), 3);
        assert_eq!(damaged_copies(&dir), 0, "the file stays where it is");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);

        // Segment rows gone: 267 on read, and quick_check sees it too.
        let (s, dir) = store_with_indexed_chunks("fts-real-segments");
        {
            let conn = s.lock();
            conn.execute_batch("DELETE FROM chunk_fts_data WHERE id > 10;")
                .unwrap();
            assert_eq!(
                extended_code(&conn, MATCH_COUNT),
                Some(rusqlite::ffi::SQLITE_CORRUPT_VTAB)
            );
            assert!(file_is_damaged(&conn), "quick_check sees this kind");
        }
        let before = FTS_CORRUPT_EVENTS.load(Ordering::SeqCst);
        assert!(s.search_chunks("C:/Notes", query, 10).is_err());
        assert_eq!(FTS_CORRUPT_EVENTS.load(Ordering::SeqCst), before + 1);
        assert_eq!(s.day_total("2026-10-15").unwrap(), None, "history lost");
        assert_eq!(damaged_copies(&dir), 1, "the whole file was moved aside");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A corrupt error that did not come from the full-text index does not
    /// pay for the O(corpus) index check.
    #[test]
    fn a_corrupt_error_outside_the_index_does_not_rebuild_it() {
        let _serial = FTS_EVENT_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        let (s, dir) = store_with_index_out_of_step("fts-not-checked");
        let _ = StoreError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));

        assert!(s.day_total("2026-10-15").unwrap().is_some());
        assert_eq!(s.fts_hits("ledgerword").unwrap(), 0, "index left alone");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// An index that agrees with its table is left alone.
    #[test]
    fn a_healthy_index_is_not_rebuilt() {
        let (s, dir) = knowledge_store("fts-healthy");
        {
            let conn = s.lock();
            conn.execute(
                "INSERT INTO chunk (doc_id, ordinal, text, text_hash, tokens)
                 VALUES (1, 0, 'ledgerword in the notes', 'h0', 4)",
                [],
            )
            .unwrap();
            assert!(!fts_index_out_of_step(&conn));
        }
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file is gone: opening it creates a new one, and the notice must say
    /// so rather than claim the old data is back.
    #[test]
    fn a_detached_store_whose_file_is_gone_says_it_started_a_new_one() {
        let dir = scratch("detached-gone");
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        detached_store(&s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.join(format!("wingman.db{suffix}")));
        }

        let mut conn = s.conn.lock().unwrap_or_else(|e| e.into_inner());
        let mut state = s.heal_state.lock().unwrap_or_else(|e| e.into_inner());
        match s.heal(&mut conn, &mut state) {
            Heal::Replaced(text) => {
                assert!(text.contains("missing"), "{text}");
                assert!(!text.contains("open again"), "{text}");
                assert!(!text.contains('\u{2014}'), "{text}");
            }
            Heal::Reopened => panic!("a new file is not a reopen"),
            _ => panic!("expected a replaced store"),
        }
        assert!(path.exists());
        drop(state);
        drop(conn);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// #1158: a file that went missing (not damaged) is replaced by an empty
    /// one, so subscribers holding old row ids are told to start again.
    #[test]
    fn a_store_started_over_a_missing_file_tells_its_subscribers() {
        let dir = scratch("missing-subscribers");
        let path = dir.join("wingman.db");
        let s = Arc::new(Store::open_at(&path).unwrap());
        detached_store(&s);
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(dir.join(format!("wingman.db{suffix}")));
        }
        let calls = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&calls);
        let sub = s.on_replaced(Arc::new(move || {
            counted.fetch_add(1, Ordering::SeqCst);
        }));
        assert_eq!(calls.load(Ordering::SeqCst), 0);

        assert_eq!(s.day_total("2026-10-15").unwrap(), None);
        assert!(path.exists());
        assert_eq!(damaged_copies(&dir), 0, "nothing was damaged");
        assert_eq!(calls.load(Ordering::SeqCst), 1);

        drop(sub);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// While the file cannot be opened, a detached store tries once per gap,
    /// not on every lock.
    #[test]
    fn a_detached_store_opens_the_file_once_per_retry_gap() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("detached-gap");
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        s.lock()
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA shrink_memory;")
            .unwrap();
        detached_store(&s);
        // No sharing at all: opening the file fails until the hold is dropped.
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&path)
            .unwrap();

        drop(s.lock());
        assert!(s.heal_state.lock().unwrap().detached);
        assert!(s.heal_state.lock().unwrap().not_before.is_some());
        drop(hold);
        // The file is free now, but the gap has not passed: no attempt.
        drop(s.lock());
        drop(s.lock());
        assert!(s.heal_state.lock().unwrap().detached, "the gap was ignored");
        expire_retry_wait(&s);
        drop(s.lock());
        assert!(!s.heal_state.lock().unwrap().detached);
        assert!(s.day_total("2026-10-15").unwrap().is_some());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The undo after a failed move reports whether a file connection is back.
    #[test]
    fn reopen_original_reports_whether_it_got_the_file_back() {
        let dir = scratch("reopen-original");
        let path = dir.join("wingman.db");
        let mut conn = Connection::open_in_memory().unwrap();
        assert!(
            !reopen_original(&mut conn, &path),
            "no file, nothing created"
        );
        assert!(!path.exists());
        // A directory where the file should be cannot be opened as a database.
        std::fs::create_dir(&path).unwrap();
        assert!(!reopen_original(&mut conn, &path));
        std::fs::remove_dir(&path).unwrap();
        drop(Store::open_at(&path).unwrap());
        assert!(reopen_original(&mut conn, &path));
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_reopened_notice_is_plain() {
        let text = reopened_notice();
        assert!(!text.contains('\u{2014}'), "{text}");
        assert!(!text.contains("copy was kept"), "no file was moved: {text}");
    }

    #[test]
    fn a_healthy_store_is_never_moved_aside_after_a_corrupt_error_elsewhere() {
        let dir = scratch("runtime-healthy");
        let s = Store::open_at(&dir.join("wingman.db")).unwrap();
        s.record_usage("2026-10-15", "a", "m", Some(0.1), None)
            .unwrap();
        // Any corrupt-class error in the process makes every store check itself.
        let _ = StoreError::from(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CORRUPT),
            None,
        ));
        assert!(s.day_total("2026-10-15").unwrap().is_some());
        assert_eq!(damaged_copies(&dir), 0);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn if_the_damaged_file_cannot_be_moved_the_store_fails_without_panicking() {
        use std::os::windows::fs::OpenOptionsExt;
        let (s, dir) = store_damaged_while_open("runtime-stuck");
        // Share read and write but not delete: the rename must fail.
        let _hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(dir.join("wingman.db"))
            .unwrap();

        assert!(s.day_total("2026-10-15").is_err());
        // Healing fails here; every later call is an ordinary error, not a panic.
        assert!(s.day_total("2026-10-15").is_err());
        assert!(s.record_usage("2026-10-16", "a", "m", None, None).is_err());
        assert_eq!(damaged_copies(&dir), 0);
        // The failed move was undone: the file is open again, not detached.
        assert!(!s.heal_state.lock().unwrap().detached);
        drop(_hold);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The file is held for a moment (antivirus, backup): the store keeps the
    /// original file and heals on a later call instead of staying empty.
    #[test]
    fn a_store_that_could_not_move_the_file_heals_once_it_is_free() {
        use std::os::windows::fs::OpenOptionsExt;
        let (s, dir) = store_damaged_while_open("runtime-released");
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(dir.join("wingman.db"))
            .unwrap();

        assert!(s.day_total("2026-10-15").is_err());
        expire_retry_wait(&s);
        assert!(s.day_total("2026-10-15").is_err());
        drop(hold);
        expire_retry_wait(&s);

        assert_eq!(s.day_total("2026-10-15").unwrap(), None);
        s.record_usage("2026-10-16", "a", "m", Some(0.2), None)
            .unwrap();
        assert!(s.day_total("2026-10-16").unwrap().is_some());
        assert_eq!(damaged_copies(&dir), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A file that stays held is not checked on every lock.
    #[test]
    fn a_stuck_heal_waits_before_trying_again() {
        use std::os::windows::fs::OpenOptionsExt;
        let (s, dir) = store_damaged_while_open("runtime-wait");
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(dir.join("wingman.db"))
            .unwrap();
        assert!(s.day_total("2026-10-15").is_err());
        assert!(s.day_total("2026-10-15").is_err());
        drop(hold);

        // Free now, but the wait has not passed: no heal yet.
        assert!(s.day_total("2026-10-15").is_err());
        assert_eq!(damaged_copies(&dir), 0);
        expire_retry_wait(&s);
        assert_eq!(s.day_total("2026-10-15").unwrap(), None);
        assert_eq!(damaged_copies(&dir), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn two_quarantines_in_one_second_keep_both_copies() {
        let dir = scratch("same-second");
        let db = dir.join("wingman.db");
        std::fs::write(&db, b"first").unwrap();
        let a = quarantine_at(&db, Some(500)).unwrap();
        std::fs::write(&db, b"second").unwrap();
        let b = quarantine_at(&db, Some(500)).unwrap();
        assert_ne!(a, b);
        assert_eq!(std::fs::read(&a).unwrap(), b"first");
        assert_eq!(std::fs::read(&b).unwrap(), b"second");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The database moved but its `-wal` could not: the move is undone, so the
    /// stale log never meets a fresh database.
    #[test]
    fn a_half_finished_quarantine_is_undone() {
        use std::os::windows::fs::OpenOptionsExt;
        let dir = scratch("half-move");
        let db = dir.join("wingman.db");
        let wal = dir.join("wingman.db-wal");
        std::fs::write(&db, b"main").unwrap();
        std::fs::write(&wal, b"log").unwrap();
        let hold = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(3)
            .open(&wal)
            .unwrap();

        assert!(quarantine_at(&db, Some(700)).is_err());
        drop(hold);

        assert_eq!(std::fs::read(&db).unwrap(), b"main");
        assert_eq!(std::fs::read(&wal).unwrap(), b"log");
        assert_eq!(damaged_copies(&dir), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leftover_side_files_are_noticed() {
        let dir = scratch("side-files");
        let db = dir.join("wingman.db");
        assert!(!side_files_present(&db));
        std::fs::write(dir.join("wingman.db-shm"), b"x").unwrap();
        assert!(side_files_present(&db));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_runtime_notice_says_what_was_lost_and_where_the_copy_is() {
        let kept = runtime_recovery_notice(Ok(Path::new("C:/x/wingman.db.corrupt-5")));
        assert!(kept.contains("wingman.db.corrupt-5"), "{kept}");
        assert!(kept.contains("history"), "{kept}");
        assert!(kept.contains("documents"), "{kept}");
        let stuck = runtime_recovery_notice(Err("in use"));
        assert!(stuck.contains("in use"), "{stuck}");
        assert!(stuck.contains("not being saved"), "{stuck}");
        for t in [kept, stuck] {
            assert!(!t.contains('\u{2014}'), "{t}");
        }
    }

    #[test]
    fn a_stuck_index_rebuild_does_not_claim_history_is_lost() {
        // #1182: only the search index is affected, history is still saved.
        let t = index_stuck_notice("disk full");
        assert!(t.contains("disk full"), "{t}");
        assert!(t.contains("search"), "{t}");
        assert!(!t.contains("not being saved"), "{t}");
        assert!(!t.contains('\u{2014}'), "{t}");
    }

    #[test]
    fn the_index_notice_never_hides_the_file_notice() {
        let mut state = HealState::default();
        let index = stuck_notice_once(&mut state, true, "busy").unwrap();
        assert!(index.contains("search"), "{index}");
        assert!(stuck_notice_once(&mut state, true, "busy").is_none());
        let file = stuck_notice_once(&mut state, false, "in use").unwrap();
        assert!(file.contains("not being saved"), "{file}");
        assert!(stuck_notice_once(&mut state, false, "in use").is_none());
    }

    #[test]
    fn only_the_newest_damaged_copies_are_kept() {
        // #795
        assert_eq!(stamps_to_prune(vec![5, 9, 7, 3], 2), vec![5, 3]);
        assert!(stamps_to_prune(vec![5, 9], 2).is_empty());
        assert!(stamps_to_prune(vec![], 2).is_empty());
        assert_eq!(stamps_to_prune(vec![4, 4, 8], 1), vec![4]);
    }

    #[test]
    fn corrupt_stamps_are_read_only_from_our_own_file_names() {
        assert_eq!(
            corrupt_stamp("wingman.db.corrupt-12", "wingman.db"),
            Some(12)
        );
        assert_eq!(
            corrupt_stamp("wingman.db-wal.corrupt-12", "wingman.db"),
            Some(12)
        );
        assert_eq!(corrupt_stamp("wingman.db", "wingman.db"), None);
        assert_eq!(corrupt_stamp("wingman.db.corrupt-", "wingman.db"), None);
        assert_eq!(corrupt_stamp("wingman.db.corrupt-1x", "wingman.db"), None);
        assert_eq!(corrupt_stamp("other.db.corrupt-1", "wingman.db"), None);
    }

    #[test]
    fn a_failed_clock_prunes_nothing_so_the_fresh_copy_survives() {
        let dir = std::env::temp_dir().join(format!("wingman-noclock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("wingman.db");
        for stamp in [100, 200] {
            std::fs::write(dir.join(format!("wingman.db.corrupt-{stamp}")), b"x").unwrap();
        }
        std::fs::write(&db, b"damaged").unwrap();
        let kept = quarantine_at(&db, None).unwrap();
        let fresh_survived = kept.exists();
        let old_ones = [100, 200]
            .iter()
            .all(|s| dir.join(format!("wingman.db.corrupt-{s}")).exists());
        let _ = std::fs::remove_dir_all(&dir);
        assert!(fresh_survived, "the fresh copy must not be deleted at once");
        assert!(old_ones, "nothing is pruned without a clock");
    }

    #[test]
    fn a_third_damaged_file_removes_the_oldest_copy() {
        let dir = std::env::temp_dir().join(format!("wingman-prune-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("wingman.db");
        for stamp in [100, 200, 300] {
            std::fs::write(dir.join(format!("wingman.db.corrupt-{stamp}")), b"x").unwrap();
            std::fs::write(dir.join(format!("wingman.db-wal.corrupt-{stamp}")), b"x").unwrap();
        }
        std::fs::write(dir.join("unrelated.txt"), b"x").unwrap();
        prune_quarantined(&db, "wingman.db", 2);
        let mut left: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        left.sort();
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(
            left,
            vec![
                "unrelated.txt",
                "wingman.db-wal.corrupt-200",
                "wingman.db-wal.corrupt-300",
                "wingman.db.corrupt-200",
                "wingman.db.corrupt-300",
            ]
        );
    }

    #[test]
    fn a_newer_file_is_not_quarantined() {
        let dir = scratch("newer");
        let path = dir.join("wingman.db");
        {
            let c = Connection::open(&path).unwrap();
            c.pragma_update(None, "user_version", 99).unwrap();
        }
        let err = Store::open_recovering(&path).err().expect("must refuse");
        assert!(matches!(err, StoreError::Newer { .. }), "{err:?}");
        assert!(path.exists(), "a newer file must stay where it is");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_error_text_is_plain_and_has_no_em_dash() {
        let all = [
            StoreError::Newer { found: 2, known: 1 },
            StoreError::Corrupt("x".into()),
            StoreError::Acl("x".into()),
            StoreError::Migration(2, "x".into()),
            StoreError::Profile("x".into()),
            StoreError::Undecryptable("x".into()),
            StoreError::Refused("x".into()),
            StoreError::Other("x".into()),
        ];
        for e in all {
            let t = e.to_string();
            assert!(t.starts_with("Wingman"), "{t}");
            assert!(!t.contains('\u{2014}'), "{t}");
        }
    }

    #[test]
    fn only_the_first_notice_of_a_run_is_kept() {
        // Process-wide state: take whatever earlier tests left, then check
        // that a second note after the first is dropped.
        let _ = take_notice();
        note("first".to_string());
        note("second".to_string());
        let got = take_notice();
        assert!(got.is_none() || got.as_deref() == Some("first"), "{got:?}");
        assert_eq!(take_notice(), None);
    }

    #[test]
    fn nothing_outside_the_store_module_names_the_sqlite_binding() {
        // The binding's name is built from pieces so this file's own text
        // does not match it. CRLF-normalised, and the candidate set must be
        // non-empty or the test would pass while checking nothing.
        let needle = format!("{}::", "rusqlite");
        let mut stack = vec![PathBuf::from("src")];
        let mut checked = 0;
        while let Some(dir) = stack.pop() {
            for e in std::fs::read_dir(&dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() {
                    if p.file_name().is_some_and(|n| n == "store") {
                        continue;
                    }
                    stack.push(p);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    let text = std::fs::read_to_string(&p).unwrap().replace('\r', "");
                    checked += 1;
                    assert!(!text.contains(&needle), "{} names the binding", p.display());
                }
            }
        }
        assert!(checked > 20, "only {checked} files scanned");
    }

    #[test]
    fn global_if_ready_never_opens_the_store() {
        // #1010: the UI thread asks this way, so it must not open anything.
        let slot: Slot = Mutex::new(None);
        assert!(ready_in(&slot).is_none(), "an unopened store is not ready");
        assert!(slot.lock().unwrap().is_none(), "asking must not open it");
    }

    #[test]
    fn global_if_ready_does_not_wait_for_an_open_in_progress() {
        let slot: Slot = Mutex::new(None);
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel::<()>();
        std::thread::scope(|s| {
            let slot_ref = &slot;
            s.spawn(move || {
                let _ = global_with(slot_ref, || {
                    started_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    Err(StoreError::Acl("slow".to_string()))
                });
            });
            started_rx.recv().unwrap();
            // The open holds the lock; the caller gets "not ready", not a wait.
            assert!(ready_in(&slot).is_none());
            go_tx.send(()).unwrap();
        });
        assert!(matches!(ready_in(&slot), Some(Err(StoreError::Acl(_)))));
    }

    #[test]
    fn a_failed_open_is_remembered_for_the_run() {
        let slot: Slot = Mutex::new(None);
        let mut calls = 0;
        for _ in 0..3 {
            let r = global_with(&slot, || {
                calls += 1;
                Err(StoreError::Acl("boom".to_string()))
            });
            assert!(matches!(r, Err(StoreError::Acl(_))));
        }
        assert_eq!(calls, 1, "a failed open must not be retried per call");
    }

    #[test]
    fn the_acl_step_runs_only_when_the_directory_is_created() {
        let dir = scratch("acl-once");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        let calls = std::cell::Cell::new(0);
        let count = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        drop(open_in(&path, &legacy, count).unwrap());
        assert_eq!(calls.get(), 1, "a new directory is restricted");
        drop(
            open_in(&path, &legacy, |_: &Path| {
                calls.set(calls.get() + 1);
                Ok(())
            })
            .unwrap(),
        );
        assert_eq!(calls.get(), 1, "an existing directory is not re-restricted");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_without_the_marker_is_restricted_on_open_and_kept() {
        // A folder left behind by a launch cut short (or made in portable
        // mode) has no marker, so the next open restricts it.
        let dir = scratch("acl-marker");
        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();
        let path = store_dir.join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        let calls = std::cell::Cell::new(0);
        let count = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(())
        };
        drop(open_in(&path, &legacy, count).unwrap());
        assert_eq!(calls.get(), 1, "an unmarked folder is restricted");
        assert!(store_dir.join(ACL_MARKER).exists());
        // A failure on a folder that was not made this run keeps the folder.
        std::fs::remove_file(store_dir.join(ACL_MARKER)).unwrap();
        let err = open_in(&path, &legacy, |_: &Path| {
            Err(StoreError::Acl("nope".to_string()))
        })
        .err()
        .expect("must fail");
        assert!(matches!(err, StoreError::Acl(_)));
        assert!(store_dir.exists(), "an existing folder is never removed");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn portable_mode_does_not_write_the_marker() {
        let dir = scratch("acl-marker-portable");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        let _forced = crate::portable::ForcedRoot::new(&dir);
        drop(open_in(&path, &legacy, |_: &Path| Ok(())).unwrap());
        assert!(!dir.join("store").join(ACL_MARKER).exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_acl_step_removes_the_directory_so_the_next_launch_retries() {
        let dir = scratch("acl-fail");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        let err = open_in(&path, &legacy, |_: &Path| {
            Err(StoreError::Acl("nope".to_string()))
        })
        .err()
        .expect("must fail");
        assert!(matches!(err, StoreError::Acl(_)));
        assert!(!dir.join("store").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "timing probe: cargo test open_default_timing -- --ignored --nocapture"]
    fn open_default_timing() {
        // The cost of the first open on the UI thread: a brand-new install
        // (directory, ACL step, migrations) and every later launch.
        let dir = scratch("timing");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        let t = std::time::Instant::now();
        drop(open_in(&path, &legacy, acl::restrict_dir).unwrap());
        println!("first-ever launch: {:?}", t.elapsed());
        let t = std::time::Instant::now();
        drop(open_in(&path, &legacy, acl::restrict_dir).unwrap());
        println!("later launch: {:?}", t.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_open_imports_a_waiting_egress_log_and_renames_it() {
        // The wiring check for the import: through the real open path, not
        // the import function alone.
        let dir = scratch("egress-import");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let row = crate::egress::build_entry(
            "legacy",
            "https://api.example.com/",
            &serde_json::json!({"model": "m"}),
            false,
            1,
            2,
            crate::egress::Outcome::Success,
            None,
        );
        std::fs::write(&egress, serde_json::to_string(&row).unwrap() + "\n").unwrap();
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };

        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();

        assert_eq!(s.egress_rows().unwrap().len(), 1);
        assert!(!egress.exists());
        assert!(dir.join("egress.log.migrated").exists());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_open_imports_a_waiting_profile_file_and_a_bad_one_does_not_stop_it() {
        // The wiring check for the profile import, through the real open path.
        let dir = scratch("profile-import");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let profile = dir.join("profile.bin");
        crate::profile::Profile {
            full_name: crate::profile::Field::new("Ada Lovelace".to_string(), false),
            ..Default::default()
        }
        .write_legacy_file(&profile)
        .unwrap();
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &profile,
        };

        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();

        assert_eq!(s.load_profile().unwrap().full_name.value, "Ada Lovelace");
        assert!(!profile.exists());
        assert!(dir.join("profile.bin.migrated").exists());
        drop(s);

        // A file nobody on this account can open: the store still opens.
        let path2 = dir.join("store2").join("wingman.db");
        std::fs::write(&profile, b"not a profile").unwrap();
        let s = open_in(&path2, &legacy, |_: &Path| Ok(()))
            .expect("a bad profile file must not fail the open");
        assert!(profile.exists(), "left where it is");
        assert!(profile_notice(&s, &legacy).is_some());
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_database_does_not_lose_the_profile() {
        let dir = scratch("profile-recover");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let profile = dir.join("profile.bin");
        crate::profile::Profile {
            full_name: crate::profile::Field::new("Ada Lovelace".to_string(), false),
            ..Default::default()
        }
        .write_legacy_file(&profile)
        .unwrap();
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &profile,
        };
        drop(open_in(&path, &legacy, |_: &Path| Ok(())).unwrap());
        assert!(dir.join("profile.bin.migrated").exists());
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();

        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();

        assert_eq!(s.load_profile().unwrap().full_name.value, "Ada Lovelace");
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn sample_row(provider: &str) -> crate::egress::EgressEntry {
        crate::egress::build_entry(
            provider,
            "https://api.example.com/",
            &serde_json::json!({"model": "m"}),
            false,
            1,
            2,
            crate::egress::Outcome::Success,
            None,
        )
    }

    #[test]
    fn rows_written_while_the_store_was_down_are_imported_by_the_next_open() {
        let dir = scratch("pending");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        egress::append_pending_at(&pending, &sample_row("down-1")).unwrap();
        egress::append_pending_at(&pending, &sample_row("down-2")).unwrap();
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };

        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();

        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["down-1", "down-2"]);
        assert!(!pending.exists(), "imported rows are not imported twice");
        drop(s);
        // A later run that finds the store down again appends again, and the
        // import runs again even though the old log was imported long ago.
        egress::append_pending_at(&pending, &sample_row("down-3")).unwrap();
        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();
        assert_eq!(s.egress_rows().unwrap().len(), 3);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_cut_off_by_a_crash_does_not_swallow_the_next_one() {
        let dir = scratch("pending-cut");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        std::fs::write(&pending, "{\"a\":1").unwrap();
        egress::append_pending_at(&pending, &sample_row("after-crash")).unwrap();
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };

        let s = open_in(&path, &legacy, |_: &Path| Ok(())).unwrap();

        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["after-crash"]);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_request_log_import_does_not_stop_the_store_opening() {
        let dir = scratch("import-fails");
        let path = dir.join("store").join("wingman.db");
        let (spend, egress) = (dir.join("spend.json"), dir.join("egress.log"));
        let pending = dir.join("egress-pending.log");
        let legacy = Legacy {
            spend: &spend,
            egress: &egress,
            pending: &pending,
            profile: &dir.join("profile.bin"),
        };
        drop(open_in(&path, &legacy, |_: &Path| Ok(())).unwrap());
        {
            // Make every egress insert fail, then leave rows waiting.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "DELETE FROM meta WHERE key = 'imported_egress';
                 CREATE TRIGGER boom BEFORE INSERT ON egress
                 BEGIN SELECT RAISE(ABORT, 'injected'); END;",
            )
            .unwrap();
        }
        std::fs::write(
            &egress,
            serde_json::to_string(&sample_row("old")).unwrap() + "\n",
        )
        .unwrap();
        egress::append_pending_at(&pending, &sample_row("waiting")).unwrap();

        let s = open_in(&path, &legacy, |_: &Path| Ok(()))
            .expect("an import failure must not fail the open");

        assert!(egress.exists() && pending.exists(), "both stay for a retry");
        let notices = request_log_notices(&s, &legacy);
        assert_eq!(
            notices.len(),
            2,
            "each failed import raises a notice: {notices:?}"
        );
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_default_open_creates_the_store_and_records_usage() {
        // The wiring check: the real entry point (ACL, recovery, import)
        // against the per-process test directory.
        let s = global().expect("the default store should open");
        s.record_usage("2026-10-15", "wiring", "m", Some(0.5), None)
            .unwrap();
        assert!(s.day_total("2026-10-15").unwrap().is_some());
        assert!(db_path().unwrap().exists());
    }
}
