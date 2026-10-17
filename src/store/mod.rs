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
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
                return StoreError::Corrupt(e.to_string());
            }
        }
        StoreError::Other(e.to_string())
    }
}

pub struct Store {
    conn: Mutex<Connection>,
}

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
        Ok(Store {
            conn: Mutex::new(conn),
        })
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
        self.conn.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(test)]
    pub(crate) fn in_memory() -> Store {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate::run(&mut conn, migrate::MIGRATIONS, None).unwrap();
        Store {
            conn: Mutex::new(conn),
        }
    }
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
    let stamp = now.unwrap_or(0);
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let mut main_target = path.to_path_buf();
    for suffix in ["", "-wal", "-shm"] {
        let from = path.with_file_name(format!("{name}{suffix}"));
        let to = path.with_file_name(format!("{name}{suffix}.corrupt-{stamp}"));
        if suffix.is_empty() {
            main_target = to.clone();
        }
        if from.exists() {
            std::fs::rename(&from, &to).map_err(|e| {
                StoreError::Other(format!("could not move the damaged data aside: {e}"))
            })?;
        }
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
/// open at start-up.
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
            .filter(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
            .collect();
        assert_eq!(moved.len(), 1);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
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
