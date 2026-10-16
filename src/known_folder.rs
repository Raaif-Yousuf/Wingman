//! Thin wrapper over `SHGetKnownFolderPath`, used to resolve well-known
//! Windows folders directly via the `windows` crate this project already
//! depends on, instead of the `dirs` crate (issue #160: `dirs` pulled in
//! `dirs-sys` -> `option-ext`, MPL-2.0, which was only a TEMPORARY per-crate
//! exception in `deny.toml`, not on AGENTS.md rule 2's permissive
//! allowlist).
//!
//! Today only [`roaming_app_data`] (`%APPDATA%`, used by `config.rs`) is
//! needed; [`known_folder`] is kept generic over the `FOLDERID_*` GUID so a
//! second well-known folder (e.g. `FOLDERID_LocalAppData`) is a one-line
//! addition, not a second copy of the `SHGetKnownFolderPath`/`CoTaskMemFree`
//! dance.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use windows::core::GUID;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    FOLDERID_LocalAppData, FOLDERID_Profile, FOLDERID_RoamingAppData, SHGetKnownFolderPath,
    KNOWN_FOLDER_FLAG,
};

/// Resolves a known-folder GUID (e.g. `FOLDERID_RoamingAppData`) to its path
/// via `SHGetKnownFolderPath`.
///
/// The returned `PWSTR` is heap-allocated by the shell and must be freed by
/// the caller with `CoTaskMemFree` regardless of what happens afterward
/// (MSDN). This frees it on every path once it has been obtained: after a
/// successful UTF-16 decode, and after a failed one -- the only path that
/// frees nothing is `SHGetKnownFolderPath` itself returning an error, since
/// then no buffer was ever allocated for this call to own.
fn known_folder(id: &GUID) -> Result<PathBuf> {
    unsafe {
        let raw = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None)
            .context("SHGetKnownFolderPath failed")?;
        let decoded = raw.to_string();
        CoTaskMemFree(Some(raw.0 as *const _));
        let text = decoded.context("known folder path was not valid UTF-16")?;
        Ok(PathBuf::from(text))
    }
}

/// `%APPDATA%` (roaming), e.g. `C:\Users\<user>\AppData\Roaming`. Same
/// folder `dirs::config_dir()` resolved to on Windows.
pub fn roaming_app_data() -> Result<PathBuf> {
    resolve(crate::portable::root().as_deref(), || {
        known_folder(&FOLDERID_RoamingAppData)
    })
}

/// The portable seam (#130): in portable mode both app-data folders are the
/// folder next to the exe, and the system is never asked. Every file path in
/// the crate comes from [`roaming_app_data`] or [`local_app_data`], so this
/// one function moves them all.
fn resolve(
    portable_root: Option<&Path>,
    system: impl FnOnce() -> Result<PathBuf>,
) -> Result<PathBuf> {
    match portable_root {
        Some(root) => Ok(root.to_path_buf()),
        None => system(),
    }
}

/// `%LOCALAPPDATA%`, e.g. `C:\Users\<user>\AppData\Local`. Machine-scoped,
/// never synced by a roaming profile -- used for `egress.rs`'s local egress
/// log (#106) and the expansion plan's eventual `store.rs` SQLite database
/// (#46), neither of which should follow a user's roaming profile between
/// machines the way `config.toml` (in [`roaming_app_data`]) intentionally
/// does.
pub fn local_app_data() -> Result<PathBuf> {
    resolve(crate::portable::root().as_deref(), || {
        known_folder(&FOLDERID_LocalAppData)
    })
}

/// `%USERPROFILE%`, e.g. `C:\Users\<user>`. Issue #270: the one place the
/// account name lives that `diagnostics.rs` needs to redact it out of a
/// pasteable report (both `config_path`, under `roaming_app_data`, and an
/// Ollama `image_path` can carry this prefix).
pub fn user_profile() -> Result<PathBuf> {
    known_folder(&FOLDERID_Profile)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Removing the `portable::root()` call from either public function
    /// makes this fail (the real wiring, not just the `resolve` helper).
    #[test]
    fn both_public_folders_follow_the_portable_root() {
        let root = PathBuf::from(r"E:\usb\Wingman-test-root");
        let _forced = crate::portable::ForcedRoot::new(&root);
        assert_eq!(roaming_app_data().unwrap(), root);
        assert_eq!(local_app_data().unwrap(), root);
    }

    #[test]
    fn portable_root_wins_and_the_system_is_not_asked() {
        let root = PathBuf::from(r"C:\usb\Wingman-test-root");
        let got = resolve(Some(&root), || panic!("system must not be asked")).unwrap();
        assert_eq!(got, root);
    }

    #[test]
    fn without_a_portable_root_the_system_answer_is_used() {
        let sys = PathBuf::from(r"C:\sys\Roaming");
        assert_eq!(resolve(None, || Ok(sys.clone())).unwrap(), sys);
    }

    /// Read-only comparison against the `APPDATA` env var Windows itself
    /// sets for every process to the same folder -- never writes anything
    /// under the resolved path. Per AGENTS.md rule 1, this test never reads
    /// `config.toml`'s contents; it only compares directory paths.
    #[test]
    fn roaming_app_data_matches_the_appdata_env_var() {
        let from_api = roaming_app_data().expect("SHGetKnownFolderPath should succeed");
        let from_env =
            std::env::var("APPDATA").expect("APPDATA should be set in this process's env");
        assert_eq!(from_api, PathBuf::from(from_env));
    }

    #[test]
    fn roaming_app_data_is_absolute_and_non_empty() {
        let path = roaming_app_data().expect("SHGetKnownFolderPath should succeed");
        assert!(path.is_absolute(), "{path:?} should be absolute");
        assert!(!path.as_os_str().is_empty());
    }

    /// Same comparison as `roaming_app_data_matches_the_appdata_env_var`,
    /// against `LOCALAPPDATA` instead.
    #[test]
    fn local_app_data_matches_the_localappdata_env_var() {
        let from_api = local_app_data().expect("SHGetKnownFolderPath should succeed");
        let from_env = std::env::var("LOCALAPPDATA")
            .expect("LOCALAPPDATA should be set in this process's env");
        assert_eq!(from_api, PathBuf::from(from_env));
    }

    #[test]
    fn local_app_data_is_absolute_and_differs_from_roaming() {
        let local = local_app_data().expect("SHGetKnownFolderPath should succeed");
        let roaming = roaming_app_data().expect("SHGetKnownFolderPath should succeed");
        assert!(local.is_absolute(), "{local:?} should be absolute");
        assert_ne!(local, roaming);
    }

    /// Same comparison as `roaming_app_data_matches_the_appdata_env_var`,
    /// against `USERPROFILE` instead. This is the ONLY test in the crate
    /// that reads the real account name (via the `USERPROFILE` env var
    /// Windows itself sets), and it never prints or asserts the name
    /// itself -- only that the two paths, whatever they are, agree.
    #[test]
    fn user_profile_matches_the_userprofile_env_var() {
        let from_api = user_profile().expect("SHGetKnownFolderPath should succeed");
        let from_env =
            std::env::var("USERPROFILE").expect("USERPROFILE should be set in this process's env");
        assert_eq!(from_api, PathBuf::from(from_env));
    }

    #[test]
    fn user_profile_is_an_ancestor_of_roaming_app_data() {
        let profile = user_profile().expect("SHGetKnownFolderPath should succeed");
        let roaming = roaming_app_data().expect("SHGetKnownFolderPath should succeed");
        assert!(
            roaming.starts_with(&profile),
            "{roaming:?} should be under {profile:?}"
        );
    }
}
