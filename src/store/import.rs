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

use super::{egress, profile_field, StoreError};
use crate::egress::EgressEntry;
use crate::usage::DayTotal;

const FLAG: &str = "imported_spend";

/// Imports `path` if it has not been imported before. Returns a plain-words
/// notice when the file exists but could not be used (it is then left in
/// place and the flag is not set, so a later build can retry).
pub fn spend_json(conn: &mut Connection, path: &Path) -> Result<Option<String>, StoreError> {
    if flag_is_set(conn, FLAG)? {
        return Ok(None);
    }
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Nothing to import, now or later.
            set_flag_named(conn, FLAG)?;
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

const EGRESS_FLAG: &str = "imported_egress";

/// Imports the old `egress.log` (one JSON row per line) once. A line that
/// does not parse is counted and skipped; the count comes back in the notice.
/// A file that exists but cannot be read is left in place with the flag unset,
/// so a later build can retry.
pub fn egress_log(conn: &mut Connection, path: &Path) -> Result<Option<String>, StoreError> {
    if flag_is_set(conn, EGRESS_FLAG)? {
        return Ok(None);
    }
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => {
            // Nothing to import, now or later.
            set_flag_named(conn, EGRESS_FLAG)?;
            return Ok(None);
        }
        Err(_) => return Ok(Some(egress_unreadable())),
    };
    let skipped = import_rows(conn, &bytes, Some(EGRESS_FLAG))?;
    // Harmless if it fails: the flag is set, so the file is not read again.
    let _ = std::fs::rename(path, path.with_extension("log.migrated"));
    Ok((skipped > 0).then(|| bad_lines_notice(skipped)))
}

/// Imports rows that were written to `egress-pending.log` while the store
/// could not open (see `crate::egress`), then removes the file. Unlike the
/// old log this happens on every open that finds the file, with no flag: the
/// file can be written again by any later run in which the store is down.
pub fn egress_pending(conn: &mut Connection, path: &Path) -> Result<Option<String>, StoreError> {
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(None),
        Err(_) => return Ok(Some(egress_unreadable())),
    };
    let skipped = import_rows(conn, &bytes, None)?;
    // If this fails the rows would be imported twice next time; a duplicate
    // log row is the cheaper mistake than losing one.
    let _ = std::fs::remove_file(path);
    Ok((skipped > 0).then(|| bad_lines_notice(skipped)))
}

/// Writes every parsable row of `bytes` (one JSON row per line) and the
/// optional `meta` flag in one transaction, then applies the table's
/// retention by age and count with no exemption for the last row. Returns how
/// many lines were skipped.
fn import_rows(
    conn: &mut Connection,
    bytes: &[u8],
    flag: Option<&str>,
) -> Result<usize, StoreError> {
    // A stray byte must not cost the whole log, so decode leniently; the
    // damaged line then fails to parse and is counted like any other.
    let text = String::from_utf8_lossy(bytes);
    let mut rows = Vec::new();
    let mut skipped = 0usize;
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<EgressEntry>(line) {
            Ok(e) => rows.push(e),
            Err(_) => skipped += 1,
        }
    }
    let tx = conn.transaction()?;
    for e in &rows {
        egress::insert(&tx, e)?;
    }
    if let Some(flag) = flag {
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
            [flag],
        )?;
    }
    egress::prune(&tx, egress::now_unix(), egress::KEEP_ROWS, false)?;
    tx.commit()?;
    Ok(skipped)
}

const PROFILE_FLAG: &str = "imported_profile";

/// Imports the old `profile.bin` once. The file is read with the same loader
/// as before (so the payment denylist runs over it), each member is sealed
/// on its own into `profile_field`, and the rows and the flag are written in
/// one transaction. Only after the commit is the file renamed
/// `profile.bin.migrated`; it is never deleted.
///
/// A file that cannot be used (sealed for another Windows user or PC,
/// damaged, or failing the payment check) comes back as a notice with the
/// file left exactly where it is and the flag unset, so a later launch on the
/// right account can still import it. Rows already in the store win over the
/// file's, so a profile entered since is never overwritten.
pub fn profile_bin(conn: &mut Connection, path: &Path) -> Result<Option<String>, StoreError> {
    if flag_is_set(conn, PROFILE_FLAG)? {
        return Ok(None);
    }
    // A fresh database after a damaged one was moved aside has lost the
    // profile, and the old file was already renamed. Read that copy again.
    let migrated = path.with_extension("bin.migrated");
    let source = match std::fs::metadata(path) {
        Ok(_) => path.to_path_buf(),
        Err(e) if e.kind() == ErrorKind::NotFound => match std::fs::metadata(&migrated) {
            Ok(_) => migrated.clone(),
            Err(m) if m.kind() == ErrorKind::NotFound => {
                // Nothing to import. Only settle that for good when the
                // folder is there: a folder that is briefly unavailable
                // (a synced or removable drive) must not look like "no file".
                if path.parent().is_some_and(Path::is_dir) {
                    set_flag_named(conn, PROFILE_FLAG)?;
                }
                return Ok(None);
            }
            Err(_) => return Ok(Some(profile_unreadable())),
        },
        Err(_) => return Ok(Some(profile_unreadable())),
    };
    let Ok(profile) = crate::profile::Profile::load_from(&source) else {
        return Ok(Some(profile_unreadable()));
    };
    let rows = match profile_field::encrypt_rows(&profile) {
        Ok(r) => r,
        Err(_) => return Ok(Some(profile_unreadable())),
    };
    let tx = conn.transaction()?;
    profile_field::insert_rows(&tx, &rows, false)?;
    tx.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
        [PROFILE_FLAG],
    )?;
    tx.commit()?;
    // Harmless if it fails: the flag is set, so the file is not read again.
    if source == path {
        let _ = std::fs::rename(path, &migrated);
    }
    Ok(None)
}

fn profile_unreadable() -> String {
    "Wingman could not open your old profile file, so it was left where it is. A profile saved by another Windows user or on another PC cannot be opened here.".to_string()
}

fn flag_is_set(conn: &Connection, flag: &str) -> Result<bool, StoreError> {
    let done: Option<String> = conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [flag], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(done.is_some())
}

fn set_flag_named(conn: &Connection, flag: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, '1')",
        [flag],
    )?;
    Ok(())
}

fn bad_lines_notice(n: usize) -> String {
    format!("{n} lines of Wingman's old request log could not be read and were skipped.")
}

fn egress_unreadable() -> String {
    "Wingman could not read its old request log, so it was left where it is.".to_string()
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

    // -- egress.log ---------------------------------------------------------

    fn line(provider: &str, t: u64) -> String {
        serde_json::to_string(&crate::egress::EgressEntry {
            time_unix: egress::now_unix() + t,
            provider: provider.to_string(),
            model: Some("m".to_string()),
            retried: false,
            attachments: Vec::new(),
            snippets: Vec::new(),
            bytes_sent: 1,
            bytes_received: 2,
            outcome: crate::egress::Outcome::Success,
            error: None,
            cost: Some(0.5),
            trigger: None,
            redactions: Default::default(),
        })
        .unwrap()
    }

    fn egress_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM egress", [], |r| r.get(0))
            .unwrap()
    }

    fn egress_flag(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'imported_egress'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    #[test]
    fn egress_rows_are_imported_in_order_once_and_the_file_renamed() {
        let dir = scratch("eg-ok");
        let path = dir.join("egress.log");
        let text = format!("{}\r\n{}\n", line("a", 1), line("b", 2));
        std::fs::write(&path, text).unwrap();
        let mut conn = db();

        assert_eq!(egress_log(&mut conn, &path).unwrap(), None);

        assert_eq!(egress_count(&conn), 2);
        assert_eq!(egress_flag(&conn).as_deref(), Some("1"));
        let order: Vec<String> = conn
            .prepare("SELECT provider FROM egress ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(order, ["a", "b"]);
        assert!(!path.exists());
        assert!(dir.join("egress.log.migrated").exists());

        // A second run, even with a file back in place, imports nothing.
        std::fs::write(&path, line("c", 3)).unwrap();
        assert_eq!(egress_log(&mut conn, &path).unwrap(), None);
        assert_eq!(egress_count(&conn), 2);
        assert!(path.exists(), "an imported-once store never reads it again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn imported_rows_older_than_the_retention_window_are_dropped() {
        let dir = scratch("eg-old");
        let path = dir.join("egress.log");
        let mut row: serde_json::Value = serde_json::from_str(&line("ancient", 0)).unwrap();
        row["time_unix"] = 5.into();
        let ancient = row.to_string();
        std::fs::write(&path, format!("{ancient}\n{}\n", line("recent", 0))).unwrap();
        let mut conn = db();

        egress_log(&mut conn, &path).unwrap();

        let kept: Vec<String> = conn
            .prepare("SELECT provider FROM egress")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kept, ["recent"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_ancient_last_line_is_dropped_by_the_import_not_exempted() {
        let dir = scratch("eg-ancient-last");
        let path = dir.join("egress.log");
        let mut row: serde_json::Value = serde_json::from_str(&line("ancient", 0)).unwrap();
        row["time_unix"] = 5.into();
        std::fs::write(&path, format!("{}\n{}\n", line("recent", 0), row)).unwrap();
        let mut conn = db();

        egress_log(&mut conn, &path).unwrap();

        let kept: Vec<String> = conn
            .prepare("SELECT provider FROM egress")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(kept, ["recent"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_bad_egress_line_is_counted_and_skipped_not_fatal() {
        let dir = scratch("eg-bad");
        let path = dir.join("egress.log");
        std::fs::write(
            &path,
            format!("{}\nnot json at all\n\n{}\n", line("a", 1), line("b", 2)),
        )
        .unwrap();
        let mut conn = db();

        let notice = egress_log(&mut conn, &path).unwrap().expect("a notice");

        assert!(notice.contains('1'), "{notice}");
        assert!(!notice.contains('\u{2014}'), "{notice}");
        assert_eq!(egress_count(&conn), 2);
        assert_eq!(egress_flag(&conn).as_deref(), Some("1"));
        assert!(dir.join("egress.log.migrated").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_egress_file_sets_the_flag_and_imports_nothing() {
        let dir = scratch("eg-missing");
        let mut conn = db();
        assert_eq!(
            egress_log(&mut conn, &dir.join("egress.log")).unwrap(),
            None
        );
        assert_eq!(egress_flag(&conn).as_deref(), Some("1"));
        assert_eq!(egress_count(&conn), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_egress_path_is_left_in_place_with_a_notice_and_no_flag() {
        // A directory where the file should be: reading it fails with
        // something other than NotFound.
        let dir = scratch("eg-unreadable");
        let path = dir.join("egress.log");
        std::fs::create_dir(&path).unwrap();
        let mut conn = db();

        let notice = egress_log(&mut conn, &path).unwrap();

        assert!(notice.is_some());
        assert!(path.exists());
        assert_eq!(egress_flag(&conn), None);
        assert_eq!(egress_count(&conn), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_all_bad_egress_file_is_not_lost_silently() {
        let dir = scratch("eg-allbad");
        let path = dir.join("egress.log");
        std::fs::write(&path, "garbage\nmore garbage\n").unwrap();
        let mut conn = db();
        let notice = egress_log(&mut conn, &path).unwrap();
        assert!(notice.is_some());
        // Kept for a hand look, as `.migrated`, with the flag set.
        assert!(dir.join("egress.log.migrated").exists());
        assert_eq!(egress_flag(&conn).as_deref(), Some("1"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_before_commit_leaves_no_rows_and_no_flag() {
        let dir = scratch("eg-crash");
        let path = dir.join("egress.log");
        std::fs::write(&path, format!("{}\n{}\n", line("a", 1), line("b", 2))).unwrap();
        let mut conn = db();
        // The second insert fails after the first succeeded, inside the same
        // transaction.
        conn.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON egress WHEN NEW.provider = 'b'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();

        assert!(egress_log(&mut conn, &path).is_err());

        assert_eq!(egress_count(&conn), 0);
        assert_eq!(egress_flag(&conn), None);
        assert!(path.exists(), "the old file stays for a retry");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_egress_notices_have_no_em_dash() {
        assert!(!bad_lines_notice(3).contains('\u{2014}'));
        assert!(!egress_unreadable().contains('\u{2014}'));
    }

    // -- profile.bin ----------------------------------------------------------

    fn sample() -> crate::profile::Profile {
        profile_field::sample_profile()
    }

    fn profile_count(conn: &Connection) -> i64 {
        conn.query_row("SELECT count(*) FROM profile_field", [], |r| r.get(0))
            .unwrap()
    }

    fn profile_flag(conn: &Connection) -> Option<String> {
        conn.query_row(
            "SELECT value FROM meta WHERE key = 'imported_profile'",
            [],
            |r| r.get(0),
        )
        .optional()
        .unwrap()
    }

    fn write_legacy(dir: &std::path::Path) -> std::path::PathBuf {
        let path = dir.join("profile.bin");
        sample().write_legacy_file(&path).unwrap();
        path
    }

    /// The store the connection belongs to, for reading the profile back.
    fn store_over(conn: Connection) -> crate::store::Store {
        crate::store::Store::over(conn, None)
    }

    #[test]
    fn profile_bin_is_imported_once_and_renamed_after_the_commit() {
        let dir = scratch("pf-ok");
        let path = write_legacy(&dir);
        let mut conn = db();

        assert_eq!(profile_bin(&mut conn, &path).unwrap(), None);

        assert_eq!(profile_flag(&conn).as_deref(), Some("1"));
        assert!(!path.exists());
        assert!(dir.join("profile.bin.migrated").exists());
        let s = store_over(conn);
        assert_eq!(s.load_profile().unwrap(), sample());

        // A second run, even with a file back in place, imports nothing.
        let path = dir.join("profile.bin");
        let mut other = sample();
        other.full_name.value = "Someone Else".to_string();
        other.write_legacy_file(&path).unwrap();
        assert_eq!(profile_bin(&mut s.lock(), &path).unwrap(), None);
        assert_eq!(s.load_profile().unwrap().full_name.value, "Ada Lovelace");
        assert!(path.exists(), "an imported-once store never reads it again");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_profile_file_sets_the_flag_and_imports_nothing() {
        let dir = scratch("pf-missing");
        let mut conn = db();
        assert_eq!(
            profile_bin(&mut conn, &dir.join("profile.bin")).unwrap(),
            None
        );
        assert_eq!(profile_flag(&conn).as_deref(), Some("1"));
        assert_eq!(profile_count(&conn), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_profile_file_that_cannot_be_decrypted_is_kept_with_a_notice_and_no_flag() {
        // What another PC or Windows user looks like: DPAPI refuses it. Here
        // the envelope is sealed under entropy this build does not use.
        let dir = scratch("pf-foreign");
        let path = dir.join("profile.bin");
        let foreign = crate::dpapi::protect(b"{}".to_vec(), Some(b"someone else")).unwrap();
        std::fs::write(&path, &foreign).unwrap();
        let mut conn = db();

        let notice = profile_bin(&mut conn, &path).unwrap().expect("a notice");

        assert!(notice.contains("another"), "{notice}");
        assert!(!notice.contains('\u{2014}'), "{notice}");
        assert_eq!(std::fs::read(&path).unwrap(), foreign, "left untouched");
        assert!(!dir.join("profile.bin.migrated").exists());
        assert_eq!(profile_flag(&conn), None);
        assert_eq!(profile_count(&conn), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_fresh_database_re_imports_the_migrated_copy() {
        // Import, lose the database (moved aside as damaged), reopen: the
        // profile comes back from profile.bin.migrated.
        let dir = scratch("pf-recover");
        let path = write_legacy(&dir);
        let mut first = db();
        assert_eq!(profile_bin(&mut first, &path).unwrap(), None);
        drop(first);

        let mut fresh = db();
        assert_eq!(profile_bin(&mut fresh, &path).unwrap(), None);

        assert_eq!(profile_flag(&fresh).as_deref(), Some("1"));
        assert_eq!(store_over(fresh).load_profile().unwrap(), sample());
        assert!(dir.join("profile.bin.migrated").exists(), "kept");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_does_not_settle_the_import_for_good() {
        let dir = scratch("pf-nofolder");
        let path = dir.join("gone").join("profile.bin");
        let mut conn = db();
        assert_eq!(profile_bin(&mut conn, &path).unwrap(), None);
        assert_eq!(profile_flag(&conn), None, "retried on a later launch");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_garbage_profile_file_is_kept_with_a_notice() {
        let dir = scratch("pf-garbage");
        let path = dir.join("profile.bin");
        std::fs::write(&path, b"this is not a DPAPI envelope").unwrap();
        let mut conn = db();

        assert!(profile_bin(&mut conn, &path).unwrap().is_some());

        assert!(path.exists());
        assert_eq!(profile_flag(&conn), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_payment_shaped_profile_file_is_not_imported() {
        let dir = scratch("pf-payment");
        let path = dir.join("profile.bin");
        let mut p = sample();
        p.notes.value = "card: 4111 1111 1111 1111".to_string();
        let sealed = crate::dpapi::protect(
            serde_json::to_vec(&p).unwrap(),
            Some(crate::dpapi::PROFILE_ENTROPY),
        )
        .unwrap();
        std::fs::write(&path, sealed).unwrap();
        let mut conn = db();

        let notice = profile_bin(&mut conn, &path).unwrap().expect("a notice");

        assert!(!notice.contains("4111"));
        assert!(path.exists());
        assert_eq!(profile_count(&conn), 0);
        assert_eq!(profile_flag(&conn), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_unreadable_profile_path_is_left_in_place_with_a_notice_and_no_flag() {
        // A directory where the file should be.
        let dir = scratch("pf-dir");
        let path = dir.join("profile.bin");
        std::fs::create_dir(&path).unwrap();
        let mut conn = db();

        assert!(profile_bin(&mut conn, &path).unwrap().is_some());

        assert!(path.exists());
        assert_eq!(profile_flag(&conn), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failure_before_commit_leaves_no_profile_rows_no_flag_and_the_file() {
        let dir = scratch("pf-crash");
        let path = write_legacy(&dir);
        let mut conn = db();
        conn.execute_batch(
            "CREATE TRIGGER boom BEFORE INSERT ON profile_field WHEN NEW.key = 'notes'
             BEGIN SELECT RAISE(ABORT, 'injected'); END;",
        )
        .unwrap();

        assert!(profile_bin(&mut conn, &path).is_err());

        assert_eq!(profile_count(&conn), 0);
        assert_eq!(profile_flag(&conn), None);
        assert!(path.exists(), "the old file stays for a retry");
        assert!(!dir.join("profile.bin.migrated").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_already_saved_profile_wins_over_the_old_file() {
        let dir = scratch("pf-wins");
        let path = write_legacy(&dir);
        let s = store_over(db());
        let mut mine = sample();
        mine.full_name.value = "Entered Since".to_string();
        s.save_profile(&mine).unwrap();

        assert_eq!(profile_bin(&mut s.lock(), &path).unwrap(), None);

        assert_eq!(s.load_profile().unwrap().full_name.value, "Entered Since");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_profile_notice_has_no_em_dash() {
        assert!(!profile_unreadable().contains('\u{2014}'));
    }

    #[test]
    fn the_notice_has_no_em_dash() {
        assert!(!unreadable().contains('\u{2014}'));
    }
}
