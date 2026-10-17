//! Owner-only access for the store directory.
//!
//! WAL mode makes SQLite create `wingman.db-wal` and `wingman.db-shm` on
//! demand, so a per-file ACL (what `egress.rs` does for its one log file)
//! would miss them. The directory gets one inheritable grant to the current
//! user, inheritance from its parent is cut, and the SYSTEM and
//! Administrators entries an elevated creator leaves are removed (#995), so
//! every file SQLite creates inside it starts out owner-only. The arguments
//! and the `icacls` call are shared with the config file
//! (`owner_acl::restrict_to_owner`).

use std::path::Path;

use super::StoreError;

/// Restricts `dir` (which must exist) to the current user. An error means the
/// store must not open: an unrestricted store is what this module exists to
/// avoid.
pub fn restrict_dir(dir: &Path) -> Result<(), StoreError> {
    // Portable mode (#130): an owner-only folder from one PC would lock the
    // next user out, and FAT/exFAT has no ACLs to set. Skipped, so the store
    // keeps the drive's permissions and the drive should be kept private.
    if crate::portable::skip_acl() {
        return Ok(());
    }
    crate::owner_acl::restrict_to_owner(dir, true).map_err(StoreError::Acl)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restrict_dir_leaves_the_folder_and_a_later_file_owner_only() {
        // The wiring check for the store: the WAL files SQLite creates later
        // must inherit an owner-only list, with no SYSTEM or Administrators.
        let dir = std::env::temp_dir().join(format!("wingman-test-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for sid in ["*S-1-5-18", "*S-1-5-32-544"] {
            crate::owner_acl::dacl::plant(&dir, sid, true);
        }
        restrict_dir(&dir).expect("restrict should succeed");
        let child = dir.join("later.db-wal");
        std::fs::write(&child, b"x").unwrap();
        let check = std::panic::catch_unwind(|| {
            crate::owner_acl::dacl::assert_owner_only(&dir);
            crate::owner_acl::dacl::assert_owner_only(&child);
        });
        let _ = std::fs::remove_dir_all(&dir);
        check.unwrap();
    }

    #[test]
    fn files_already_in_the_folder_are_owner_only_after_an_upgrade() {
        // The upgrade case: wingman.db and its log files were made under the
        // old list. One child only inherited the folder's entries; the other
        // carries an explicit SYSTEM and Administrators entry, as a file made
        // by an elevated process does.
        let dir =
            std::env::temp_dir().join(format!("wingman-test-acl-upgrade-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for sid in ["*S-1-5-18", "*S-1-5-32-544"] {
            crate::owner_acl::dacl::plant(&dir, sid, true);
        }
        let inherited = dir.join("wingman.db");
        let explicit = dir.join("wingman.db-wal");
        std::fs::write(&inherited, b"x").unwrap();
        std::fs::write(&explicit, b"x").unwrap();
        for sid in ["*S-1-5-18", "*S-1-5-32-544"] {
            crate::owner_acl::dacl::plant(&explicit, sid, false);
        }
        restrict_dir(&dir).expect("restrict should succeed");
        let check = std::panic::catch_unwind(|| {
            crate::owner_acl::dacl::assert_owner_only(&inherited);
            crate::owner_acl::dacl::assert_owner_only(&explicit);
        });
        let _ = std::fs::remove_dir_all(&dir);
        check.unwrap();
    }

    #[test]
    fn a_folder_that_cannot_be_restricted_is_an_error() {
        // Fail closed: the store does not open on this error (`open_in`).
        let missing =
            std::env::temp_dir().join(format!("wingman-test-acl-missing-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&missing);
        assert!(matches!(restrict_dir(&missing), Err(StoreError::Acl(_))));
    }
}
