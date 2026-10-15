//! One-time import of the old `spend.json` into `usage_day`.
//!
//! One source of truth after the move: the rows and the `imported_spend`
//! flag are written in one transaction, then the old file is renamed
//! `spend.json.migrated` (kept for one release as a hand rollback) and is
//! never read again.

use std::collections::BTreeMap;
use std::io::ErrorKind;
use std::path::Path;

use rusqlite::{Connection, OptionalExtension};

use super::StoreError;
use crate::usage::DayTotal;

const FLAG: &str = "imported_spend";

/// Imports `path` if it has not been imported before. Returns a plain-words
/// notice when the file exists but could not be used (it is then left in
/// place and the flag is not set, so a later build can retry).
pub fn spend_json(conn: &mut Connection, path: &Path) -> Result<Option<String>, StoreError> {
    let done: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [FLAG], |r| {
            r.get(0)
        })
        .optional()?;
    if done.is_some() {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Nothing to import, now or later.
            set_flag(conn)?;
            return Ok(None);
        }
        Err(_) => return Ok(Some(unreadable())),
    };
    let Ok(days) = serde_json::from_str::<BTreeMap<String, DayTotal>>(&text) else {
        return Ok(Some(unreadable()));
    };
    let tx = conn.transaction()?;
    for (date, d) in &days {
        tx.execute(
            "INSERT INTO usage_day (date, provider, model, requests, unpriced, usd)
             VALUES (?1, '', '', ?2, ?3, ?4)
             ON CONFLICT (date, provider, model) DO UPDATE SET
               requests = requests + excluded.requests,
               unpriced = unpriced + excluded.unpriced,
               usd = usd + excluded.usd",
            rusqlite::params![date, d.requests, d.unpriced, d.usd],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
        [FLAG],
    )?;
    tx.commit()?;
    // Harmless if it fails: the flag is set, so the file is not read again.
    let _ = std::fs::rename(path, path.with_extension("json.migrated"));
    Ok(None)
}

fn set_flag(conn: &Connection) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
        [FLAG],
    )?;
    Ok(())
}

fn unreadable() -> String {
    "Wingman could not read its old spend totals, so they were left where they are.".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::migrate::{self, MIGRATIONS};

    fn db() -> Connection {
        let mut conn = Connection::open_in_memory().unwrap();
        migrate::run(&mut conn, MIGRATIONS, None).unwrap();
        conn
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("wingman-test-import-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const FIXTURE: &str = r#"{"2026-10-14":{"usd":0.25,"requests":3,"unpriced":1},
                             "2026-10-15":{"usd":1.5,"requests":10,"unpriced":0}}"#;

    #[test]
    fn imports_every_day_once_and_renames_the_file() {
        let dir = scratch("ok");
        let path = dir.join("spend.json");
        std::fs::write(&path, FIXTURE).unwrap();
        let mut conn = db();

        assert_eq!(spend_json(&mut conn, &path).unwrap(), None);

        let (n, usd, req): (i64, f64, i64) = conn
            .query_row(
                "SELECT count(*), sum(usd), sum(requests) FROM usage_day",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!((n, req), (2, 13));
        assert!((usd - 1.75).abs() < 1e-9);
        assert!(!path.exists());
        assert!(dir.join("spend.json.migrated").exists());

        // A second run, even with a file back in place, imports nothing.
        std::fs::write(&path, FIXTURE).unwrap();
        assert_eq!(spend_json(&mut conn, &path).unwrap(), None);
        let n: i64 = conn
            .query_row("SELECT sum(requests) FROM usage_day", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 13);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_sets_the_flag_and_imports_nothing() {
        let dir = scratch("missing");
        let mut conn = db();
        assert_eq!(
            spend_json(&mut conn, &dir.join("spend.json")).unwrap(),
            None
        );
        let flag: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'imported_spend'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, "1");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_file_is_left_in_place_with_a_notice_and_no_flag() {
        let dir = scratch("bad");
        let path = dir.join("spend.json");
        std::fs::write(&path, "{ this is not json").unwrap();
        let mut conn = db();

        let notice = spend_json(&mut conn, &path).unwrap();

        assert!(notice.is_some());
        assert!(path.exists());
        let flags: i64 = conn
            .query_row("SELECT count(*) FROM meta", [], |r| r.get(0))
            .unwrap();
        assert_eq!(flags, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_notice_has_no_em_dash() {
        assert!(!unreadable().contains('\u{2014}'));
    }
}
