//! The "about me" profile store (issue #36, expansion plan §4's
//! `profile.rs` row and §6's "Fill this form uses the profile"). Local
//! only, DPAPI-encrypted at rest via [`crate::dpapi`], and by construction
//! carries no card, bank or password field -- [`Profile`]'s fields are the
//! ones the expansion plan names ("about me": names, emails, phones,
//! addresses, company, preferences) and nothing else, which is what issue
//! #36's acceptance criterion ("the profile type has no payment fields")
//! means in a typed language: there is no field to smuggle one into.
//!
//! On top of that structural guarantee, [`denylist::check_field`] is a
//! second, independent line of defense against a payment-shaped value
//! landing in a *free-form* field (`notes`, or any address line) by
//! accident or by a user pasting the wrong thing -- enforced on every
//! [`Profile::save_to`] (so it is never written) and every
//! [`Profile::load_from`] (so a hand-edited or otherwise tampered file can
//! never hand payment-shaped text back into memory either).
//!
//! # Sensitivity and the OWNER_TODO decision
//!
//! Every field carries a `sensitive: bool` via [`Field`]. `date_of_birth`
//! is always sensitive -- [`Profile::normalize_sensitivity`] forces it,
//! on every load and every save, regardless of what was stored or passed
//! in. Nothing else defaults to sensitive. This is deliberately *not* the
//! same thing as deciding whether "Fill this form" (issue #40) may use a
//! non-sensitive field without a per-field tick in the preview -- that
//! product decision is still owed (expansion plan §15 item 2,
//! `OWNER_TODO.md` item 3) and this module does not make it. It only makes
//! sure the data model can express either answer: a future `fill_form`
//! proposal builder can read `field.sensitive` and require a tick when
//! true, whichever way the owner ultimately decides the untouched cases
//! should default.
//!
//! # Storage
//!
//! `%APPDATA%\Wingman\profile.bin`: JSON (this module's own
//! [`Profile`] shape) inside a [`crate::dpapi`] envelope, keyed with
//! [`dpapi::PROFILE_ENTROPY`]. Written atomically (temp file + rename),
//! the same pattern `config.rs`'s `Config::save_to` uses. The path is
//! always a parameter to the `_from`/`_to` functions actually doing file
//! I/O (rule 9): only [`Profile::load`]/[`Profile::save`] hard-code the
//! real `%APPDATA%` path, so tests exercise everything else against a
//! scratch directory.

// Issue #36's scope is the data model, the store and the denylist, not
// wiring a profile settings page (#50) or the "Fill this form" action
// (#40) that will read from it -- both are later issues. Until one of
// them exists in `app.rs`'s own reachability graph, this binary crate's
// dead-code analysis would otherwise flag the whole public surface below
// as unused, the same reason `ocr.rs` and `dpapi.rs` carry this.
#![allow(dead_code)]

mod denylist;
mod labels;

// Re-exported for #40's future use (`profile::match_label`,
// `profile::FieldKind`); unused from outside this module's own tests until
// that action exists, same reasoning as the file-level allow above.
#[allow(unused_imports)]
pub use labels::{match_label, FieldKind};

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::dpapi;

/// A profile value paired with whether it should be treated as sensitive
/// (see the module docs' "Sensitivity" section). Generic so it wraps both
/// a plain `String` (most fields) and a structured value ([`Address`]).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Field<T> {
    pub value: T,
    pub sensitive: bool,
}

impl<T> Field<T> {
    pub fn new(value: T, sensitive: bool) -> Self {
        Self { value, sensitive }
    }
}

/// The structured parts of a postal address. One [`Field`] wraps the whole
/// thing (one sensitivity flag for the address as a unit, matching how the
/// expansion plan lists "address lines/city/region/postcode/country" as a
/// single "about me" item), not one flag per sub-part.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Address {
    /// Free-form street address lines (e.g. `["221B Baker Street"]`), in
    /// display order. A `Vec` rather than a fixed line count since real
    /// addresses run from zero lines (nothing filled in yet) to several.
    pub lines: Vec<String>,
    pub city: String,
    pub region: String,
    pub postcode: String,
    pub country: String,
}

/// The "about me" profile. See the module docs for storage, sensitivity
/// and the payment-data denylist. `#[serde(default)]` on every level means
/// an older on-disk profile missing a field this build added gets that
/// field's default rather than failing to parse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Profile {
    pub full_name: Field<String>,
    pub preferred_name: Field<String>,
    /// Plural: a person may have more than one address to hand to a form.
    pub emails: Vec<Field<String>>,
    pub phones: Vec<Field<String>>,
    pub address: Field<Address>,
    pub organisation: Field<String>,
    pub job_title: Field<String>,
    /// ISO 8601 (`YYYY-MM-DD`), stored as plain text -- this module does
    /// not parse or validate the date shape, only that its *content* is
    /// not payment-shaped (the denylist runs over every field's raw text
    /// regardless). Always sensitive; see [`Profile::normalize_sensitivity`].
    pub date_of_birth: Field<String>,
    pub website: Field<String>,
    /// Free-form. The field most likely to accidentally collect a pasted
    /// card number or bank detail, so the denylist matters most here.
    pub notes: Field<String>,
}

impl Profile {
    /// `%APPDATA%\Wingman\profile.bin`.
    pub fn path() -> Result<PathBuf> {
        let base = crate::known_folder::roaming_app_data()
            .context("could not determine the platform config directory")?;
        Ok(base.join("Wingman").join("profile.bin"))
    }

    /// Loads the profile from the well-known path. A missing file is not
    /// an error -- first run, or a user who has never opened the profile
    /// page (#50) -- and yields [`Profile::default`].
    pub fn load() -> Result<Profile> {
        Self::load_from(&Self::path()?)
    }

    /// Same as [`Profile::load`] but against an arbitrary path, so tests
    /// never touch the real `%APPDATA%\Wingman\profile.bin` (rule 9).
    ///
    /// Runs the payment-data denylist over every field after decrypting
    /// and parsing, *before* returning the profile to the caller -- see
    /// the module docs. A file that fails it is reported as `Err`, not
    /// silently repaired by dropping the offending field, since a repaired
    /// write-back here could paper over a real compromise of the file.
    pub fn load_from(path: &Path) -> Result<Profile> {
        if !path.exists() {
            return Ok(Profile::default());
        }
        let envelope = fs::read(path).context("failed to read the profile file")?;
        let plaintext = dpapi::unprotect(&envelope, Some(dpapi::PROFILE_ENTROPY))
            .context("failed to decrypt the profile file")?;
        let mut profile: Profile = serde_json::from_slice(&plaintext)
            .context("failed to parse the decrypted profile as JSON")?;
        profile.normalize_sensitivity();
        profile
            .validate()
            .context("the stored profile failed the payment-data denylist on load")?;
        Ok(profile)
    }

    /// Saves the profile to the well-known path.
    pub fn save(&self) -> Result<()> {
        self.save_to(&Self::path()?)
    }

    /// Same as [`Profile::save`] but against an arbitrary path (rule 9).
    ///
    /// Runs [`Profile::normalize_sensitivity`] and then the payment-data
    /// denylist *before touching the filesystem at all* -- a rejected
    /// value is never written, matching issue #36's requirement exactly.
    /// Writes atomically: a temp file next to `path`, then an OS-level
    /// rename, so a reader (or a crash mid-write) never observes a
    /// half-written envelope.
    pub fn save_to(&self, path: &Path) -> Result<()> {
        let mut normalized = self.clone();
        normalized.normalize_sensitivity();
        normalized
            .validate()
            .context("profile was not saved: a field failed the payment-data denylist")?;

        let plaintext =
            serde_json::to_vec(&normalized).context("failed to serialize the profile")?;
        let envelope = dpapi::protect(plaintext, Some(dpapi::PROFILE_ENTROPY))
            .context("failed to encrypt the profile")?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).context("failed to create the profile directory")?;
        }
        let mut tmp_name = path.as_os_str().to_os_string();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);
        fs::write(&tmp_path, &envelope).context("failed to write the profile temp file")?;
        fs::rename(&tmp_path, path).context("failed to move the profile temp file into place")?;

        #[cfg(windows)]
        Self::restrict_acl(path);

        Ok(())
    }

    /// Restricts the profile file's ACL to the current user only, via
    /// `icacls`. Best-effort, same as `Config::restrict_acl`: any failure
    /// (missing binary, non-NTFS volume) is ignored, since DPAPI's own
    /// user-scoped encryption is the real protection here -- this is
    /// defense in depth, not the only barrier.
    #[cfg(windows)]
    fn restrict_acl(path: &Path) {
        let username = match std::env::var("USERNAME") {
            Ok(u) if !u.is_empty() => u,
            _ => return,
        };
        let _ = std::process::Command::new("icacls")
            .arg(path)
            .arg("/inheritance:r")
            .arg("/grant:r")
            .arg(format!("{username}:F"))
            .output();
    }

    /// Enforces the one non-negotiable sensitivity rule: date of birth is
    /// always sensitive, regardless of what was loaded from disk or set in
    /// memory. Called by both `load_from` and `save_to`, so it holds for a
    /// profile built by hand in a test, one round-tripped through disk, and
    /// one loaded from a file some older build (without this rule) wrote.
    fn normalize_sensitivity(&mut self) {
        self.date_of_birth.sensitive = true;
    }

    /// Runs the payment-data denylist over every field. See the module
    /// docs' second paragraph for why this exists alongside the type-level
    /// guarantee of having no payment fields at all.
    fn validate(&self) -> Result<()> {
        check("full_name", &self.full_name.value)?;
        check("preferred_name", &self.preferred_name.value)?;
        for (i, email) in self.emails.iter().enumerate() {
            check(&format!("email #{}", i + 1), &email.value)?;
        }
        for (i, phone) in self.phones.iter().enumerate() {
            check(&format!("phone #{}", i + 1), &phone.value)?;
        }
        for (i, line) in self.address.value.lines.iter().enumerate() {
            check(&format!("address line #{}", i + 1), line)?;
        }
        check("address city", &self.address.value.city)?;
        check("address region", &self.address.value.region)?;
        check("address postcode", &self.address.value.postcode)?;
        check("address country", &self.address.value.country)?;
        check("organisation", &self.organisation.value)?;
        check("job_title", &self.job_title.value)?;
        check("date_of_birth", &self.date_of_birth.value)?;
        check("website", &self.website.value)?;
        check("notes", &self.notes.value)?;
        Ok(())
    }
}

fn check(field: &str, value: &str) -> Result<()> {
    denylist::check_field(field, value).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_path(tag: &str) -> PathBuf {
        // Test-only scratch directory under the OS temp dir plus this
        // process's pid, so parallel test runs never collide and nothing
        // here ever touches the real %APPDATA%\Wingman\profile.bin
        // (rule 9).
        std::env::temp_dir().join(format!(
            "wingman-profile-test-{}-{}-{}",
            std::process::id(),
            tag,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    fn sample_profile() -> Profile {
        Profile {
            full_name: Field::new("Ada Lovelace".to_string(), false),
            preferred_name: Field::new("Ada".to_string(), false),
            emails: vec![
                Field::new("ada@example.com".to_string(), false),
                Field::new("ada.personal@example.com".to_string(), true),
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
            date_of_birth: Field::new("1815-12-10".to_string(), false), // normalized to true on save/load
            website: Field::new("https://example.com".to_string(), false),
            notes: Field::new("Prefers async standups.".to_string(), false),
        }
    }

    // -- round trip -----------------------------------------------------

    #[test]
    fn round_trips_a_full_profile() {
        let path = scratch_path("round-trip");
        let mut profile = sample_profile();
        profile.save_to(&path).expect("save_to should succeed");

        let loaded = Profile::load_from(&path).expect("load_from should succeed");

        profile.normalize_sensitivity(); // date_of_birth.sensitive becomes true
        assert_eq!(loaded, profile);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_loads_as_default() {
        let path = scratch_path("missing");
        let loaded = Profile::load_from(&path).expect("a missing file should not be an error");
        assert_eq!(loaded, Profile::default());
    }

    #[test]
    fn date_of_birth_is_always_sensitive_after_load_even_if_saved_as_false() {
        let path = scratch_path("dob-sensitive");
        let profile = Profile {
            date_of_birth: Field::new("2000-01-01".to_string(), false),
            ..Profile::default()
        };
        profile.save_to(&path).expect("save_to should succeed");

        let loaded = Profile::load_from(&path).expect("load_from should succeed");
        assert!(loaded.date_of_birth.sensitive);

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn non_dob_sensitivity_flags_round_trip_as_set() {
        let path = scratch_path("sensitivity-round-trip");
        let profile = sample_profile();
        profile.save_to(&path).expect("save_to should succeed");

        let loaded = Profile::load_from(&path).expect("load_from should succeed");
        assert!(!loaded.emails[0].sensitive);
        assert!(loaded.emails[1].sensitive);

        let _ = fs::remove_file(&path);
    }

    // -- atomic write -----------------------------------------------------

    #[test]
    fn save_to_writes_atomically_leaving_no_temp_file_behind() {
        let path = scratch_path("atomic");
        let profile = sample_profile();
        profile.save_to(&path).expect("save_to should succeed");

        assert!(path.exists(), "the profile file itself must exist");
        let mut tmp = path.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert!(
            !PathBuf::from(tmp).exists(),
            "the temp file must be renamed away, not left behind"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn saved_file_is_not_plaintext_json() {
        let path = scratch_path("encrypted");
        let profile = Profile {
            full_name: Field::new("a very distinctive plaintext marker".to_string(), false),
            ..Profile::default()
        };
        profile.save_to(&path).expect("save_to should succeed");

        let on_disk = fs::read(&path).expect("file should exist");
        let on_disk_text = String::from_utf8_lossy(&on_disk);
        assert!(
            !on_disk_text.contains("a very distinctive plaintext marker"),
            "the on-disk bytes must not contain the plaintext value"
        );
        assert!(
            !on_disk_text.contains("full_name"),
            "the on-disk bytes must not contain the plaintext JSON field names"
        );

        let _ = fs::remove_file(&path);
    }

    // -- payment-data denylist: enforced on write --------------------------

    #[test]
    fn save_rejects_a_card_number_in_notes_and_writes_nothing() {
        let path = scratch_path("denylist-write-card");
        let profile = Profile {
            notes: Field::new("card: 4111 1111 1111 1111".to_string(), false),
            ..Profile::default()
        };

        let result = profile.save_to(&path);
        assert!(result.is_err());
        let message = format!("{:#}", result.unwrap_err());
        assert!(message.contains("notes"));
        assert!(
            !message.contains("4111"),
            "error must never echo the value: {message}"
        );
        assert!(!path.exists(), "a rejected profile must never be written");
    }

    #[test]
    fn save_rejects_an_iban_in_an_address_line_and_writes_nothing() {
        let path = scratch_path("denylist-write-iban");
        let mut profile = Profile::default();
        profile.address.value.lines = vec!["GB29 NWBK 6016 1331 9268 19".to_string()];

        let result = profile.save_to(&path);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn save_rejects_a_cvv_label_in_notes_and_writes_nothing() {
        let path = scratch_path("denylist-write-cvv");
        let profile = Profile {
            notes: Field::new("CVV: 123".to_string(), false),
            ..Profile::default()
        };

        let result = profile.save_to(&path);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn save_rejects_a_bank_account_label_in_notes_and_writes_nothing() {
        let path = scratch_path("denylist-write-bank");
        let profile = Profile {
            notes: Field::new("Account Number: 12345678".to_string(), false),
            ..Profile::default()
        };

        let result = profile.save_to(&path);
        assert!(result.is_err());
        assert!(!path.exists());
    }

    #[test]
    fn save_accepts_an_ordinary_profile() {
        let path = scratch_path("denylist-accepts-ordinary");
        let profile = sample_profile();
        assert!(profile.save_to(&path).is_ok());
        let _ = fs::remove_file(&path);
    }

    // -- payment-data denylist: enforced on load ---------------------------

    #[test]
    fn load_rejects_a_tampered_file_that_bypassed_the_write_time_check() {
        let path = scratch_path("denylist-load");
        // Simulate a file that reached disk without going through
        // save_to's own validate() call (a hand-edited file, or a bug in
        // some future writer): build a Profile with a card number
        // directly, serialize and encrypt it with the same envelope
        // format save_to uses, but skip validate().
        let profile = Profile {
            notes: Field::new("card: 4111 1111 1111 1111".to_string(), false),
            ..Profile::default()
        };
        let plaintext = serde_json::to_vec(&profile).unwrap();
        let envelope = dpapi::protect(plaintext, Some(dpapi::PROFILE_ENTROPY)).unwrap();
        fs::write(&path, &envelope).unwrap();

        let result = Profile::load_from(&path);
        assert!(
            result.is_err(),
            "a file containing a payment-shaped value must be rejected on load, not silently loaded"
        );

        let _ = fs::remove_file(&path);
    }

    // -- envelope / decryption failure surfaces as Err, not a panic ---------

    #[test]
    fn load_of_a_non_dpapi_file_fails_cleanly() {
        let path = scratch_path("not-dpapi");
        fs::write(&path, b"this is not a DPAPI envelope at all").unwrap();

        let result = Profile::load_from(&path);
        assert!(result.is_err());

        let _ = fs::remove_file(&path);
    }
}
