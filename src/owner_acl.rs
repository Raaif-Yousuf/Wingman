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
/// Only a success is cached: a failed read is tried again next time.
pub fn current_user_sid() -> Result<String, String> {
    static SID: OnceLock<String> = OnceLock::new();
    if let Some(sid) = SID.get() {
        return Ok(sid.clone());
    }
    let sid = read_current_user_sid()?;
    Ok(SID.get_or_init(|| sid).clone())
}

/// The account part of an `icacls` grant: `*` and the SID, which `icacls`
/// reads as "this SID" rather than a name to look up.
pub fn grant_account() -> Result<String, String> {
    Ok(format!("*{}", current_user_sid()?))
}

/// The `icacls` arguments that leave one file or folder to `account` only
/// (#966, #995). `/inheritance:r` drops inherited entries but not explicit
/// ones, and an item made by an elevated process carries explicit SYSTEM and
/// Administrators entries from the token's default DACL, so those two are
/// removed by name. Neither is needed: the owner is the only reader, and
/// nothing here holds data a backup or indexer service must see. For a folder
/// the grant is inheritable, `(OI)(CI)`, so files made inside it later start
/// out owner-only too (SQLite's WAL files rely on that).
fn owner_only_args(path: &std::path::Path, account: &str, is_dir: bool) -> Vec<std::ffi::OsString> {
    use std::ffi::OsString;
    let flags = if is_dir { "(OI)(CI)" } else { "" };
    vec![
        path.as_os_str().to_owned(),
        OsString::from("/inheritance:r"),
        OsString::from("/grant:r"),
        OsString::from(format!("{account}:{flags}F")),
        OsString::from("/remove:g"),
        OsString::from("*S-1-5-18"),
        OsString::from("*S-1-5-32-544"),
    ]
}

/// Restricts `path` (which must exist) to the current user. The one place
/// every owner-only file and folder goes through. An `Err` must never be
/// swallowed by a caller that holds private data: the item is still readable
/// by whatever its parent allowed, so the caller refuses to go on (the store
/// does not open) or passes the error up so a card can show it.
pub fn restrict_to_owner(path: &std::path::Path, is_dir: bool) -> Result<(), String> {
    // #736: the grant names the current user by SID, and `icacls` is the one
    // in the system folder, with no console window.
    let account = grant_account()?;
    let output = icacls_command()?
        .args(owner_only_args(path, &account, is_dir))
        .output()
        .map_err(|e| format!("could not run icacls: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "icacls exited with {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

/// What a test needs to read an access list back without parsing icacls's
/// localized text: the SDDL from `icacls /save`, with the two well-known
/// short names spelled as SIDs.
#[cfg(test)]
pub(crate) mod dacl {
    use std::path::Path;

    /// One access entry: type letter (`A` allow), flags (`OICI`, `ID`), SID.
    pub struct Ace {
        pub kind: String,
        pub flags: String,
        pub sid: String,
    }

    pub fn parse_sddl(sddl: &str) -> (bool, Vec<Ace>) {
        // The file is the path on one line and the SDDL on the next.
        let line = sddl.lines().find(|l| l.starts_with("D:")).unwrap_or("");
        let dacl = line.get(2..).unwrap_or("");
        let dacl = dacl.split("S:").next().unwrap_or("");
        let protected = dacl.split('(').next().unwrap_or("").contains('P');
        let aces = dacl
            .split('(')
            .skip(1)
            .filter_map(|a| {
                let a = a.trim().trim_end_matches(')');
                let f: Vec<&str> = a.split(';').collect();
                if f.len() < 6 {
                    return None;
                }
                let sid = match f[5] {
                    "SY" => "S-1-5-18",
                    "BA" => "S-1-5-32-544",
                    other => other,
                };
                Some(Ace {
                    kind: f[0].to_string(),
                    flags: f[1].to_string(),
                    sid: sid.to_string(),
                })
            })
            .collect();
        (protected, aces)
    }

    /// The DACL of `path`, read with `icacls /save` into a scratch file.
    pub fn read(path: &Path) -> (bool, Vec<Ace>) {
        let out = std::env::temp_dir().join(format!(
            "wingman-test-dacl-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        let status = super::icacls_command()
            .unwrap()
            .arg(path)
            .arg("/save")
            .arg(&out)
            .arg("/q")
            .output()
            .expect("icacls should run");
        assert!(status.status.success(), "icacls /save failed");
        let bytes = std::fs::read(&out).expect("icacls /save wrote nothing");
        let _ = std::fs::remove_file(&out);
        // Measured: UTF-16LE with no byte order mark. Accept a mark or plain
        // bytes too.
        let text = if bytes.len() > 1 && (bytes.starts_with(&[0xFF, 0xFE]) || bytes[1] == 0) {
            let skip = if bytes.starts_with(&[0xFF, 0xFE]) {
                2
            } else {
                0
            };
            let units: Vec<u16> = bytes[skip..]
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(&bytes).into_owned()
        };
        parse_sddl(&text)
    }

    /// Gives `path` an explicit full-control entry for `sid` (`*S-1-...`),
    /// the way an elevated creator's default DACL does.
    pub fn plant(path: &Path, sid: &str, is_dir: bool) {
        let grant = format!("{sid}:{}F", if is_dir { "(OI)(CI)" } else { "" });
        let out = super::icacls_command()
            .unwrap()
            .arg(path)
            .arg("/grant")
            .arg(grant)
            .output()
            .expect("icacls should run");
        assert!(out.status.success(), "could not plant {sid}");
    }

    /// Asserts every entry on `path` is an allow entry for the current user.
    pub fn assert_owner_only(path: &Path) {
        let me = super::current_user_sid().unwrap();
        let (_, aces) = read(path);
        let sids: Vec<&str> = aces.iter().map(|a| a.sid.as_str()).collect();
        assert!(!aces.is_empty(), "no entries at all on {path:?}");
        assert!(
            aces.iter().all(|a| a.kind == "A" && a.sid == me),
            "{path:?} is not owner-only: {sids:?}"
        );
    }

    #[test]
    fn sddl_parsing_reads_flags_and_short_names() {
        let (prot, aces) = parse_sddl("D:PAI(A;OICI;FA;;;SY)(A;ID;FA;;;BA)(A;;FA;;;S-1-5-21-1)");
        assert!(prot);
        assert_eq!(aces.len(), 3);
        assert_eq!(aces[0].sid, "S-1-5-18");
        assert_eq!(aces[0].flags, "OICI");
        assert_eq!(aces[1].sid, "S-1-5-32-544");
        assert_eq!(aces[2].sid, "S-1-5-21-1");
        assert!(!parse_sddl("D:AI(A;;FA;;;SY)").0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn owner_only_file_args_grant_the_user_and_drop_system_and_admins() {
        let args: Vec<String> = owner_only_args(
            std::path::Path::new(r"C:\x\config.toml"),
            "*S-1-5-21-1",
            false,
        )
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
        assert_eq!(
            args,
            [
                r"C:\x\config.toml",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-21-1:F",
                "/remove:g",
                "*S-1-5-18",
                "*S-1-5-32-544"
            ]
        );
    }
    #[test]
    fn owner_only_dir_args_make_the_grant_inheritable() {
        let args: Vec<String> =
            owner_only_args(std::path::Path::new(r"C:\x\store"), "*S-1-5-21-1", true)
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
        assert_eq!(
            args,
            [
                r"C:\x\store",
                "/inheritance:r",
                "/grant:r",
                "*S-1-5-21-1:(OI)(CI)F",
                "/remove:g",
                "*S-1-5-18",
                "*S-1-5-32-544"
            ]
        );
    }

    fn scratch(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "wingman-test-owner-acl-{}-{name}",
            std::process::id()
        ))
    }

    #[test]
    fn a_file_is_left_to_the_current_user_only() {
        // #966: plant the entries an elevated creator leaves, restrict, read back.
        let path = scratch("file.txt");
        std::fs::write(&path, "x").unwrap();
        for sid in ["*S-1-5-18", "*S-1-5-32-544"] {
            dacl::plant(&path, sid, false);
        }
        let planted = dacl::read(&path).1.len();
        restrict_to_owner(&path, false).expect("restrict should succeed");
        let (protected, _) = dacl::read(&path);
        let check = std::panic::catch_unwind(|| dacl::assert_owner_only(&path));
        let _ = std::fs::remove_file(&path);
        assert!(planted >= 3, "the planting did nothing: {planted} entries");
        assert!(protected, "inheritance is not cut");
        check.unwrap();
    }

    #[test]
    fn a_directory_and_a_file_made_in_it_later_are_owner_only() {
        // #995: the folder kept SYSTEM and Administrators entries, and so
        // did every file SQLite made inside it.
        let dir = scratch("dir");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for sid in ["*S-1-5-18", "*S-1-5-32-544"] {
            dacl::plant(&dir, sid, true);
        }
        restrict_to_owner(&dir, true).expect("restrict should succeed");
        let child = dir.join("later.db-wal");
        std::fs::write(&child, b"x").unwrap();
        let me = current_user_sid().unwrap();
        let (dir_protected, dir_aces) = dacl::read(&dir);
        let (_, child_aces) = dacl::read(&child);
        let check = std::panic::catch_unwind(|| {
            dacl::assert_owner_only(&dir);
            dacl::assert_owner_only(&child);
        });
        let _ = std::fs::remove_dir_all(&dir);
        assert!(dir_protected, "inheritance is not cut on the folder");
        assert!(
            dir_aces
                .iter()
                .any(|a| a.sid == me && a.flags.contains("OI") && a.flags.contains("CI")),
            "the folder grant is not inheritable"
        );
        assert!(
            child_aces.iter().all(|a| a.flags.contains("ID")),
            "the new file's entries were not inherited"
        );
        check.unwrap();
    }

    #[test]
    fn a_failing_icacls_is_an_error_not_a_silent_success() {
        let missing = scratch("does-not-exist");
        let _ = std::fs::remove_file(&missing);
        assert!(restrict_to_owner(&missing, false).is_err());
    }

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
