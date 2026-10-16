//! Versioned, forward-only schema migrations.
//!
//! `PRAGMA user_version` holds the version. Each step runs in its own
//! transaction that also bumps the version, so a failing step rolls back and
//! leaves the database usable at the previous version. A shipped step is never
//! edited; a mistake is fixed by the next one.

use std::path::Path;

use rusqlite::Connection;

use super::StoreError;

pub struct Migration {
    pub version: i64,
    pub sql: &'static str,
}

/// Every step, in order. Versions are contiguous from 1 (a test checks it).
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        sql: include_str!("schema/v001.sql"),
    },
    Migration {
        version: 2,
        sql: include_str!("schema/v002.sql"),
    },
    Migration {
        version: 3,
        sql: include_str!("schema/v003.sql"),
    },
];

pub fn current_version(conn: &Connection) -> Result<i64, StoreError> {
    Ok(conn.query_row("PRAGMA user_version", [], |r| r.get(0))?)
}

/// Brings `conn` up to the last of `migrations`. `db_path` is the database's
/// file, used to take a snapshot before upgrading a non-empty database; `None`
/// (an in-memory database) skips it.
pub fn run(
    conn: &mut Connection,
    migrations: &[Migration],
    db_path: Option<&Path>,
) -> Result<(), StoreError> {
    let current = current_version(conn)?;
    let latest = migrations.last().map_or(0, |m| m.version);
    if current > latest {
        // Made by a newer build. Do not touch it.
        return Err(StoreError::Newer {
            found: current,
            known: latest,
        });
    }
    if current == latest {
        return Ok(());
    }
    if current > 0 {
        if let Some(path) = db_path {
            snapshot_before_upgrade(conn, path, current)?;
        }
    }
    for m in migrations.iter().filter(|m| m.version > current) {
        let step = |conn: &mut Connection| -> rusqlite::Result<()> {
            let tx = conn.transaction()?;
            tx.execute_batch(m.sql)?;
            tx.pragma_update(None, "user_version", m.version)?;
            tx.commit()
        };
        step(conn).map_err(|e| match StoreError::from(e) {
            StoreError::Other(msg) => StoreError::Migration(m.version, msg),
            other => other,
        })?;
    }
    Ok(())
}

/// `VACUUM INTO <db>.pre-v<N>`, keeping only the newest such copy. A bad
/// migration is then recoverable by hand. The new copy is written under a
/// temporary name and swapped in only once it is complete, so a failed
/// snapshot never costs the previous one.
fn snapshot_before_upgrade(
    conn: &Connection,
    db_path: &Path,
    version: i64,
) -> Result<(), StoreError> {
    let name = db_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let prefix = format!("{name}.pre-v");
    let tmp = db_path.with_file_name(format!("{prefix}{version}.tmp"));
    let target = db_path.with_file_name(format!("{prefix}{version}"));
    let _ = std::fs::remove_file(&tmp);
    conn.execute("VACUUM INTO ?1", [tmp.to_string_lossy().as_ref()])
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            StoreError::Other(format!("could not back up the data before upgrading: {e}"))
        })?;
    if let Some(dir) = db_path.parent() {
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                if e.file_name().to_string_lossy().starts_with(&prefix) && e.path() != tmp {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
    std::fs::rename(&tmp, &target)
        .map_err(|e| StoreError::Other(format!("could not keep the backup: {e}")))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn real_versions_are_contiguous_from_one() {
        assert!(!MIGRATIONS.is_empty());
        for (i, m) in MIGRATIONS.iter().enumerate() {
            assert_eq!(m.version, i as i64 + 1);
        }
    }

    const V1: Migration = Migration {
        version: 1,
        sql: "CREATE TABLE t (a TEXT, b INTEGER);",
    };
    const V2: Migration = Migration {
        version: 2,
        sql: "ALTER TABLE t ADD COLUMN c TEXT DEFAULT 'new';",
    };

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-migrate-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_database_at_n_opens_at_n_plus_one_with_data_intact() {
        let dir = scratch("n1");
        let path = dir.join("t.db");
        let mut conn = Connection::open(&path).unwrap();
        run(&mut conn, &[V1], Some(&path)).unwrap();
        conn.execute("INSERT INTO t VALUES ('keep me', 7)", [])
            .unwrap();

        run(&mut conn, &[V1, V2], Some(&path)).unwrap();

        assert_eq!(current_version(&conn).unwrap(), 2);
        let (a, b, c): (String, i64, String) = conn
            .query_row("SELECT a, b, c FROM t", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!((a.as_str(), b, c.as_str()), ("keep me", 7, "new"));
        // The pre-upgrade snapshot holds the version 1 data.
        let snap = Connection::open(dir.join("t.db.pre-v1")).unwrap();
        let n: i64 = snap
            .query_row("SELECT count(*) FROM t", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 1);
        drop(snap);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_real_v1_database_upgrades_to_v2_with_its_rows_intact() {
        // One test per shipped step (spec): a database at version 1 with
        // real rows opens at version 2 and keeps every one.
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &MIGRATIONS[..1], None).unwrap();
        conn.execute(
            "INSERT INTO usage_day (date, provider, model, requests, usd)
             VALUES ('2026-10-15', 'anthropic', 'm', 4, 1.5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES ('imported_spend', '1')",
            [],
        )
        .unwrap();

        run(&mut conn, &MIGRATIONS[..2], None).unwrap();

        assert_eq!(current_version(&conn).unwrap(), 2);
        let (requests, usd): (i64, f64) = conn
            .query_row("SELECT requests, usd FROM usage_day", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((requests, usd), (4, 1.5));
        let flag: String = conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'imported_spend'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(flag, "1");
        // The new table is there and empty.
        let n: i64 = conn
            .query_row("SELECT count(*) FROM egress", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn the_real_v2_database_upgrades_to_v3_with_its_rows_intact() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &MIGRATIONS[..2], None).unwrap();
        conn.execute(
            "INSERT INTO egress (time_unix, provider, outcome) VALUES (5, 'anthropic', 'success')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO usage_day (date, provider, model, requests, usd)
             VALUES ('2026-10-15', 'anthropic', 'm', 4, 1.5)",
            [],
        )
        .unwrap();

        run(&mut conn, MIGRATIONS, None).unwrap();

        assert_eq!(current_version(&conn).unwrap(), 3);
        let (egress, usage): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT count(*) FROM egress), (SELECT count(*) FROM usage_day)",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((egress, usage), (1, 1));
        let profile: i64 = conn
            .query_row("SELECT count(*) FROM profile_field", [], |r| r.get(0))
            .unwrap();
        assert_eq!(profile, 0, "the new table is there and empty");
    }

    #[test]
    fn a_failed_snapshot_keeps_the_previous_one() {
        let dir = scratch("snap");
        let path = dir.join("t.db");
        let mut conn = Connection::open(&path).unwrap();
        run(&mut conn, &[V1], Some(&path)).unwrap();
        // An older snapshot from an earlier upgrade.
        std::fs::write(dir.join("t.db.pre-v0"), b"older").unwrap();
        // Block the new snapshot: its temporary name is a directory, so
        // VACUUM INTO cannot write there and removing it as a file fails.
        std::fs::create_dir(dir.join("t.db.pre-v1.tmp")).unwrap();

        let err = run(&mut conn, &[V1, V2], Some(&path)).unwrap_err();

        assert!(matches!(err, StoreError::Other(_)), "{err:?}");
        assert_eq!(std::fs::read(dir.join("t.db.pre-v0")).unwrap(), b"older");
        assert_eq!(current_version(&conn).unwrap(), 1);
        drop(conn);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failing_step_rolls_back_and_leaves_the_previous_version() {
        let bad = Migration {
            version: 2,
            sql: "ALTER TABLE t ADD COLUMN c TEXT; THIS IS NOT SQL;",
        };
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &[V1], None).unwrap();
        let err = run(&mut conn, &[V1, bad], None).unwrap_err();
        assert!(matches!(err, StoreError::Migration(2, _)), "{err:?}");
        assert_eq!(current_version(&conn).unwrap(), 1);
        // The half-applied ALTER was rolled back too.
        assert!(conn.prepare("SELECT c FROM t").is_err());
    }

    #[test]
    fn a_newer_database_is_refused_and_left_alone() {
        let mut conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        let err = run(&mut conn, &[V1], None).unwrap_err();
        assert!(
            matches!(
                err,
                StoreError::Newer {
                    found: 99,
                    known: 1
                }
            ),
            "{err:?}"
        );
        assert_eq!(current_version(&conn).unwrap(), 99);
    }

    #[test]
    fn running_twice_is_a_no_op() {
        let mut conn = Connection::open_in_memory().unwrap();
        run(&mut conn, &[V1, V2], None).unwrap();
        run(&mut conn, &[V1, V2], None).unwrap();
        assert_eq!(current_version(&conn).unwrap(), 2);
    }
}
