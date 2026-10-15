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
mod import;
mod migrate;
mod usage_day;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode};

#[derive(Debug)]
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
    fn in_memory() -> Store {
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
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
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
    Ok(main_target)
}

// -- the process-wide store ------------------------------------------------

static GLOBAL: Mutex<Option<Arc<Store>>> = Mutex::new(None);
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

/// The real open: owner-only directory, recovery, migrations, imports.
fn open_default() -> Result<Store, StoreError> {
    let path = db_path()?;
    let dir = path
        .parent()
        .ok_or_else(|| StoreError::Other("the store path has no folder".to_string()))?;
    std::fs::create_dir_all(dir).map_err(|e| StoreError::Other(e.to_string()))?;
    acl::restrict_dir(dir)?;
    let (store, recovered) = Store::open_recovering(&path)?;
    if let Some(n) = recovered {
        note(n);
    }
    let legacy = legacy_spend_path()?;
    if let Some(n) = import::spend_json(&mut store.lock(), &legacy)? {
        note(n);
    }
    Ok(store)
}

/// The shared store, opened on first use. A failed open is reported once and
/// retried on the next call.
pub fn global() -> Result<Arc<Store>, StoreError> {
    let mut slot = GLOBAL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(s) = slot.as_ref() {
        return Ok(s.clone());
    }
    match open_default() {
        Ok(s) => {
            let s = Arc::new(s);
            *slot = Some(s.clone());
            Ok(s)
        }
        Err(e) => {
            report(&e);
            Err(e)
        }
    }
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
        for table in ["meta", "usage_day"] {
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
