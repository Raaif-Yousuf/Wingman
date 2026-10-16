//! Portable mode (#130): a marker file next to the exe moves every file
//! Wingman keeps (config, actions, profile, the store, the egress log, the
//! crash note, the calendar temp files) next to the exe, and turns the
//! autostart (Run key) off.
//!
//! Decision: the marker is an empty file named [`MARKER`] in the same folder
//! as `wingman.exe`. Nothing is read from it, so its content does not matter.
//! The data folder is `Wingman` next to the exe, so the files keep the same
//! relative names they have under `%APPDATA%` and `%LOCALAPPDATA%`.
//!
//! The seam is [`root`]: `known_folder::roaming_app_data` and
//! `known_folder::local_app_data` return it when it is set, and every file
//! path in the crate already goes through those two. A new file path must do
//! the same, never read `%APPDATA%` directly.
//!
//! Secrets are not moved: API keys stay in Windows Credential Manager. A
//! portable copy on another machine starts with no keys (set them again or
//! use the `*_API_KEY` environment variables). A key can still sit in
//! `config.toml` if the Credential Manager write failed or the user typed it
//! by hand, so keep the drive private.
//!
//! The owner-only restriction is skipped in portable mode (see [`skip_acl`]):
//! the drive moves between PCs and users, and FAT/exFAT has no ACLs. Files
//! keep the drive's own permissions.
//!
//! The marker is ignored when the exe is under `Program Files` or
//! `WindowsApps`: a writable folder there could otherwise plant a marker and
//! a config that points Wingman at another server.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The marker file name. Put an empty file with this name next to the exe.
pub const MARKER: &str = "wingman.portable";

/// Folders where an exe is installed for all users. A marker planted next to
/// an installed exe must not switch the data folder.
fn is_install_dir(exe_dir: &Path) -> bool {
    exe_dir.components().any(|c| {
        let name = c.as_os_str().to_string_lossy().to_ascii_lowercase();
        name == "program files" || name == "program files (x86)" || name == "windowsapps"
    })
}

/// The data root for an exe that lives in `exe_dir`: `exe_dir` itself when the
/// marker file is there and the folder is not an install folder. Pure apart
/// from the one `is_file` check, so tests point it at a scratch folder.
pub fn root_for(exe_dir: &Path) -> Option<PathBuf> {
    if is_install_dir(exe_dir) {
        return None;
    }
    exe_dir
        .join(MARKER)
        .is_file()
        .then(|| exe_dir.to_path_buf())
}

/// The portable data root for this process, decided once at first use.
/// `None` is normal mode. If the exe path cannot be read the answer is
/// normal mode, the same as before this feature existed.
pub fn root() -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(forced) = TEST_ROOT.with(|r| r.borrow().clone()) {
        return Some(forced);
    }
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        root_for(exe.parent()?)
    })
    .clone()
}

#[cfg(test)]
thread_local! {
    /// Test seam: makes [`root`] answer with this folder on this thread.
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> =
        const { std::cell::RefCell::new(None) };
}

/// Forces portable mode with a given root, on this thread, until dropped.
#[cfg(test)]
pub(crate) struct ForcedRoot;

#[cfg(test)]
impl ForcedRoot {
    pub(crate) fn new(dir: &Path) -> Self {
        TEST_ROOT.with(|r| *r.borrow_mut() = Some(dir.to_path_buf()));
        ForcedRoot
    }
}

#[cfg(test)]
impl Drop for ForcedRoot {
    fn drop(&mut self) {
        TEST_ROOT.with(|r| *r.borrow_mut() = None);
    }
}

/// Whether this process runs in portable mode.
pub fn is_portable() -> bool {
    root().is_some()
}

/// True when the owner-only restriction on a data file or folder is skipped:
/// always in portable mode. The drive moves between PCs and users, so an ACL
/// that strips inheritance on one PC would lock the next user out of their
/// own data, and FAT/exFAT drives have no ACLs to set at all. Normal mode
/// never skips.
pub fn skip_acl() -> bool {
    is_portable()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "wingman-test-portable-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_marker_means_normal_mode() {
        let dir = scratch("none");
        assert_eq!(root_for(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_file_makes_the_exe_folder_the_root() {
        let dir = scratch("marker");
        std::fs::write(dir.join(MARKER), b"").unwrap();
        assert_eq!(root_for(&dir), Some(dir.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_the_marker_name_is_not_a_marker() {
        let dir = scratch("folder");
        std::fs::create_dir(dir.join(MARKER)).unwrap();
        assert_eq!(root_for(&dir), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_test_binary_is_not_portable() {
        assert!(!is_portable());
    }

    #[test]
    fn a_marker_under_an_install_folder_is_ignored() {
        let base = scratch("install");
        for sub in [
            "Program Files",
            "Program Files (x86)",
            "WindowsApps",
            "windowsapps",
        ] {
            let dir = base.join(sub).join("Wingman");
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(MARKER), b"").unwrap();
            assert_eq!(root_for(&dir), None, "{sub}");
        }
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_folder_names_are_matched_whole() {
        assert!(!is_install_dir(Path::new(r"E:\tools\Wingman")));
        assert!(!is_install_dir(Path::new(r"E:\Program Files Backup\W")));
        assert!(is_install_dir(Path::new(r"C:\Program Files\Wingman")));
        assert!(is_install_dir(Path::new(r"C:\Program Files (x86)\W")));
    }

    #[test]
    fn acl_restriction_is_skipped_only_in_portable_mode() {
        assert!(!skip_acl());
        let _forced = ForcedRoot::new(Path::new(r"E:\usb"));
        assert!(skip_acl());
    }
}
