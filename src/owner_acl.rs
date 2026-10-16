//! What `icacls` needs to give a file or folder to the current user only,
//! shared by `config.rs` (the config file) and `store/acl.rs` (the store
//! folder) so the two cannot drift (#736).
//!
//! Two things were done wrongly before. `icacls` was started by bare name, and
//! Rust's Windows process search looks in the exe's own folder before
//! System32, so an `icacls.exe` placed next to `wingman.exe` would have run
//! instead (THEORY (unverified): same-user only, so low). And the grant named
//! the account from the `USERNAME` environment variable, which is inherited
//! from the parent process and is not a SID: it can be empty, renamed or hold
//! characters `icacls` parses. Now the program is the one in the system
//! folder, and the grant target is the current user's SID from the process
//! token.

use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    GetSidIdentifierAuthority, GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation,
    TokenUser, PSID, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

/// `icacls.exe` inside `system_dir` (the Windows `System32` folder).
fn icacls_in(system_dir: &str) -> PathBuf {
    PathBuf::from(system_dir.trim_end_matches(['\\', '/'])).join("icacls.exe")
}

/// The system folder's `icacls.exe`, never a search by name.
pub fn icacls_path() -> Result<PathBuf, String> {
    let mut buf = [0u16; 260];
    let n = unsafe { GetSystemDirectoryW(Some(&mut buf)) } as usize;
    if n == 0 || n >= buf.len() {
        return Err("could not find the Windows system folder".to_string());
    }
    Ok(icacls_in(&String::from_utf16_lossy(&buf[..n])))
}

/// A `Command` for the system folder's `icacls`, with no console window
/// (Wingman has no console, so without the flag each call would flash one).
pub fn icacls_command() -> Result<Command, String> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let mut cmd = Command::new(icacls_path()?);
    cmd.creation_flags(CREATE_NO_WINDOW);
    Ok(cmd)
}

/// A SID in its string form: `S-1-<authority>-<sub>-<sub>...`. The authority
/// is six big-endian bytes, written in decimal when it fits in 32 bits and as
/// `0x` and 12 hex digits otherwise (the Windows rule).
fn sid_string(authority: [u8; 6], subs: &[u32]) -> String {
    let mut out = String::from("S-1-");
    if authority[0] == 0 && authority[1] == 0 {
        let v = u32::from_be_bytes([authority[2], authority[3], authority[4], authority[5]]);
        out.push_str(&v.to_string());
    } else {
        out.push_str("0x");
        for b in authority {
            out.push_str(&format!("{b:02X}"));
        }
    }
    for s in subs {
        out.push('-');
        out.push_str(&s.to_string());
    }
    out
}

/// The current user's SID as a string, from the process token.
fn read_current_user_sid() -> Result<String, String> {
    unsafe {
        let mut token = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)
            .map_err(|e| format!("could not open the process token: {e}"))?;
        let result = (|| {
            let mut needed = 0u32;
            // The first call only reports the size (it fails by design).
            let _ = GetTokenInformation(token, TokenUser, None, 0, &mut needed);
            if needed == 0 {
                return Err("could not size the token user".to_string());
            }
            // u64 storage keeps the TOKEN_USER the call writes 8-byte aligned.
            let mut buf = vec![0u64; (needed as usize).div_ceil(8)];
            GetTokenInformation(
                token,
                TokenUser,
                Some(buf.as_mut_ptr().cast()),
                needed,
                &mut needed,
            )
            .map_err(|e| format!("could not read the token user: {e}"))?;
            let user = &*(buf.as_ptr() as *const TOKEN_USER);
            let sid: PSID = user.User.Sid;
            if sid.0.is_null() {
                return Err("the token user has no SID".to_string());
            }
            let count = *GetSidSubAuthorityCount(sid) as u32;
            let subs: Vec<u32> = (0..count).map(|i| *GetSidSubAuthority(sid, i)).collect();
            let authority = (*GetSidIdentifierAuthority(sid)).Value;
            Ok(sid_string(authority, &subs))
        })();
        let _ = CloseHandle(token);
        result
    }
}

/// The current user's SID string (`S-1-5-21-...`), read once per process.
pub fn current_user_sid() -> Result<String, String> {
    static SID: OnceLock<Result<String, String>> = OnceLock::new();
    SID.get_or_init(read_current_user_sid).clone()
}

/// The account part of an `icacls` grant: `*` and the SID, which `icacls`
/// reads as "this SID" rather than a name to look up.
pub fn grant_account() -> Result<String, String> {
    Ok(format!("*{}", current_user_sid()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn icacls_is_named_by_full_path() {
        assert_eq!(
            icacls_in(r"C:\Windows\System32"),
            PathBuf::from(r"C:\Windows\System32\icacls.exe")
        );
        assert_eq!(
            icacls_in(r"C:\Windows\System32\"),
            PathBuf::from(r"C:\Windows\System32\icacls.exe")
        );
    }

    #[test]
    fn the_real_icacls_path_is_absolute_and_exists() {
        let p = icacls_path().unwrap();
        assert!(p.is_absolute(), "{p:?}");
        assert!(p.ends_with("System32\\icacls.exe") || p.ends_with("system32\\icacls.exe"));
        assert!(p.exists(), "{p:?}");
    }

    #[test]
    fn sid_strings_follow_the_windows_form() {
        // Everyone: S-1-1-0, Administrators: S-1-5-32-544.
        assert_eq!(sid_string([0, 0, 0, 0, 0, 1], &[0]), "S-1-1-0");
        assert_eq!(sid_string([0, 0, 0, 0, 0, 5], &[32, 544]), "S-1-5-32-544");
        assert_eq!(
            sid_string([0, 0, 0, 0, 0, 5], &[21, 1, 2, 3, 1001]),
            "S-1-5-21-1-2-3-1001"
        );
        // An authority above 32 bits is hex.
        assert_eq!(sid_string([1, 2, 3, 4, 5, 6], &[7]), "S-1-0x010203040506-7");
    }

    #[test]
    fn the_current_user_sid_is_a_user_sid_and_the_grant_names_it() {
        let sid = current_user_sid().unwrap();
        assert!(
            sid.starts_with("S-1-5-") || sid.starts_with("S-1-12-1-"),
            "{sid}"
        );
        assert_eq!(grant_account().unwrap(), format!("*{sid}"));
    }
}
