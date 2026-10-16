//! The `profile_field` table: the "about me" profile, one row per `Profile`
//! member, each value in its own DPAPI envelope (spec, decision 1). It
//! replaces the whole-file `profile.bin`.
//!
//! Decision: the key is the `Profile` member name (`full_name`, `emails`,
//! `address`, ...), not a `FieldKind`. `FieldKind` is the label matcher's
//! vocabulary and has no entry for `notes` or the sensitivity flag; the
//! member is the unit that is read and written.
//!
//! The payment denylist runs here, on every write and every read, so no
//! caller can skip it. A value that cannot be decrypted (another Windows
//! user, another PC, a damaged row) is an error with a plain message and the
//! rows are left exactly as they are: nothing here deletes data on a failed
//! read.

use rusqlite::{params, Transaction};
use serde_json::{Map, Value};

use super::{Store, StoreError};
use crate::dpapi;
use crate::profile::Profile;

/// Every `Profile` member, in a fixed order. A test checks this against the
/// type, so a member added to `Profile` cannot be left out of the table.
pub(super) const KEYS: [&str; 10] = [
    "full_name",
    "preferred_name",
    "emails",
    "phones",
    "address",
    "organisation",
    "job_title",
    "date_of_birth",
    "website",
    "notes",
];

type Rows = Vec<(&'static str, Vec<u8>)>;

/// Turns a database error into a profile error; a damaged file keeps its own
/// kind so the open-time recovery still applies.
fn db(e: rusqlite::Error) -> StoreError {
    match StoreError::from(e) {
        StoreError::Other(why) => StoreError::Profile(why),
        other => other,
    }
}

/// Normalises, runs the payment denylist and encrypts each member. Nothing is
/// written by this function; both [`Store::save_profile`] and the one-time
/// import of `profile.bin` use it, so both are checked the same way.
pub(super) fn encrypt_rows(profile: &Profile) -> Result<Rows, StoreError> {
    let mut checked = profile.clone();
    checked.normalize_sensitivity();
    checked
        .validate()
        .map_err(|e| StoreError::Refused(format!("{e:#}")))?;
    let Value::Object(mut members) = serde_json::to_value(&checked)
        .map_err(|e| StoreError::Profile(format!("could not prepare the profile: {e}")))?
    else {
        return Err(StoreError::Profile(
            "the profile did not serialise to an object".to_string(),
        ));
    };
    let mut rows = Vec::with_capacity(KEYS.len());
    for key in KEYS {
        let member = members.remove(key).unwrap_or(Value::Null);
        let plain = serde_json::to_vec(&member)
            .map_err(|e| StoreError::Profile(format!("could not prepare the profile: {e}")))?;
        let sealed = dpapi::protect(plain, Some(dpapi::PROFILE_FIELD_ENTROPY))
            .map_err(|e| StoreError::Profile(format!("could not encrypt it: {e:#}")))?;
        rows.push((key, sealed));
    }
    Ok(rows)
}

/// Writes `rows` inside `tx`. With `replace` an existing row is overwritten;
/// without it an existing row wins and the new one is dropped.
pub(super) fn insert_rows(
    tx: &Transaction<'_>,
    rows: &Rows,
    replace: bool,
) -> Result<(), StoreError> {
    let sql = if replace {
        "INSERT OR REPLACE INTO profile_field (key, value, updated_unix) VALUES (?1, ?2, ?3)"
    } else {
        "INSERT OR IGNORE INTO profile_field (key, value, updated_unix) VALUES (?1, ?2, ?3)"
    };
    let now = i64::try_from(super::egress::now_unix()).unwrap_or(i64::MAX);
    for (key, sealed) in rows {
        tx.execute(sql, params![key, sealed, now]).map_err(db)?;
    }
    Ok(())
}

impl Store {
    /// The saved profile, or `Profile::default()` when nothing was saved. An
    /// undecryptable or invalid row is an error and changes nothing.
    pub fn load_profile(&self) -> Result<Profile, StoreError> {
        let stored: Vec<(String, Vec<u8>)> = {
            let conn = self.lock();
            let mut stmt = conn
                .prepare("SELECT key, value FROM profile_field")
                .map_err(db)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
                .map_err(db)?;
            rows.collect::<Result<_, _>>().map_err(db)?
        };
        // Decrypted with the lock released: DPAPI is a system call.
        let mut members = Map::new();
        for (key, sealed) in stored {
            if !KEYS.contains(&key.as_str()) {
                continue;
            }
            let plain = dpapi::unprotect(&sealed, Some(dpapi::PROFILE_FIELD_ENTROPY))
                .map_err(|e| StoreError::Undecryptable(format!("{e:#}")))?;
            let member: Value = serde_json::from_slice(&plain)
                .map_err(|e| StoreError::Profile(format!("a saved value is damaged: {e}")))?;
            members.insert(key, member);
        }
        if members.is_empty() {
            return Ok(Profile::default());
        }
        let mut profile: Profile = serde_json::from_value(Value::Object(members))
            .map_err(|e| StoreError::Profile(format!("a saved value is damaged: {e}")))?;
        profile.normalize_sensitivity();
        profile.validate().map_err(|e| {
            StoreError::Profile(format!(
                "a saved value failed the payment data check: {e:#}"
            ))
        })?;
        Ok(profile)
    }

    /// Replaces the saved profile. A payment-shaped value is refused before
    /// anything is written.
    pub fn save_profile(&self, profile: &Profile) -> Result<(), StoreError> {
        let rows = encrypt_rows(profile)?;
        let mut conn = self.lock();
        let tx = conn.transaction().map_err(db)?;
        tx.execute("DELETE FROM profile_field", []).map_err(db)?;
        insert_rows(&tx, &rows, true)?;
        tx.commit().map_err(db)?;
        Ok(())
    }
}

#[cfg(test)]
impl Store {
    /// Test builds only: replaces one stored value with raw bytes.
    pub(crate) fn corrupt_profile_row_for_test(
        &self,
        key: &str,
        bytes: &[u8],
    ) -> rusqlite::Result<usize> {
        self.lock().execute(
            "UPDATE profile_field SET value = ?2 WHERE key = ?1",
            params![key, bytes],
        )
    }

    /// Test builds only: one stored value, exactly as it is on disk.
    pub(crate) fn stored_profile_value_for_test(&self, key: &str) -> rusqlite::Result<Vec<u8>> {
        self.lock().query_row(
            "SELECT value FROM profile_field WHERE key = ?1",
            [key],
            |r| r.get(0),
        )
    }
}

/// A filled-in profile for tests in this module and in `import`.
#[cfg(test)]
pub(super) fn sample_profile() -> Profile {
    use crate::profile::{Address, Field};
    Profile {
        full_name: Field::new("Ada Lovelace".to_string(), false),
        preferred_name: Field::new("Ada".to_string(), false),
        emails: vec![
            Field::new("ada@example.com".to_string(), false),
            Field::new("ada.private@example.com".to_string(), true),
        ],
        phones: vec![Field::new("+1 555-123-4567".to_string(), false)],
        address: Field::new(
            Address {
                lines: vec!["221B Baker Street".to_string()],
                city: "London".to_string(),
                region: "Greater London".to_string(),
                postcode: "NW1 6XE".to_string(),
                country: "United Kingdom".to_string(),
            },
            false,
        ),
        organisation: Field::new("Analytical Engines Ltd".to_string(), false),
        job_title: Field::new("Mathematician".to_string(), false),
        date_of_birth: Field::new("1815-12-10".to_string(), true),
        website: Field::new("https://example.com".to_string(), false),
        notes: Field::new("Prefers async standups.".to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Profile {
        sample_profile()
    }

    fn count(s: &Store) -> i64 {
        s.lock()
            .query_row("SELECT count(*) FROM profile_field", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn the_key_list_covers_every_profile_member() {
        let Value::Object(members) = serde_json::to_value(Profile::default()).unwrap() else {
            panic!("a profile is an object");
        };
        let mut want: Vec<&str> = members.keys().map(String::as_str).collect();
        let mut have: Vec<&str> = KEYS.to_vec();
        want.sort_unstable();
        have.sort_unstable();
        assert!(!want.is_empty());
        assert_eq!(have, want);
    }

    #[test]
    fn an_empty_store_loads_the_default_profile() {
        let s = Store::in_memory();
        assert_eq!(s.load_profile().unwrap(), Profile::default());
        assert_eq!(count(&s), 0, "loading never creates rows");
    }

    #[test]
    fn a_full_profile_round_trips() {
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        assert_eq!(s.load_profile().unwrap(), sample());
        assert_eq!(count(&s), KEYS.len() as i64);
    }

    #[test]
    fn a_second_save_replaces_the_first() {
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        let mut next = sample();
        next.full_name.value = "Grace Hopper".to_string();
        next.emails.clear();
        s.save_profile(&next).unwrap();
        let got = s.load_profile().unwrap();
        assert_eq!(got.full_name.value, "Grace Hopper");
        assert!(got.emails.is_empty());
        assert_eq!(count(&s), KEYS.len() as i64);
    }

    #[test]
    fn date_of_birth_is_sensitive_even_if_saved_as_false() {
        let s = Store::in_memory();
        let mut p = sample();
        p.date_of_birth.sensitive = false;
        s.save_profile(&p).unwrap();
        assert!(s.load_profile().unwrap().date_of_birth.sensitive);
    }

    #[test]
    fn nothing_readable_is_stored() {
        let s = Store::in_memory();
        let mut p = sample();
        p.full_name.value = "a very distinctive plaintext marker".to_string();
        s.save_profile(&p).unwrap();
        let all: Vec<u8> = s
            .lock()
            .prepare("SELECT key, value FROM profile_field")
            .unwrap()
            .query_map([], |r| {
                let mut v: Vec<u8> = r.get::<_, String>(0)?.into_bytes();
                v.extend(r.get::<_, Vec<u8>>(1)?);
                Ok(v)
            })
            .unwrap()
            .flat_map(Result::unwrap)
            .collect();
        let text = String::from_utf8_lossy(&all);
        assert!(!text.contains("distinctive plaintext marker"));
        assert!(!text.contains("Lovelace"));
        assert!(!text.contains("\"value\""), "no JSON field names either");
    }

    #[test]
    fn a_payment_shaped_value_is_refused_and_nothing_is_written() {
        for bad in [
            "card: 4111 1111 1111 1111",
            "CVV: 123",
            "Account Number: 12345678",
        ] {
            let s = Store::in_memory();
            let mut p = sample();
            p.notes.value = bad.to_string();
            let err = s.save_profile(&p).unwrap_err();
            assert!(matches!(err, StoreError::Refused(_)), "{err:?}");
            let text = err.to_string();
            assert!(text.contains("notes"), "{text}");
            assert!(!text.contains("4111"), "never echo the value: {text}");
            assert_eq!(count(&s), 0);
        }
        let s = Store::in_memory();
        let mut p = sample();
        p.address.value.lines = vec!["GB29 NWBK 6016 1331 9268 19".to_string()];
        assert!(s.save_profile(&p).is_err());
        assert_eq!(count(&s), 0);
    }

    #[test]
    fn a_refused_save_keeps_the_previous_profile() {
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        let mut p = sample();
        p.notes.value = "card: 4111 1111 1111 1111".to_string();
        assert!(s.save_profile(&p).is_err());
        assert_eq!(s.load_profile().unwrap(), sample());
    }

    #[test]
    fn a_payment_shaped_row_already_in_the_table_is_refused_on_load() {
        let s = Store::in_memory();
        let mut p = sample();
        p.notes.value = "card: 4111 1111 1111 1111".to_string();
        // Seal it by hand, past the write check.
        let plain = serde_json::to_vec(&p.notes).unwrap();
        let sealed = dpapi::protect(plain, Some(dpapi::PROFILE_FIELD_ENTROPY)).unwrap();
        s.lock()
            .execute(
                "INSERT INTO profile_field (key, value, updated_unix) VALUES ('notes', ?1, 0)",
                [sealed],
            )
            .unwrap();
        let err = s.load_profile().unwrap_err();
        assert!(matches!(err, StoreError::Profile(_)), "{err:?}");
        assert!(!err.to_string().contains("4111"));
    }

    #[test]
    fn an_undecryptable_row_is_a_plain_error_and_the_rows_stay() {
        // What another Windows user or another PC looks like: the envelope is
        // well formed but DPAPI refuses it. Here: sealed under other entropy.
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        let foreign = dpapi::protect(b"{}".to_vec(), Some(b"someone else")).unwrap();
        s.lock()
            .execute(
                "UPDATE profile_field SET value = ?1 WHERE key = 'full_name'",
                [foreign.clone()],
            )
            .unwrap();

        let err = s.load_profile().unwrap_err();

        assert!(matches!(err, StoreError::Undecryptable(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("another"), "{text}");
        assert!(!text.contains('\u{2014}'), "{text}");
        assert_eq!(count(&s), KEYS.len() as i64, "nothing is deleted");
        let kept: Vec<u8> = s
            .lock()
            .query_row(
                "SELECT value FROM profile_field WHERE key = 'full_name'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kept, foreign);
    }

    #[test]
    fn a_garbage_row_fails_cleanly() {
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        s.lock()
            .execute(
                "UPDATE profile_field SET value = x'0102' WHERE key = 'notes'",
                [],
            )
            .unwrap();
        assert!(matches!(
            s.load_profile().unwrap_err(),
            StoreError::Undecryptable(_)
        ));
    }

    #[test]
    fn a_row_for_a_member_this_build_does_not_know_is_ignored() {
        let s = Store::in_memory();
        s.save_profile(&sample()).unwrap();
        s.lock()
            .execute(
                "INSERT INTO profile_field (key, value, updated_unix) VALUES ('future', x'00', 0)",
                [],
            )
            .unwrap();
        assert_eq!(s.load_profile().unwrap(), sample());
    }

    #[test]
    fn a_locked_store_returns_an_error_and_does_not_hang() {
        // A second connection holds the write lock past the timeout. The
        // save must come back as an error, not block or panic.
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-profile-locked-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("wingman.db");
        let s = Store::open_at(&path).unwrap();
        s.lock()
            .busy_timeout(std::time::Duration::from_millis(50))
            .unwrap();
        let other = rusqlite::Connection::open(&path).unwrap();
        other.execute_batch("BEGIN IMMEDIATE").unwrap();

        let err = s.save_profile(&sample()).unwrap_err();

        assert!(matches!(err, StoreError::Profile(_)), "{err:?}");
        assert!(err.to_string().starts_with("Wingman"));
        other.execute_batch("ROLLBACK").unwrap();
        drop(other);
        drop(s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_database_file_reports_as_corrupt_not_a_panic() {
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-profile-corrupt-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("wingman.db");
        std::fs::write(&path, vec![b'x'; 4096]).unwrap();
        assert!(matches!(
            Store::open_at(&path).err().unwrap(),
            StoreError::Corrupt(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
