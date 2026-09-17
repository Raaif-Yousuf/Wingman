//! Thin wrapper over `SHGetKnownFolderPath`, used to resolve well-known
//! Windows folders directly via the `windows` crate this project already
//! depends on, instead of the `dirs` crate (issue #160: `dirs` pulled in
//! `dirs-sys` -> `option-ext`, MPL-2.0, which was only a TEMPORARY per-crate
//! exception in `deny.toml`, not on CLAUDE.md rule 2's permissive
//! allowlist).
//!
//! Today only [`roaming_app_data`] (`%APPDATA%`, used by `config.rs`) is
//! needed; [`known_folder`] is kept generic over the `FOLDERID_*` GUID so a
//! second well-known folder (e.g. `FOLDERID_LocalAppData`) is a one-line
//! addition, not a second copy of the `SHGetKnownFolderPath`/`CoTaskMemFree`
//! dance.

use std::path::PathBuf;

use anyhow::{Context, Result};
use windows::core::GUID;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::UI::Shell::{
    SHGetKnownFolderPath, FOLDERID_RoamingAppData, KNOWN_FOLDER_FLAG,
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
    known_folder(&FOLDERID_RoamingAppData)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-only comparison against the `APPDATA` env var Windows itself
    /// sets for every process to the same folder -- never writes anything
    /// under the resolved path. Per CLAUDE.md rule 1, this test never reads
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
}
