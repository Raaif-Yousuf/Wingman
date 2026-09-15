//! "Start with Windows", via the per-user Run key.
//!
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` rather than a shortcut
//! in the Startup folder: it needs no admin rights, no COM (a `.lnk` means
//! `IShellLink` + `IPersistFile`), and it is a single value to read, write or
//! delete — which makes "is this currently on?" a question with an honest
//! answer instead of a guess.
//!
//! Nothing here touches `Config`. Autostart is state owned by Windows, so the
//! checkbox in Settings reads the registry when it opens rather than trusting
//! a mirrored bool that can drift the moment someone edits the Run key by hand.

use anyhow::{Context, Result};
use windows::core::PCWSTR;
use windows::Win32::Foundation::ERROR_FILE_NOT_FOUND;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The Run value name. Also what shows up in Task Manager's Startup tab.
const VALUE_NAME: &str = "copilot-ask";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open(access: windows::Win32::System::Registry::REG_SAM_FLAGS) -> Result<HKEY> {
    let mut key = HKEY::default();
    let sub = wide(RUN_KEY);
    unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            Some(0),
            access,
            &mut key,
        )
        .ok()
        .context("opening the Run key")?;
    }
    Ok(key)
}

/// The command we register: the current executable's full path, quoted so a
/// path containing spaces is not split into arguments by the shell.
fn command() -> Result<String> {
    let exe = std::env::current_exe().context("locating the running executable")?;
    Ok(format!("\"{}\"", exe.display()))
}

/// The value currently registered, if any.
fn current_value() -> Option<String> {
    let key = open(KEY_READ).ok()?;
    let name = wide(VALUE_NAME);
    let mut kind = Default::default();
    let mut len: u32 = 0;

    // Size probe first: the value is a path, and paths have no fixed length.
    let probe = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            None,
            Some(&mut len),
        )
    };
    if probe.is_err() || len == 0 {
        unsafe { let _ = RegCloseKey(key); }
        return None;
    }

    let mut buf = vec![0u8; len as usize];
    let read = unsafe {
        RegQueryValueExW(
            key,
            PCWSTR(name.as_ptr()),
            None,
            Some(&mut kind),
            Some(buf.as_mut_ptr()),
            Some(&mut len),
        )
    };
    unsafe { let _ = RegCloseKey(key); }
    if read.is_err() {
        return None;
    }

    // REG_SZ comes back as UTF-16 bytes, NUL-terminated.
    let units: Vec<u16> = buf
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    Some(String::from_utf16_lossy(&units))
}

/// Whether copilot-ask is registered to start with Windows.
pub fn is_enabled() -> bool {
    current_value().is_some()
}

/// Turn autostart on or off. Enabling always (re)writes the current
/// executable path, so toggling it off and on is also the way to repair a
/// stale entry after moving the exe.
pub fn set_enabled(on: bool) -> Result<()> {
    let key = open(KEY_WRITE)?;
    let name = wide(VALUE_NAME);

    let result = if on {
        let cmd = wide(&command()?);
        let bytes = unsafe {
            std::slice::from_raw_parts(cmd.as_ptr() as *const u8, cmd.len() * 2)
        };
        unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }
            .ok()
            .context("writing the Run value")
    } else {
        let deleted = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
        // Already absent is the desired end state, not a failure.
        if deleted == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            deleted.ok().context("deleting the Run value")
        }
    };

    unsafe { let _ = RegCloseKey(key); }
    result
}

/// If autostart is on but points somewhere else, rewrite it to this exe.
///
/// Called at startup. Without it, moving or rebuilding the binary to a new
/// path leaves a Run entry aimed at a file that no longer exists — autostart
/// silently stops working and the checkbox still says it is on, which is the
/// worst of both.
pub fn repair_if_stale() {
    let Some(registered) = current_value() else {
        return;
    };
    let Ok(want) = command() else {
        return;
    };
    if !registered.eq_ignore_ascii_case(&want) {
        let _ = set_enabled(true);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_is_quoted_and_absolute() {
        let cmd = command().expect("current_exe should resolve under test");
        assert!(cmd.starts_with('"') && cmd.ends_with('"'), "got {cmd}");
        let inner = cmd.trim_matches('"');
        assert!(
            std::path::Path::new(inner).is_absolute(),
            "path must be absolute so the shell does not resolve it against CWD: {inner}"
        );
    }

    #[test]
    fn toggling_round_trips() {
        // Records the real state first and restores it, so running the suite
        // never changes whether the user's machine autostarts this app.
        let before = is_enabled();

        set_enabled(true).expect("enable");
        assert!(is_enabled(), "should read back as enabled");
        assert_eq!(
            current_value().as_deref(),
            Some(command().unwrap().as_str()),
            "enabling must point at the current executable"
        );

        set_enabled(false).expect("disable");
        assert!(!is_enabled(), "should read back as disabled");

        // Disabling twice is not an error.
        set_enabled(false).expect("disabling an absent value is a no-op");

        set_enabled(before).expect("restore original state");
        assert_eq!(is_enabled(), before);
    }
}
