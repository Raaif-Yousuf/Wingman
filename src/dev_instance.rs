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
#[cfg(any(debug_assertions, test))]
pub const ENV_VAR: &str = "WINGMAN_DEV_INSTANCE";

/// Longest accepted name. Short on purpose: it ends up in kernel object names.
#[cfg(any(debug_assertions, test))]
const MAX_NAME_LEN: usize = 16;

/// Whether `name` is usable: 1 to 16 characters, ASCII letters, digits or `-`.
#[cfg(any(debug_assertions, test))]
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// The suffix to use for a raw environment value, or why it is ignored.
/// `Ok(None)` is the normal case (unset or empty). `portable` is whether the
/// process runs in portable mode. Pure.
#[cfg(any(debug_assertions, test))]
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

/// What the process does with the raw variable: the name to use and the
/// warning to record. Pure, so the ignore paths are tested directly.
#[cfg(any(debug_assertions, test))]
pub fn resolve(raw: Option<&str>, portable: bool) -> (Option<String>, Option<String>) {
    match parse(raw, portable) {
        Ok(name) => (name, None),
        Err(why) => (None, Some(why)),
    }
}

/// The tray tooltip for a dev instance: `text` plus ` (dev: <name>)`, so two
/// Wingman icons can be told apart. The tag is kept whole when `text` is too
/// long for the 127 units a tooltip holds; unchanged for a normal run. Pure.
pub fn tooltip(text: &str, suffix: Option<&str>) -> String {
    let Some(name) = suffix else {
        return text.to_string();
    };
    let tag = format!(" (dev: {name})");
    let room = 127usize.saturating_sub(tag.encode_utf16().count());
    let mut base = String::new();
    let mut used = 0;
    for c in text.chars() {
        used += c.len_utf16();
        if used > room {
            break;
        }
        base.push(c);
    }
    base + &tag
}

/// [`tooltip`] for this process.
pub fn tray_tooltip(text: &str) -> String {
    tooltip(text, active())
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
                let (name, warning) = resolve(raw.as_deref(), crate::portable::is_portable());
                if let Some(why) = warning {
                    crate::diagnostics::note_warning(&why);
                }
                name
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
    fn resolve_ignores_a_name_without_portable_mode_and_says_why() {
        let (name, warning) = resolve(Some("dev1"), false);
        assert_eq!(name, None);
        assert!(warning.unwrap().contains("portable"));
    }

    #[test]
    fn resolve_ignores_a_bad_name_and_says_why() {
        let (name, warning) = resolve(Some("bad name"), true);
        assert_eq!(name, None);
        assert!(warning.unwrap().contains("letters, digits or dashes"));
    }

    #[test]
    fn resolve_uses_a_good_name_with_no_warning() {
        assert_eq!(resolve(Some("dev1"), true), (Some("dev1".into()), None));
        assert_eq!(resolve(None, false), (None, None));
    }

    #[test]
    fn the_tooltip_names_the_dev_instance_only_when_there_is_one() {
        assert_eq!(tooltip("Wingman: x", None), "Wingman: x");
        assert_eq!(
            tooltip("Wingman: x", Some("dev1")),
            "Wingman: x (dev: dev1)"
        );
    }

    #[test]
    fn a_long_tooltip_keeps_the_dev_tag_within_the_limit() {
        let long = "w".repeat(300);
        let t = tooltip(&long, Some("dev1"));
        assert!(t.ends_with(" (dev: dev1)"));
        assert_eq!(t.encode_utf16().count(), 127);
        let astral = "\u{1F600}".repeat(100);
        assert!(tooltip(&astral, Some("d")).encode_utf16().count() <= 127);
    }
    #[test]
    fn wide_names_are_nul_terminated() {
        let w = wide("ab");
        assert_eq!(w, vec![b'a' as u16, b'b' as u16, 0]);
    }
}
