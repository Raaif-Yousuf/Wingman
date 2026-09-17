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
    HKEY_CURRENT_USER, KEY_READ, KEY_WRITE, REG_SAM_FLAGS, REG_SZ,
};

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// The Run value name. Also what shows up in Task Manager's Startup tab.
const VALUE_NAME: &str = "Wingman";

/// The Run value name pre-rename builds registered. Only
/// [`remove_old_run_value`] reads this, once, to delete the stale entry; it
/// is never written again.
const OLD_VALUE_NAME: &str = "copilot-ask";

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open(access: REG_SAM_FLAGS) -> Result<HKEY> {
    open_at(RUN_KEY, access)
}

/// [`open`], with the subkey path injected so it can be pointed at a
/// scratch key in tests instead of the real Run key (Hard Rule 9).
fn open_at(key_path: &str, access: REG_SAM_FLAGS) -> Result<HKEY> {
    let mut key = HKEY::default();
    let sub = wide(key_path);
    unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            Some(0),
            access,
            &mut key,
        )
        .ok()
        .context("opening the registry key")?;
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
    value_at(RUN_KEY, VALUE_NAME)
}

/// [`current_value`], with both the key path and value name injected so
/// tests can read back a scratch key instead of the real Run key.
fn value_at(key_path: &str, value_name: &str) -> Option<String> {
    let key = open_at(key_path, KEY_READ).ok()?;
    let name = wide(value_name);
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

/// Whether Wingman is registered to start with Windows.
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

/// Removes the pre-rename `copilot-ask` Run value, if a pre-rename install
/// left one behind. Autostart's on/off state is carried forward first: if
/// the old value was present and `Wingman` is not already registered, the
/// new value is written before the old one is deleted, so upgrading in
/// place does not silently turn autostart off. No-op (not an error) if the
/// old value is already absent, so this is safe to call unconditionally on
/// every startup rather than gating it on whether config migration also ran.
pub fn remove_old_run_value() -> Result<()> {
    let cmd = command()?;
    migrate_value_at(RUN_KEY, OLD_VALUE_NAME, VALUE_NAME, &cmd)
}

/// Carries `old_name`'s presence forward to `new_name` (writing `new_data`
/// as its value) if `new_name` is not already set, then deletes `old_name`.
/// `key_path` is injected so tests exercise this against a scratch key
/// instead of the real Run key (Hard Rule 9); [`remove_old_run_value`] is
/// the only caller that points it at `RUN_KEY`.
fn migrate_value_at(key_path: &str, old_name: &str, new_name: &str, new_data: &str) -> Result<()> {
    if value_at(key_path, old_name).is_some() && value_at(key_path, new_name).is_none() {
        write_value_at(key_path, new_name, new_data)?;
    }
    delete_value_at(key_path, old_name)
}

/// Writes `value` as a REG_SZ under `value_name` at `key_path`.
fn write_value_at(key_path: &str, value_name: &str, value: &str) -> Result<()> {
    let key = open_at(key_path, KEY_WRITE)?;
    let name = wide(value_name);
    let data = wide(value);
    let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
    let result = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }
        .ok()
        .context("writing a Run value");
    unsafe { let _ = RegCloseKey(key); }
    result
}

/// Deletes `value_name` from `key_path` under `HKEY_CURRENT_USER`. Already
/// absent is treated as success, matching [`set_enabled`]'s own delete arm.
fn delete_value_at(key_path: &str, value_name: &str) -> Result<()> {
    let key = open_at(key_path, KEY_WRITE)?;
    let name = wide(value_name);
    let deleted = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())) };
    unsafe { let _ = RegCloseKey(key); }
    if deleted == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        deleted.ok().context("deleting a Run value")
    }
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

    // -- old Run value removal ------------------------------------------------
    //
    // These exercise `delete_value_at` / `value_at` against a key this test
    // creates and destroys itself -- never `RUN_KEY` -- so the suite can
    // never touch the user's real startup entries (Hard Rule 9).

    /// A scratch subkey unique to this test process/run.
    fn test_key_path() -> String {
        format!(
            "Software\\WingmanAutostartTest\\{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    fn create_test_key(path: &str) -> HKEY {
        use windows::Win32::System::Registry::{RegCreateKeyExW, KEY_ALL_ACCESS, REG_OPTION_VOLATILE};
        let sub = wide(path);
        let mut key = HKEY::default();
        unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                None,
                REG_OPTION_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut key,
                None,
            )
        }
        .ok()
        .expect("creating the scratch test key");
        key
    }

    fn write_test_value(key: HKEY, value_name: &str, value: &str) {
        let name = wide(value_name);
        let data = wide(value);
        let bytes = unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
        unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)) }
            .ok()
            .expect("writing the scratch test value");
    }

    fn delete_test_key(path: &str) {
        use windows::Win32::System::Registry::RegDeleteTreeW;
        let sub = wide(path);
        unsafe {
            let _ = RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()));
        }
    }

    #[test]
    fn delete_value_at_removes_only_the_named_value() {
        let path = test_key_path();
        let key = create_test_key(&path);
        write_test_value(key, OLD_VALUE_NAME, "leftover pre-rename entry");
        write_test_value(key, VALUE_NAME, "current entry, must survive");
        unsafe { let _ = RegCloseKey(key); }

        delete_value_at(&path, OLD_VALUE_NAME).expect("deleting the old value should succeed");

        assert!(
            value_at(&path, OLD_VALUE_NAME).is_none(),
            "the old value must be gone"
        );
        assert_eq!(
            value_at(&path, VALUE_NAME).as_deref(),
            Some("current entry, must survive"),
            "an unrelated value under the same key must be untouched"
        );

        delete_test_key(&path);
    }

    #[test]
    fn delete_value_at_on_an_already_clean_key_is_not_an_error() {
        let path = test_key_path();
        let key = create_test_key(&path);
        write_test_value(key, VALUE_NAME, "only the new value exists");
        unsafe { let _ = RegCloseKey(key); }

        // The old value was never written here -- this is the steady state
        // for anyone who installs Wingman fresh, never having run
        // copilot-ask. Deleting an absent value must not surface as a
        // startup error.
        delete_value_at(&path, OLD_VALUE_NAME).expect("absent old value must be a no-op, not an error");

        assert_eq!(value_at(&path, VALUE_NAME).as_deref(), Some("only the new value exists"));

        delete_test_key(&path);
    }

    // -- carrying autostart forward across the rename ------------------------

    #[test]
    fn migrate_value_at_writes_the_new_value_and_deletes_the_old_one() {
        // The shape a real upgrade hits: autostart was on under the old
        // name, and nothing has ever been written under the new name yet.
        let path = test_key_path();
        let key = create_test_key(&path);
        write_test_value(key, OLD_VALUE_NAME, "\"C:\\old\\copilot-ask.exe\"");
        unsafe { let _ = RegCloseKey(key); }

        migrate_value_at(&path, OLD_VALUE_NAME, VALUE_NAME, "\"C:\\new\\wingman.exe\"")
            .expect("migrating the value should succeed");

        assert!(
            value_at(&path, OLD_VALUE_NAME).is_none(),
            "the old value must be gone, or a duplicate Run entry would launch twice"
        );
        assert_eq!(
            value_at(&path, VALUE_NAME).as_deref(),
            Some("\"C:\\new\\wingman.exe\""),
            "autostart must carry forward under the new name, or upgrading in place silently turns it off"
        );

        delete_test_key(&path);
    }

    #[test]
    fn migrate_value_at_never_overwrites_an_existing_new_value() {
        // The new value was already set some other way (e.g. a fresh
        // install.ps1 run) before this ever ran; it must win.
        let path = test_key_path();
        let key = create_test_key(&path);
        write_test_value(key, OLD_VALUE_NAME, "\"C:\\old\\copilot-ask.exe\"");
        write_test_value(key, VALUE_NAME, "\"C:\\already\\set\\wingman.exe\"");
        unsafe { let _ = RegCloseKey(key); }

        migrate_value_at(&path, OLD_VALUE_NAME, VALUE_NAME, "\"C:\\would-be\\overwrite.exe\"")
            .expect("migrating the value should succeed");

        assert_eq!(
            value_at(&path, VALUE_NAME).as_deref(),
            Some("\"C:\\already\\set\\wingman.exe\""),
            "an already-registered new value must never be clobbered"
        );
        assert!(value_at(&path, OLD_VALUE_NAME).is_none(), "the old value is still cleaned up");

        delete_test_key(&path);
    }

    #[test]
    fn migrate_value_at_with_no_old_value_leaves_the_new_one_untouched() {
        // Autostart was never on -- the common case for anyone who never
        // enabled "start with Windows" under copilot-ask.
        let path = test_key_path();
        let key = create_test_key(&path);
        write_test_value(key, VALUE_NAME, "\"C:\\new\\wingman.exe\"");
        unsafe { let _ = RegCloseKey(key); }

        migrate_value_at(&path, OLD_VALUE_NAME, VALUE_NAME, "\"C:\\should-not-be-written.exe\"")
            .expect("migrating with no old value should still succeed");

        assert_eq!(value_at(&path, VALUE_NAME).as_deref(), Some("\"C:\\new\\wingman.exe\""));

        delete_test_key(&path);
    }

    #[test]
    fn migrate_value_at_with_neither_value_present_is_a_no_op() {
        let path = test_key_path();
        let key = create_test_key(&path);
        unsafe { let _ = RegCloseKey(key); }

        migrate_value_at(&path, OLD_VALUE_NAME, VALUE_NAME, "\"C:\\unused.exe\"")
            .expect("migrating an empty key should still succeed");

        assert!(value_at(&path, VALUE_NAME).is_none());
        assert!(value_at(&path, OLD_VALUE_NAME).is_none());

        delete_test_key(&path);
    }
}
