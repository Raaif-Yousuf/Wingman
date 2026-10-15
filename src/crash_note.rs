//! Issue #227: a crash note, so a panic is never a silent disappearance.
//!
//! The release profile keeps `panic = "abort"`, so a panic on any thread
//! ends the process with no chance to show a card. A panic hook, installed
//! first thing in `main`, runs before the abort and writes a short note to
//! `%LOCALAPPDATA%\Wingman\crash-note.txt`. The next start reads it once,
//! deletes it and shows a card saying Wingman closed unexpectedly, with the
//! place in the code and the panic message ([`take_note`], [`card_text`]).
//!
//! # What the note may hold
//!
//! Version, thread name, a unix timestamp, the source location and the
//! panic message. Never a screenshot, OCR text, a key or the clipboard: the
//! hook reads nothing of the sort. The one place user data could sneak in
//! is the panic message itself (`unwrap()` on an `Err` formats the error,
//! and an error can quote a response). So [`sanitize`] runs the message
//! through the key-shaped-token redaction, strips the account name out of
//! paths, folds it onto one line and cuts it to [`MAX_MESSAGE_CHARS`].
//!
//! # Debug builds
//!
//! With `panic = "unwind"` (dev and test builds) the process survives a
//! worker panic, so the hook is not installed there and no note is written:
//! a note would claim a crash that did not happen. `app::catch_worker` turns
//! such a panic into an ordinary error card instead.
//!
//! Nothing here polls or runs while idle: the hook only runs when a panic
//! does, and the note is read once at startup.

use std::path::{Path, PathBuf};

/// Longest panic message kept, in characters.
pub const MAX_MESSAGE_CHARS: usize = 160;

/// Longest location kept, in characters.
const MAX_LOCATION_CHARS: usize = 120;

/// Longest thread name kept, in characters.
const MAX_THREAD_CHARS: usize = 40;

/// What the hook knows about a panic, before sanitising.
pub struct PanicFacts<'a> {
    pub thread: &'a str,
    pub location: &'a str,
    pub message: &'a str,
    pub version: &'a str,
    pub unix_secs: u64,
}

/// `%LOCALAPPDATA%\Wingman\crash-note.txt`. Machine-scoped like the egress
/// log. AGENTS.md rule 9: under `#[cfg(test)]` this is a process-scoped file
/// in `%TEMP%`, never the real path.
pub fn note_path() -> anyhow::Result<PathBuf> {
    #[cfg(test)]
    {
        Ok(std::env::temp_dir().join(format!(
            "wingman-test-crash-note-{}.txt",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    {
        Ok(crate::known_folder::local_app_data()?
            .join("Wingman")
            .join("crash-note.txt"))
    }
}

/// Folds `text` onto one line, redacts key-shaped tokens and the account
/// name, and cuts it to `max` characters (with an ellipsis when cut). Pure.
pub fn sanitize(text: &str, profile: &str, max: usize) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let redacted = crate::egress::redact_opaque_tokens(&one_line);
    let redacted = crate::diagnostics::redact_user_profile(&redacted, profile);
    let (cut, was_cut) = crate::provider::truncate_chars(&redacted, max);
    if was_cut {
        format!("{cut}...")
    } else {
        cut
    }
}

/// The note's text: one `key: value` line each, every value sanitised.
pub fn format_note(facts: &PanicFacts, profile: &str) -> String {
    format!(
        "version: {}\nthread: {}\ntime_unix: {}\nlocation: {}\nmessage: {}\n",
        sanitize(facts.version, profile, 20),
        sanitize(facts.thread, profile, MAX_THREAD_CHARS),
        facts.unix_secs,
        sanitize(facts.location, profile, MAX_LOCATION_CHARS),
        sanitize(facts.message, profile, MAX_MESSAGE_CHARS),
    )
}

/// The value after `key: ` in a note, if that line exists.
fn field<'a>(note: &'a str, key: &str) -> Option<&'a str> {
    let prefix = format!("{key}: ");
    note.lines()
        .find_map(|l| l.trim_end_matches('\r').strip_prefix(prefix.as_str()))
}

/// Headline and detail for the "closed unexpectedly" card. Pure. No em
/// dashes (AGENTS.md rule 11).
pub fn card_text(note: &str) -> (String, String) {
    let location = field(note, "location").unwrap_or("unknown");
    let message = field(note, "message").unwrap_or("");
    let mut detail = format!("Wingman stopped because of an internal error in {location}.");
    if !message.is_empty() {
        detail.push_str(&format!("\n\nError: {message}"));
    }
    detail.push_str(
        "\n\nUse Copy details or Copy diagnostics from the tray menu to report it. \
         This message shows once.",
    );
    ("Wingman closed unexpectedly last time".to_string(), detail)
}

/// Writes `note` to `path`, creating the folder. Best effort for the hook.
pub fn write_note(path: &Path, note: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, note)
}

/// Reads the note at `path` and deletes it, so the card shows once. `None`
/// when there is no note, it is empty, or it could not be deleted (a note
/// that cannot be removed would otherwise show on every start).
pub fn take_note(path: &Path) -> Option<String> {
    let note = std::fs::read_to_string(path).ok()?;
    std::fs::remove_file(path).ok()?;
    if note.trim().is_empty() {
        return None;
    }
    Some(note)
}

/// Installs the panic hook. Call first thing in `main`. A no-op unless the
/// build aborts on panic (see the module docs).
pub fn install_hook() {
    if !cfg!(panic = "abort") {
        return;
    }
    std::panic::set_hook(Box::new(|info| {
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown".to_string());
        let payload = info.payload();
        let message = payload
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_default();
        let thread = std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string();
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let profile = crate::known_folder::user_profile()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let note = format_note(
            &PanicFacts {
                thread: &thread,
                location: &location,
                message: &message,
                version: env!("CARGO_PKG_VERSION"),
                unix_secs,
            },
            &profile,
        );
        if let Ok(path) = note_path() {
            let _ = write_note(&path, &note);
        }
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts<'a>(message: &'a str, location: &'a str) -> PanicFacts<'a> {
        PanicFacts {
            thread: "worker",
            location,
            message,
            version: "1.2.3",
            unix_secs: 1_760_000_000,
        }
    }

    #[test]
    fn note_has_every_field() {
        let note = format_note(&facts("boom", "src\\app.rs:10:5"), "");
        assert_eq!(
            note,
            "version: 1.2.3\nthread: worker\ntime_unix: 1760000000\n\
             location: src\\app.rs:10:5\nmessage: boom\n"
        );
    }

    #[test]
    fn message_is_cut_and_folded_onto_one_line() {
        let long = format!("first\nsecond {}", "x ".repeat(200));
        let out = sanitize(&long, "", MAX_MESSAGE_CHARS);
        assert!(!out.contains('\n'));
        assert!(out.chars().count() <= MAX_MESSAGE_CHARS + 3);
        assert!(out.ends_with("..."));
        assert!(out.starts_with("first second"));
    }

    #[test]
    fn short_message_is_untouched() {
        assert_eq!(
            sanitize("index out of bounds", "", 160),
            "index out of bounds"
        );
    }

    #[test]
    fn key_shaped_tokens_are_redacted() {
        let key = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789";
        let out = sanitize(&format!("called unwrap on Err: bad key {key}"), "", 160);
        assert!(!out.contains("AbCdEfGhIjKl"), "{out}");
        assert!(!out.contains("0123456789"), "{out}");
    }

    #[test]
    fn account_name_is_removed_from_paths() {
        let out = sanitize(
            "C:\\Users\\jdoe\\.cargo\\registry\\src\\x.rs:1:1",
            "C:\\Users\\jdoe",
            160,
        );
        assert!(!out.contains("jdoe"), "{out}");
        assert!(out.contains("%USERPROFILE%"), "{out}");
    }

    #[test]
    fn a_note_with_hostile_values_stays_one_line_per_field() {
        let note = format_note(&facts("a\nversion: fake\nb", "loc\r\nx"), "");
        assert_eq!(note.lines().count(), 5);
    }

    #[test]
    fn card_text_names_the_location_and_message() {
        let note = format_note(&facts("boom", "src\\app.rs:10:5"), "");
        let (headline, detail) = card_text(&note);
        assert_eq!(headline, "Wingman closed unexpectedly last time");
        assert!(detail.contains("src\\app.rs:10:5"));
        assert!(detail.contains("Error: boom"));
        assert!(detail.contains("Copy diagnostics"));
        assert!(!detail.contains('\u{2014}'));
    }

    #[test]
    fn card_text_survives_a_damaged_note() {
        let (_, detail) = card_text("garbage");
        assert!(detail.contains("unknown"));
        assert!(!detail.contains("Error:"));
    }

    #[test]
    fn card_text_reads_crlf_notes() {
        let note = format_note(&facts("boom", "a.rs:1:1"), "").replace('\n', "\r\n");
        let (_, detail) = card_text(&note);
        assert!(detail.contains("a.rs:1:1."), "{detail}");
        assert!(detail.contains("Error: boom\n"), "{detail}");
    }

    fn temp_note(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("wingman-test-crash-{name}-{}", std::process::id()))
    }

    #[test]
    fn a_note_is_read_once_then_gone() {
        let path = temp_note("once");
        let _ = std::fs::remove_file(&path);
        write_note(&path, "version: 1\nlocation: a.rs:1:1\n").unwrap();
        assert_eq!(
            take_note(&path).as_deref(),
            Some("version: 1\nlocation: a.rs:1:1\n")
        );
        assert!(!path.exists());
        assert_eq!(take_note(&path), None);
    }

    #[test]
    fn no_note_means_no_card() {
        assert_eq!(take_note(&temp_note("missing")), None);
    }

    #[test]
    fn an_empty_note_is_cleared_and_ignored() {
        let path = temp_note("empty");
        write_note(&path, "  \n").unwrap();
        assert_eq!(take_note(&path), None);
        assert!(!path.exists());
    }

    #[test]
    fn write_note_creates_the_folder() {
        let dir = temp_note("dir");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("note.txt");
        write_note(&path, "x").unwrap();
        assert!(path.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_builds_never_use_the_production_path() {
        let p = note_path().unwrap();
        assert!(p.starts_with(std::env::temp_dir()));
        assert!(p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("wingman-test-"));
    }
}
