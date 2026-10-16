//! The `egress` table: one row per outbound request attempt, replacing the
//! old `egress.log`. Rows hold kinds, sizes and counts, never content (see
//! `crate::egress`).
//!
//! Retention (spec, decision 6): 90 days and at most 10 000 rows, whichever
//! is reached first. Pruning runs inside the insert, never on a timer, and
//! never removes the row just written.

use rusqlite::{params, Row};

use super::{Store, StoreError};
use crate::egress::{Attachment, EgressEntry, Outcome};

/// Rows older than this are pruned by the next insert.
pub const KEEP_SECS: u64 = 90 * 24 * 60 * 60;
/// Newest rows kept.
pub const KEEP_ROWS: usize = 10_000;

impl Store {
    /// Appends one request attempt, then prunes past the retention limits, in
    /// one transaction.
    pub fn record_egress(&self, entry: &EgressEntry) -> Result<(), StoreError> {
        self.record_egress_at(entry, now_unix(), KEEP_ROWS)
    }

    /// [`Store::record_egress`] with the clock and the row limit passed in,
    /// so retention can be tested without ten thousand rows.
    pub(super) fn record_egress_at(
        &self,
        entry: &EgressEntry,
        now: u64,
        keep_rows: usize,
    ) -> Result<(), StoreError> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        insert(&tx, entry)?;
        prune(&tx, now, keep_rows)?;
        tx.commit()?;
        Ok(())
    }

    /// Every kept row, oldest first.
    pub fn egress_rows(&self) -> Result<Vec<EgressEntry>, StoreError> {
        let conn = self.lock();
        let mut stmt = conn.prepare(
            "SELECT time_unix, provider, model, retried, trigger_label, attachments,
                    snippets, redactions, bytes_sent, bytes_received, outcome, error, cost
             FROM egress ORDER BY id",
        )?;
        let rows = stmt.query_map([], entry_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Test builds only: forgets every row.
    #[cfg(test)]
    pub fn clear_egress(&self) {
        self.lock().execute("DELETE FROM egress", []).unwrap();
    }
}

/// Inserts one row. Shared with the one-time import of the old log.
pub(super) fn insert(
    tx: &rusqlite::Transaction<'_>,
    entry: &EgressEntry,
) -> Result<(), StoreError> {
    let json = |e: serde_json::Error| StoreError::Other(e.to_string());
    let attachments = serde_json::to_string(&entry.attachments).map_err(json)?;
    let snippets = serde_json::to_string(&entry.snippets).map_err(json)?;
    let redactions = serde_json::to_string(&entry.redactions).map_err(json)?;
    let outcome = match entry.outcome {
        Outcome::Success => "success",
        Outcome::Failure => "failure",
    };
    tx.execute(
        "INSERT INTO egress
           (time_unix, provider, model, retried, trigger_label, attachments,
            snippets, redactions, bytes_sent, bytes_received, outcome, error, cost)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            i64::try_from(entry.time_unix).unwrap_or(i64::MAX),
            entry.provider,
            entry.model,
            i64::from(entry.retried),
            entry.trigger,
            attachments,
            snippets,
            redactions,
            i64::try_from(entry.bytes_sent).unwrap_or(i64::MAX),
            i64::try_from(entry.bytes_received).unwrap_or(i64::MAX),
            outcome,
            entry.error,
            entry.cost,
        ],
    )?;
    Ok(())
}

/// Drops rows past the retention limits. The row inserted last is exempt
/// from the age rule, so a bad clock cannot make the newest request the first
/// thing deleted.
pub(super) fn prune(
    tx: &rusqlite::Transaction<'_>,
    now: u64,
    keep_rows: usize,
) -> Result<(), StoreError> {
    let cutoff = i64::try_from(now.saturating_sub(KEEP_SECS)).unwrap_or(i64::MAX);
    tx.execute(
        "DELETE FROM egress WHERE time_unix < ?1 AND id != last_insert_rowid()",
        [cutoff],
    )?;
    tx.execute(
        "DELETE FROM egress WHERE id <=
           (SELECT id FROM egress ORDER BY id DESC LIMIT 1 OFFSET ?1)",
        [i64::try_from(keep_rows).unwrap_or(i64::MAX)],
    )?;
    Ok(())
}

pub(super) fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Reads one row back. JSON columns that do not parse come back empty: a
/// damaged detail must not hide the rest of the history.
fn entry_from_row(r: &Row<'_>) -> rusqlite::Result<EgressEntry> {
    let attachments: String = r.get(5)?;
    let snippets: String = r.get(6)?;
    let redactions: String = r.get(7)?;
    let outcome: String = r.get(10)?;
    Ok(EgressEntry {
        time_unix: u64::try_from(r.get::<_, i64>(0)?).unwrap_or(0),
        provider: r.get(1)?,
        model: r.get(2)?,
        retried: r.get::<_, i64>(3)? != 0,
        trigger: r.get(4)?,
        attachments: serde_json::from_str::<Vec<Attachment>>(&attachments).unwrap_or_default(),
        snippets: serde_json::from_str(&snippets).unwrap_or_default(),
        redactions: serde_json::from_str(&redactions).unwrap_or_default(),
        bytes_sent: usize::try_from(r.get::<_, i64>(8)?).unwrap_or(0),
        bytes_received: usize::try_from(r.get::<_, i64>(9)?).unwrap_or(0),
        outcome: if outcome == "success" {
            Outcome::Success
        } else {
            Outcome::Failure
        },
        error: r.get(11)?,
        cost: r.get(12)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::egress::{build_entry, AttachmentKind};
    use serde_json::json;

    fn entry(provider: &str, time_unix: u64) -> EgressEntry {
        let mut e = build_entry(
            provider,
            "https://api.example.com/",
            &json!({"model": "m", "messages": [{"role": "user", "content": "hello"}]}),
            false,
            10,
            20,
            Outcome::Success,
            None,
        );
        e.time_unix = time_unix;
        e
    }

    fn count(s: &Store) -> i64 {
        s.lock()
            .query_row("SELECT count(*) FROM egress", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn a_row_round_trips_every_field() {
        let s = Store::in_memory();
        let mut e = entry("anthropic", 1_000);
        e.model = Some("claude-opus-5".into());
        e.retried = true;
        e.attachments = vec![Attachment {
            kind: AttachmentKind::Image,
            size_bytes: 1234,
            width: Some(340),
            height: Some(200),
        }];
        e.snippets = vec!["resume.pdf#3".into()];
        e.outcome = Outcome::Failure;
        e.error = Some("HTTP 500".into());
        e.cost = Some(0.0123);
        e.trigger = Some("hotkey".into());
        e.redactions.insert("email".into(), 2);

        s.record_egress_at(&e, 1_000, KEEP_ROWS).unwrap();

        assert_eq!(s.egress_rows().unwrap(), vec![e]);
    }

    #[test]
    fn absent_optionals_come_back_absent_not_zero() {
        let s = Store::in_memory();
        let mut e = entry("p", 5);
        e.model = None;
        e.cost = None;
        e.trigger = None;
        s.record_egress_at(&e, 5, KEEP_ROWS).unwrap();
        let back = &s.egress_rows().unwrap()[0];
        assert_eq!(
            (back.model.clone(), back.cost, back.trigger.clone()),
            (None, None, None)
        );
    }

    #[test]
    fn rows_come_back_oldest_first() {
        let s = Store::in_memory();
        for (i, p) in ["a", "b", "c"].iter().enumerate() {
            s.record_egress_at(&entry(p, 100 + i as u64), 200, KEEP_ROWS)
                .unwrap();
        }
        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["a", "b", "c"]);
    }

    #[test]
    fn rows_past_the_row_limit_are_pruned_oldest_first() {
        let s = Store::in_memory();
        for i in 0..7u64 {
            s.record_egress_at(&entry(&format!("p{i}"), 1_000 + i), 2_000, 5)
                .unwrap();
        }
        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["p2", "p3", "p4", "p5", "p6"]);
    }

    #[test]
    fn rows_older_than_ninety_days_are_pruned_by_the_next_insert() {
        let s = Store::in_memory();
        let now = 10_000_000u64;
        s.record_egress_at(&entry("old", now - KEEP_SECS - 1), now, KEEP_ROWS)
            .unwrap();
        s.record_egress_at(&entry("edge", now - KEEP_SECS), now, KEEP_ROWS)
            .unwrap();
        s.record_egress_at(&entry("new", now), now, KEEP_ROWS)
            .unwrap();
        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["edge", "new"]);
    }

    #[test]
    fn the_row_just_written_is_never_pruned_even_with_a_bad_clock() {
        // time_unix 0 is what the entry builder falls back to when the clock
        // is unreadable; it must not be the first thing deleted.
        let s = Store::in_memory();
        s.record_egress_at(&entry("zero", 0), 10_000_000, KEEP_ROWS)
            .unwrap();
        assert_eq!(count(&s), 1);
    }

    #[test]
    fn a_damaged_json_column_does_not_hide_the_row() {
        let s = Store::in_memory();
        s.record_egress_at(&entry("p", 1), 1, KEEP_ROWS).unwrap();
        s.lock()
            .execute(
                "UPDATE egress SET attachments = 'nope', snippets = '{', redactions = '['",
                [],
            )
            .unwrap();
        let back = &s.egress_rows().unwrap()[0];
        assert!(back.attachments.is_empty() && back.snippets.is_empty());
        assert!(back.redactions.is_empty());
        assert_eq!(back.provider, "p");
    }

    #[test]
    fn two_threads_recording_both_land() {
        let s = std::sync::Arc::new(Store::in_memory());
        let handles: Vec<_> = (0..2)
            .map(|t| {
                let s = s.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        s.record_egress(&entry(&format!("t{t}-{i}"), now_unix()))
                            .unwrap();
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        assert_eq!(count(&s), 50);
    }

    #[test]
    fn a_locked_database_reports_failure_and_loses_nothing_already_saved() {
        let dir =
            std::env::temp_dir().join(format!("wingman-test-egress-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        s.record_egress(&entry("before", now_unix())).unwrap();
        // Fail fast instead of waiting out the 5 s production timeout.
        s.lock()
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();

        // Another connection holds the write lock.
        let other = rusqlite::Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();
        let err = s.record_egress(&entry("blocked", now_unix())).unwrap_err();
        assert!(matches!(err, StoreError::Other(_)), "{err:?}");
        other.execute_batch("ROLLBACK").unwrap();

        // The store still works and still has the earlier row.
        s.record_egress(&entry("after", now_unix())).unwrap();
        let got: Vec<String> = s
            .egress_rows()
            .unwrap()
            .into_iter()
            .map(|e| e.provider)
            .collect();
        assert_eq!(got, ["before", "after"]);
        drop(other);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
