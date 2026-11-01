//! Dev instance (#1271): a debug-only switch for running a second Wingman
//! next to an installed one without touching it.
//!
//! Set `WINGMAN_DEV_INSTANCE=<name>` on a debug build that runs in portable
//! mode (`wingman.portable` next to the exe). Then:
//!
//! - the single-instance mutex and the owner window class get a `.Dev.<name>`
//!   suffix, so the installed instance is not found, poked or exited on;
//! - the low-level keyboard hook is not installed, so the Copilot key keeps
//!   going to the installed instance (use the tray menu to ask).
//!
//! Release builds ignore the variable: [`active`] is always `None` there, so
//! the shipped names and the hook never change.
//!
//! Why portable is required: without it the dev instance would read and write
//! the installed instance's `%APPDATA%` files. A name given without portable
//! mode is ignored, with a warning.
//!
//! Nothing else needs a suffix. The tray icon is keyed by (window, id), and
//! the dev instance has its own window. The card, palette, Settings and
//! overlay window classes are never looked up by name from another process.
//! Autostart is a no-op in portable mode. The one shared thing is Credential
//! Manager (API keys): a key saved in a dev instance is saved for both.

/// The environment variable that turns the switch on.
pub const ENV_VAR: &str = "WINGMAN_DEV_INSTANCE";

/// Longest accepted name. Short on purpose: it ends up in kernel object names.
const MAX_NAME_LEN: usize = 16;

/// Whether `name` is usable: 1 to 16 characters, ASCII letters, digits or `-`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The suffix to use for a raw environment value, or why it is ignored.
/// `Ok(None)` is the normal case (unset or empty). `portable` is whether the
/// process runs in portable mode. Pure.
pub fn parse(raw: Option<&str>, portable: bool) -> Result<Option<String>, String> {
    let Some(raw) = raw.map(str::trim).filter(|r| !r.is_empty()) else {
        return Ok(None);
    };
    if !valid_name(raw) {
        return Err(format!(
            "{ENV_VAR} ignored: use 1 to {MAX_NAME_LEN} letters, digits or dashes"
        ));
    }
    if !portable {
        return Err(format!(
            "{ENV_VAR} ignored: it needs portable mode (a wingman.portable file next to the exe)"
        ));
    }
    Ok(Some(raw.to_string()))
}

/// `base` with the dev suffix, or `base` unchanged for a normal run.
pub fn with_suffix(base: &str, suffix: Option<&str>) -> String {
    match suffix {
        Some(s) => format!("{base}.Dev.{s}"),
        None => base.to_string(),
    }
}

/// Whether the low-level keyboard hook is installed: not in a dev instance,
/// where it would swallow the Copilot key from the installed one.
pub fn install_hook(suffix: Option<&str>) -> bool {
    suffix.is_none()
}

/// The dev name for this process, decided once. Always `None` in release.
pub fn active() -> Option<&'static str> {
    #[cfg(debug_assertions)]
    {
        use std::sync::OnceLock;
        static ACTIVE: OnceLock<Option<String>> = OnceLock::new();
        ACTIVE
            .get_or_init(|| {
                let raw = std::env::var(ENV_VAR).ok();
                match parse(raw.as_deref(), crate::portable::is_portable()) {
                    Ok(name) => name,
                    Err(why) => {
                        eprintln!("{why}");
                        crate::diagnostics::note_warning(&why);
                        None
                    }
                }
            })
            .as_deref()
    }
    #[cfg(not(debug_assertions))]
    {
        None
    }
}

/// NUL-terminated UTF-16 of `base` plus this process's dev suffix.
pub fn wide_name(base: &str) -> Vec<u16> {
    wide(&with_suffix(base, active()))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Whether this process should install the keyboard hook.
pub fn hook_enabled() -> bool {
    install_hook(active())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_or_empty_is_a_normal_run() {
        assert_eq!(parse(None, true), Ok(None));
        assert_eq!(parse(Some(""), true), Ok(None));
        assert_eq!(parse(Some("  "), false), Ok(None));
    }

    #[test]
    fn a_valid_name_in_portable_mode_is_used() {
        assert_eq!(parse(Some("dev1"), true), Ok(Some("dev1".to_string())));
        assert_eq!(parse(Some(" a-B-9 "), true), Ok(Some("a-B-9".to_string())));
    }

    #[test]
    fn a_bad_name_is_ignored_with_a_reason() {
        for bad in [
            "a b",
            "a.b",
            "a\\b",
            "caf\u{e9}",
            "x/y",
            "this-name-is-way-too-long",
        ] {
            assert!(parse(Some(bad), true).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_name_length_limit_is_exact() {
        assert!(valid_name(&"a".repeat(16)));
        assert!(!valid_name(&"a".repeat(17)));
        assert!(!valid_name(""));
    }

    #[test]
    fn without_portable_mode_the_name_is_ignored() {
        assert!(parse(Some("dev1"), false).is_err());
    }

    #[test]
    fn the_suffix_changes_the_name_and_only_for_a_dev_run() {
        let base = "Local\\Test.Base";
        assert_eq!(with_suffix(base, None), base);
        assert_eq!(with_suffix(base, Some("dev1")), "Local\\Test.Base.Dev.dev1");
        assert_ne!(with_suffix(base, Some("a")), with_suffix(base, Some("b")));
    }

    #[test]
    fn the_hook_is_skipped_only_in_a_dev_instance() {
        assert!(install_hook(None));
        assert!(!install_hook(Some("dev1")));
    }

    #[test]
    fn the_test_binary_is_not_a_dev_instance() {
        // The test binary is not portable, so even a stray variable is ignored.
        assert_eq!(active(), None);
        assert!(hook_enabled());
    }

    #[test]
    fn wide_names_are_nul_terminated() {
        let w = wide("ab");
        assert_eq!(w, vec![b'a' as u16, b'b' as u16, 0]);
    }
}
