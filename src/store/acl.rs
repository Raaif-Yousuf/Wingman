//! Owner-only access for the store directory.
//!
//! WAL mode makes SQLite create `wingman.db-wal` and `wingman.db-shm` on
//! demand, so a per-file ACL (what `egress.rs` does for its one log file)
//! would miss them. The directory gets one inheritable grant to the current
//! user and inheritance from its parent is cut, so every file SQLite creates
//! inside it starts out owner-only.

use std::ffi::OsString;
use std::path::Path;

use super::StoreError;

/// The `icacls` arguments. Pure so a test can pin them.
pub fn acl_args(dir: &Path, user: &str) -> Vec<OsString> {
    vec![
        dir.as_os_str().to_owned(),
        OsString::from("/inheritance:r"),
        OsString::from("/grant:r"),
        OsString::from(format!("{user}:(OI)(CI)F")),
    ]
}

/// Restricts `dir` (which must exist) to the current user. An error means the
/// store must not open: an unrestricted store is what this module exists to
/// avoid.
pub fn restrict_dir(dir: &Path) -> Result<(), StoreError> {
    // Portable mode (#130) on a FAT/exFAT drive has no ACLs to set, and
    // refusing to open would leave the portable copy with no store at all.
    // Only skipped when the volume is known to have none (see
    // `portable::skip_acl`); the store then relies on the drive being yours.
    if crate::portable::skip_acl(dir) {
        return Ok(());
    }
    let user = std::env::var("USERNAME")
        .ok()
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            StoreError::Acl("the USERNAME environment variable is not set".to_string())
        })?;
    // CREATE_NO_WINDOW: Wingman has no console, so without this flag icacls
    // flashes a console window.
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("icacls")
        .args(acl_args(dir, &user))
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|e| StoreError::Acl(format!("could not run icacls: {e}")))?;
    if !output.status.success() {
        return Err(StoreError::Acl(format!(
            "icacls exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn args_grant_the_user_an_inheritable_full_control_and_cut_inheritance() {
        let args = acl_args(Path::new("C:\\x\\store"), "bob");
        let text: Vec<String> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            text,
            [
                "C:\\x\\store",
                "/inheritance:r",
                "/grant:r",
                "bob:(OI)(CI)F"
            ]
        );
    }

    #[test]
    fn a_file_created_later_in_the_directory_is_owner_only() {
        // The wiring check for the grant itself: the WAL files SQLite
        // creates later must inherit it.
        let dir = std::env::temp_dir().join(format!("wingman-test-acl-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        restrict_dir(&dir).expect("restrict should succeed");
        let child = dir.join("later.db-wal");
        std::fs::write(&child, b"x").unwrap();
        let out = std::process::Command::new("icacls")
            .arg(&child)
            .output()
            .expect("icacls should run");
        let listing = String::from_utf8_lossy(&out.stdout).to_string();
        let user = std::env::var("USERNAME").unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        // The ACE itself: the user, inherited, full control. (The user name
        // alone is not enough: the temp path contains it.)
        assert!(
            listing.contains(&format!("{user}:(I)(F)")),
            "no inherited full-control ACE for {user}: {listing}"
        );
        for other in ["Everyone", "BUILTIN\\Users", "Authenticated Users"] {
            assert!(
                !listing.contains(other),
                "{other} still has access: {listing}"
            );
        }
    }
}
