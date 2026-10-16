//! Portable mode (#130): a marker file next to the exe moves every file
//! Wingman keeps (config, actions, profile, the store, the egress log, the
//! crash note) next to the exe, and turns the autostart (Run key) off.
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
//! Secrets are not moved: API keys stay in Windows Credential Manager, never
//! in the data folder. A portable copy on another machine starts with no keys
//! (set them again or use the `*_API_KEY` environment variables).
//!
//! FAT and exFAT drives have no ACLs. The owner-only restriction is skipped
//! there (see [`skip_acl`]) so the start does not fail; the files on such a
//! drive are readable by anyone who can read the drive. They hold no API key.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The marker file name. Put an empty file with this name next to the exe.
pub const MARKER: &str = "wingman.portable";

/// `FILE_PERSISTENT_ACLS` in `GetVolumeInformationW`'s flags.
const FILE_PERSISTENT_ACLS: u32 = 0x0000_0008;

/// The data root for an exe that lives in `exe_dir`: `exe_dir` itself when the
/// marker file is there. Pure apart from the one `is_file` check, so tests
/// point it at a scratch folder.
pub fn root_for(exe_dir: &Path) -> Option<PathBuf> {
    exe_dir
        .join(MARKER)
        .is_file()
        .then(|| exe_dir.to_path_buf())
}

/// The portable data root for this process, decided once at first use.
/// `None` is normal mode. If the exe path cannot be read the answer is
/// normal mode, the same as before this feature existed.
pub fn root() -> Option<&'static Path> {
    static ROOT: OnceLock<Option<PathBuf>> = OnceLock::new();
    ROOT.get_or_init(|| {
        let exe = std::env::current_exe().ok()?;
        root_for(exe.parent()?)
    })
    .as_deref()
}

/// Whether this process runs in portable mode.
pub fn is_portable() -> bool {
    root().is_some()
}

/// Pure core of [`volume_has_acls`]: the volume flags say ACLs are kept.
fn flags_have_acls(flags: u32) -> bool {
    flags & FILE_PERSISTENT_ACLS != 0
}

/// Whether the volume holding `path` keeps ACLs. `None` when it cannot be
/// told (the caller then tries the restriction anyway, which is the safe
/// outcome: a failure there still aborts the write).
pub fn volume_has_acls(path: &Path) -> Option<bool> {
    use std::os::windows::ffi::OsStrExt;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetVolumeInformationW, GetVolumePathNameW};

    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut root = [0u16; 261];
    let mut flags = 0u32;
    unsafe {
        GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut root).ok()?;
        GetVolumeInformationW(
            PCWSTR(root.as_ptr()),
            None,
            None,
            None,
            Some(&mut flags),
            None,
        )
        .ok()?;
    }
    Some(flags_have_acls(flags))
}

/// Pure decision for [`skip_acl`].
fn acl_skip_wanted(portable: bool, has_acls: Option<bool>) -> bool {
    portable && has_acls == Some(false)
}

/// True when the owner-only restriction on `path` should be skipped: portable
/// mode, on a volume that is known not to keep ACLs (FAT, exFAT). Normal mode
/// never skips, and an unknown answer never skips.
pub fn skip_acl(path: &Path) -> bool {
    is_portable() && acl_skip_wanted(true, volume_has_acls(path))
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
    fn acl_flag_is_read_from_the_volume_flags() {
        assert!(flags_have_acls(0x0000_0008 | 0x0000_0001));
        assert!(!flags_have_acls(0x0000_0001));
        assert!(!flags_have_acls(0));
    }

    #[test]
    fn acl_restriction_is_skipped_only_in_portable_mode_on_a_known_no_acl_volume() {
        assert!(acl_skip_wanted(true, Some(false)));
        assert!(!acl_skip_wanted(true, Some(true)));
        assert!(!acl_skip_wanted(true, None));
        assert!(!acl_skip_wanted(false, Some(false)));
    }

    #[test]
    fn the_temp_volume_is_not_reported_as_having_no_acls() {
        let dir = scratch("vol");
        assert_ne!(volume_has_acls(&dir), Some(false));
        assert!(volume_has_acls(&dir).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
